//! In-memory HNSW graph (Malkov & Yashunin) over `(EntityId, vector)` pairs.
//!
//! The graph is a cache built from the persisted vectors; `VectorIndex` rebuilds it when the stored
//! version changes. Levels come from a hash of the entity id, so builds are deterministic.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

use dtlv_codec::EntityId;

use crate::{compute_distance, Metric};

const M: usize = 16;
const M_MAX0: usize = 2 * M;
const EF_CONSTRUCTION: usize = 200;

/// Total-order wrapper so distances can live in heaps.
#[derive(Clone, Copy, PartialEq)]
struct Dist(f32);

impl Eq for Dist {}

impl PartialOrd for Dist {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Dist {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.partial_cmp(&other.0).unwrap_or(Ordering::Equal)
    }
}

pub struct Hnsw {
    metric: Metric,
    ids: Vec<EntityId>,
    vecs: Vec<Vec<f32>>,
    /// `links[node][layer]` lists neighbor node indices.
    links: Vec<Vec<Vec<u32>>>,
    entry: Option<u32>,
    max_level: usize,
    /// `alive[node]` is false once the node has been removed or replaced (tombstone).
    alive: Vec<bool>,
    /// The live node for each entity.
    slot: HashMap<EntityId, u32>,
    dead: usize,
}

/// splitmix64 hash, mapped to a level with the usual `floor(-ln(u) / ln(M))` distribution.
fn level_for(eid: EntityId) -> usize {
    let mut z = eid.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let u = ((z >> 11) as f64 + 1.0) / ((1u64 << 53) as f64 + 1.0);
    let ml = 1.0 / (M as f64).ln();
    (-u.ln() * ml).floor() as usize
}

impl Hnsw {
    /// Builds the graph, inserting entities in id order so the result does not depend on storage order.
    pub fn build(metric: Metric, mut items: Vec<(EntityId, Vec<f32>)>) -> Self {
        items.sort_by_key(|(eid, _)| *eid);
        let mut g = Hnsw {
            metric,
            ids: Vec::new(),
            vecs: Vec::new(),
            links: Vec::new(),
            entry: None,
            max_level: 0,
            alive: Vec::new(),
            slot: HashMap::new(),
            dead: 0,
        };
        for (eid, vec) in items {
            g.upsert(eid, vec);
        }
        g
    }

    /// Inserts or replaces the vector of `eid`. A replaced node stays in the graph as a tombstone.
    pub fn upsert(&mut self, eid: EntityId, vec: Vec<f32>) {
        if let Some(old) = self.slot.get(&eid).copied() {
            self.alive[old as usize] = false;
            self.dead += 1;
        }
        let node = self.ids.len() as u32;
        self.insert(eid, vec);
        self.alive.push(true);
        self.slot.insert(eid, node);
    }

    /// Tombstones the vector of `eid`, if it has one.
    pub fn remove(&mut self, eid: EntityId) {
        if let Some(node) = self.slot.remove(&eid) {
            self.alive[node as usize] = false;
            self.dead += 1;
        }
    }

    pub fn live_len(&self) -> usize {
        self.slot.len()
    }

    /// Tombstones pile up until a rebuild is cheaper than searching past them.
    pub fn needs_compaction(&self) -> bool {
        self.dead > 1000 && self.dead > self.slot.len()
    }

    fn dist(&self, q: &[f32], node: u32) -> f32 {
        compute_distance(self.metric, q, &self.vecs[node as usize])
    }

    fn insert(&mut self, eid: EntityId, vec: Vec<f32>) {
        let level = level_for(eid);
        let node = self.ids.len() as u32;
        self.ids.push(eid);
        self.vecs.push(vec);
        self.links.push(vec![Vec::new(); level + 1]);

        let Some(mut ep) = self.entry else {
            self.entry = Some(node);
            self.max_level = level;
            return;
        };
        let q = self.vecs[node as usize].clone();
        for lc in (level + 1..=self.max_level).rev() {
            ep = self.greedy(&q, ep, lc);
        }
        for lc in (0..=level.min(self.max_level)).rev() {
            let cands = self.search_layer(&q, &[ep], EF_CONSTRUCTION, lc);
            let m_max = if lc == 0 { M_MAX0 } else { M };
            let chosen: Vec<u32> = cands.iter().take(M).map(|&(_, n)| n).collect();
            self.links[node as usize][lc] = chosen.clone();
            for &nb in &chosen {
                self.links[nb as usize][lc].push(node);
                if self.links[nb as usize][lc].len() > m_max {
                    self.prune(nb, lc, m_max);
                }
            }
            if let Some(&(_, best)) = cands.first() {
                ep = best;
            }
        }
        if level > self.max_level {
            self.max_level = level;
            self.entry = Some(node);
        }
    }

    /// Keeps the `m_max` neighbors of `node` closest to it.
    fn prune(&mut self, node: u32, lc: usize, m_max: usize) {
        let base = self.vecs[node as usize].clone();
        let mut scored: Vec<(Dist, u32)> = self.links[node as usize][lc]
            .iter()
            .map(|&n| (Dist(compute_distance(self.metric, &base, &self.vecs[n as usize])), n))
            .collect();
        scored.sort();
        scored.truncate(m_max);
        self.links[node as usize][lc] = scored.into_iter().map(|(_, n)| n).collect();
    }

    fn greedy(&self, q: &[f32], mut ep: u32, lc: usize) -> u32 {
        let mut best = self.dist(q, ep);
        loop {
            let mut moved = false;
            for &nb in &self.links[ep as usize][lc] {
                let d = self.dist(q, nb);
                if d < best {
                    best = d;
                    ep = nb;
                    moved = true;
                }
            }
            if !moved {
                return ep;
            }
        }
    }

    /// Beam search on one layer. Returns `(distance, node)` sorted ascending, at most `ef` long.
    fn search_layer(&self, q: &[f32], entries: &[u32], ef: usize, lc: usize) -> Vec<(f32, u32)> {
        let mut visited: HashSet<u32> = entries.iter().copied().collect();
        // Min-heap of candidates to expand, max-heap of current best results.
        let mut candidates: BinaryHeap<std::cmp::Reverse<(Dist, u32)>> = BinaryHeap::new();
        let mut results: BinaryHeap<(Dist, u32)> = BinaryHeap::new();
        for &e in entries {
            let d = Dist(self.dist(q, e));
            candidates.push(std::cmp::Reverse((d, e)));
            results.push((d, e));
        }
        while let Some(std::cmp::Reverse((d, c))) = candidates.pop() {
            let worst = results.peek().map(|r| r.0).unwrap_or(d);
            if d > worst && results.len() >= ef {
                break;
            }
            for &nb in &self.links[c as usize][lc] {
                if !visited.insert(nb) {
                    continue;
                }
                let nd = Dist(self.dist(q, nb));
                let worst = results.peek().map(|r| r.0).unwrap_or(nd);
                if results.len() < ef || nd < worst {
                    candidates.push(std::cmp::Reverse((nd, nb)));
                    results.push((nd, nb));
                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }
        let mut out: Vec<(f32, u32)> = results.into_iter().map(|(d, n)| (d.0, n)).collect();
        out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal).then(a.1.cmp(&b.1)));
        out
    }

    /// Approximate top-`k` `(EntityId, distance)` sorted ascending.
    pub fn search(&self, q: &[f32], k: usize, ef: usize) -> Vec<(EntityId, f32)> {
        let Some(mut ep) = self.entry else {
            return Vec::new();
        };
        for lc in (1..=self.max_level).rev() {
            ep = self.greedy(q, ep, lc);
        }
        // Tombstones take slots in the beam, so widen it by the number of dead nodes.
        let ef = ef.max(k) + self.dead.min(2000);
        self.search_layer(q, &[ep], ef, 0)
            .into_iter()
            .filter(|&(_, n)| self.alive[n as usize])
            .take(k)
            .map(|(d, n)| (self.ids[n as usize], d))
            .collect()
    }
}
