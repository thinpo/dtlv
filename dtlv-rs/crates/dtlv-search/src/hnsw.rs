//! In-memory HNSW graph (Malkov & Yashunin) over `(EntityId, vector)` pairs.
//!
//! The graph is a cache built from the persisted vectors; `VectorIndex` rebuilds it when the stored
//! version changes. Levels come from a hash of the entity id, so builds are deterministic.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

use dtlv_codec::EntityId;

use crate::{compute_distance, Metric};

/// Leading byte of the serialized form; bump it when the layout changes.
const FORMAT_VERSION: u8 = 1;

const M: usize = 16;
const M_MAX0: usize = 2 * M;
const EF_CONSTRUCTION: usize = 100;

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

fn metric_tag(metric: Metric) -> u8 {
    match metric {
        Metric::Cosine => 0,
        Metric::Euclidean => 1,
        Metric::DotProduct => 2,
    }
}

fn metric_from_tag(tag: u8) -> Option<Metric> {
    match tag {
        0 => Some(Metric::Cosine),
        1 => Some(Metric::Euclidean),
        2 => Some(Metric::DotProduct),
        _ => None,
    }
}

/// Bounds-checked little-endian reader for `Hnsw::from_bytes`.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    /// `count` little-endian u32 values.
    fn u32s(&mut self, count: usize) -> Option<Vec<u32>> {
        let raw = self.take(count.checked_mul(4)?)?;
        Some(raw.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect())
    }
}

impl Hnsw {
    pub fn metric(&self) -> Metric {
        self.metric
    }

    /// Serializes the whole graph, tombstones included, so a reload searches exactly like the original.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![FORMAT_VERSION, metric_tag(self.metric)];
        out.extend_from_slice(&(self.ids.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.entry.unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(&(self.max_level as u32).to_le_bytes());
        for node in 0..self.ids.len() {
            out.extend_from_slice(&self.ids[node].to_le_bytes());
            out.push(self.alive[node] as u8);
            out.extend_from_slice(&(self.vecs[node].len() as u32).to_le_bytes());
            for f in &self.vecs[node] {
                out.extend_from_slice(&f.to_le_bytes());
            }
            out.extend_from_slice(&(self.links[node].len() as u32).to_le_bytes());
            for layer in &self.links[node] {
                out.extend_from_slice(&(layer.len() as u32).to_le_bytes());
                for nb in layer {
                    out.extend_from_slice(&nb.to_le_bytes());
                }
            }
        }
        out
    }

    /// Inverse of `to_bytes`. Returns `None` for any malformed input, including a graph whose links would
    /// index past a node's layers, so a corrupt cache is rebuilt instead of crashing a search.
    pub fn from_bytes(bytes: &[u8]) -> Option<Hnsw> {
        let mut r = Reader { bytes, pos: 0 };
        if r.u8()? != FORMAT_VERSION {
            return None;
        }
        let metric = metric_from_tag(r.u8()?)?;
        let n = r.u32()? as usize;
        let entry = r.u32()?;
        let max_level = r.u32()? as usize;
        let mut ids = Vec::new();
        let mut vecs = Vec::new();
        let mut links = Vec::new();
        let mut alive = Vec::new();
        for _ in 0..n {
            ids.push(r.u64()?);
            alive.push(match r.u8()? {
                0 => false,
                1 => true,
                _ => return None,
            });
            let dim = r.u32()? as usize;
            vecs.push(
                r.take(dim.checked_mul(4)?)?
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                    .collect::<Vec<f32>>(),
            );
            let layer_count = r.u32()? as usize;
            let mut node_links = Vec::new();
            for _ in 0..layer_count {
                let count = r.u32()? as usize;
                node_links.push(r.u32s(count)?);
            }
            links.push(node_links);
        }
        if r.pos != bytes.len() {
            return None;
        }
        let entry = match (n, entry) {
            (0, u32::MAX) => None,
            (_, e) if (e as usize) < n => Some(e),
            _ => return None,
        };
        if let Some(e) = entry {
            // Search walks layers 0..=max_level from the entry point.
            if links[e as usize].len() <= max_level {
                return None;
            }
        }
        for node_links in &links {
            for (lc, layer) in node_links.iter().enumerate() {
                for &nb in layer {
                    if nb as usize >= n || links[nb as usize].len() <= lc {
                        return None;
                    }
                }
            }
        }
        let mut slot = HashMap::new();
        for (node, &live) in alive.iter().enumerate() {
            if live {
                slot.insert(ids[node], node as u32);
            }
        }
        let dead = alive.iter().filter(|&&a| !a).count();
        Some(Hnsw { metric, ids, vecs, links, entry, max_level, alive, slot, dead })
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    struct Lcg(u64);

    impl Lcg {
        fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    #[test]
    fn bytes_round_trip_searches_identically_and_rejects_malformed_input() {
        let mut rng = Lcg(99);
        let items: Vec<(EntityId, Vec<f32>)> =
            (1..=200u64).map(|e| (e, (0..8).map(|_| rng.next_f32()).collect())).collect();
        let mut g = Hnsw::build(Metric::Euclidean, items);
        g.remove(7);
        g.upsert(3, (0..8).map(|_| rng.next_f32()).collect());
        let bytes = g.to_bytes();
        let back = Hnsw::from_bytes(&bytes).expect("round trip");
        assert_eq!(back.metric(), Metric::Euclidean);
        for _ in 0..10 {
            let q: Vec<f32> = (0..8).map(|_| rng.next_f32()).collect();
            assert_eq!(g.search(&q, 10, EF_SEARCH_TEST), back.search(&q, 10, EF_SEARCH_TEST));
        }
        for len in [0, 1, 2, 10, bytes.len() / 2, bytes.len() - 1] {
            assert!(Hnsw::from_bytes(&bytes[..len]).is_none(), "truncated at {len}");
        }
        assert!(Hnsw::from_bytes(&[]).is_none());
    }

    const EF_SEARCH_TEST: usize = 64;
}
