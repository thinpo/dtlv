pub mod fulltext;
pub mod hnsw;
pub mod stem;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use hnsw::Hnsw;

pub use fulltext::{analyze, tokenize, FtsConfig, FullTextIndex, Query};

use dtlv_codec::EntityId;
use dtlv_kv::{KvError, KvStore, RawDb, ReadTxn, WriteTxn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Cosine,
    Euclidean,
    DotProduct,
}

/// Indexes at or below this size are searched exactly; larger ones use the HNSW graph.
pub const BRUTE_FORCE_MAX: u64 = 512;
/// Beam width for graph search (raised to `k` when `k` is larger).
const EF_SEARCH: usize = 64;
/// How many versions of vector writes are kept for incremental graph updates.
const LOG_RETENTION: u64 = 4096;
const VERSION_KEY: &[u8] = b"version";
const GRAPH_KEY: &[u8] = b"graph";
const GRAPH_VERSION_KEY: &[u8] = b"graph_version";
const OP_UPSERT: u8 = 0;
const OP_REMOVE: u8 = 1;

/// Counters for how searches kept the graph current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VectorStats {
    /// Full graph builds.
    pub rebuilds: u64,
    /// Searches that brought the cached graph up to date by replaying logged writes.
    pub incremental_updates: u64,
    /// Graphs loaded from the copy saved by `save_graph`, instead of rebuilt.
    pub loads: u64,
    /// Graphs written to `meta` by `save_graph`.
    pub saves: u64,
}

/// LMDB-persisted vector index for entity embeddings (entity resolution & semantic lookup).
///
/// Vectors live in LMDB. Each write also appends an operation to a log keyed by the index version. A search
/// over more than `BRUTE_FORCE_MAX` vectors keeps a cached HNSW graph current by replaying the logged writes
/// after the graph's version, reading only from its own snapshot. It rebuilds from scratch when the log no
/// longer covers the gap or when too many nodes are tombstoned.
pub struct VectorIndex {
    vectors: RawDb,
    meta: RawDb,
    log: RawDb,
    metric: Metric,
    graph: Mutex<Option<(u64, Hnsw)>>,
    rebuilds: AtomicU64,
    incremental: AtomicU64,
    loads: AtomicU64,
    saves: AtomicU64,
    /// Set after a rebuild; the next write saves the graph so a restart can load it.
    dirty: AtomicBool,
}

impl VectorIndex {
    pub fn open(store: &KvStore, wtxn: &mut WriteTxn<'_>, name: &str, metric: Metric) -> Result<Self, KvError> {
        let vectors = store.open_db(wtxn, name)?;
        let meta = store.open_db(wtxn, &format!("{name}__meta"))?;
        let log = store.open_db(wtxn, &format!("{name}__log"))?;
        Ok(Self {
            vectors,
            meta,
            log,
            metric,
            graph: Mutex::new(None),
            rebuilds: AtomicU64::new(0),
            incremental: AtomicU64::new(0),
            loads: AtomicU64::new(0),
            saves: AtomicU64::new(0),
            dirty: AtomicBool::new(false),
        })
    }

    pub fn stats(&self) -> VectorStats {
        VectorStats {
            rebuilds: self.rebuilds.load(Ordering::Relaxed),
            incremental_updates: self.incremental.load(Ordering::Relaxed),
            loads: self.loads.load(Ordering::Relaxed),
            saves: self.saves.load(Ordering::Relaxed),
        }
    }

    /// Writes the cached graph and its version into `meta`, so a later process can skip the rebuild.
    /// Returns `false` and writes nothing when no graph is cached yet.
    pub fn save_graph(&self, wtxn: &mut WriteTxn<'_>) -> Result<bool, KvError> {
        let guard = self.graph.lock().unwrap_or_else(|e| e.into_inner());
        let Some((version, graph)) = guard.as_ref() else {
            return Ok(false);
        };
        self.meta.put(wtxn, GRAPH_KEY, &graph.to_bytes())?;
        self.meta.put(wtxn, GRAPH_VERSION_KEY, &version.to_be_bytes())?;
        self.saves.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }

    /// Saves the graph after a rebuild, on the first write that follows it.
    fn save_if_dirty(&self, wtxn: &mut WriteTxn<'_>) -> Result<(), KvError> {
        if self.dirty.load(Ordering::Relaxed) {
            self.save_graph(wtxn)?;
            // shortcut: dirty is cleared before commit; if this write txn aborts, the save is lost until the next rebuild.
            self.dirty.store(false, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Loads the graph saved by `save_graph` if it is intact, uses this index's metric, and is not newer than
    /// `version`. Catch-up in `search_vec` decides whether it is still usable.
    fn load_graph(&self, rtxn: &ReadTxn<'_>, version: u64) -> Result<Option<(u64, Hnsw)>, KvError> {
        let (Some(v), Some(bytes)) = (self.meta.get(rtxn, GRAPH_VERSION_KEY)?, self.meta.get(rtxn, GRAPH_KEY)?)
        else {
            return Ok(None);
        };
        if v.len() != 8 {
            return Ok(None);
        }
        let graph_version = u64::from_be_bytes(v.as_ref().try_into().unwrap());
        if graph_version > version {
            return Ok(None);
        }
        Ok(Hnsw::from_bytes(&bytes)
            .filter(|g| g.metric() == self.metric)
            .map(|g| (graph_version, g)))
    }

    /// Advances the version, records the operation under it, and drops entries past the retention window.
    fn record(&self, wtxn: &mut WriteTxn<'_>, op: &[u8]) -> Result<(), KvError> {
        let cur = match self.meta.get_rw(wtxn, VERSION_KEY)? {
            Some(b) if b.len() == 8 => u64::from_be_bytes(b.as_ref().try_into().unwrap()),
            _ => 0,
        };
        let next = cur + 1;
        self.meta.put(wtxn, VERSION_KEY, &next.to_be_bytes())?;
        self.log.put(wtxn, &next.to_be_bytes(), op)?;
        if next > LOG_RETENTION {
            self.log.delete(wtxn, &(next - LOG_RETENTION).to_be_bytes())?;
        }
        Ok(())
    }

    pub fn add_vec(&self, wtxn: &mut WriteTxn<'_>, eid: EntityId, vec: &[f32]) -> Result<(), KvError> {
        let mut bytes = Vec::with_capacity(vec.len() * 4);
        for &f in vec {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
        self.vectors.put(wtxn, &eid.to_be_bytes(), &bytes)?;
        let mut op = vec![OP_UPSERT];
        op.extend_from_slice(&eid.to_be_bytes());
        op.extend_from_slice(&bytes);
        self.record(wtxn, &op)?;
        self.save_if_dirty(wtxn)
    }

    /// Removes the vector of `eid`. Removing an absent entity changes nothing and does not advance the version.
    pub fn remove_vec(&self, wtxn: &mut WriteTxn<'_>, eid: EntityId) -> Result<(), KvError> {
        if !self.vectors.delete(wtxn, &eid.to_be_bytes())? {
            return Ok(());
        }
        let mut op = vec![OP_REMOVE];
        op.extend_from_slice(&eid.to_be_bytes());
        self.record(wtxn, &op)?;
        self.save_if_dirty(wtxn)
    }

    pub fn dump_vecs(&self, rtxn: &ReadTxn<'_>) -> Result<Vec<(EntityId, Vec<f32>)>, KvError> {
        let mut out = Vec::new();
        for item in self.vectors.iter(rtxn)? {
            let (key, val) = item?;
            if key.len() != 8 || val.len() % 4 != 0 {
                continue;
            }
            let eid = u64::from_be_bytes(key.as_ref().try_into().unwrap());
            let vec: Vec<f32> = val
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                .collect();
            out.push((eid, vec));
        }
        Ok(out)
    }

    /// Returns top-k `(EntityId, distance)` sorted ascending (smaller = closer).
    /// Returns top-k `(EntityId, distance)` sorted ascending (smaller = closer).
    /// Exact for small indexes; approximate (HNSW) above `BRUTE_FORCE_MAX` vectors.
    pub fn search_vec(
        &self,
        rtxn: &ReadTxn<'_>,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(EntityId, f32)>, KvError> {
        if k == 0 {
            return Ok(Vec::new());
        }
        if self.vectors.len(rtxn)? <= BRUTE_FORCE_MAX {
            return self.search_exact(rtxn, query, k);
        }
        let version = match self.meta.get(rtxn, VERSION_KEY)? {
            Some(b) if b.len() == 8 => u64::from_be_bytes(b.as_ref().try_into().unwrap()),
            _ => 0,
        };
        let mut guard = self.graph.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            if let Some(loaded) = self.load_graph(rtxn, version)? {
                *guard = Some(loaded);
                self.loads.fetch_add(1, Ordering::Relaxed);
            }
        }
        let mut current = false;
        if let Some((v0, graph)) = guard.as_mut() {
            if *v0 == version {
                current = true;
            } else if version > *v0 && version - *v0 <= LOG_RETENTION {
                let mut ops = Vec::with_capacity((version - *v0) as usize);
                for v in (*v0 + 1)..=version {
                    match self.log.get(rtxn, &v.to_be_bytes())? {
                        Some(op) => ops.push(op.into_owned()),
                        None => break,
                    }
                }
                if ops.len() as u64 == version - *v0 {
                    for op in &ops {
                        apply_op(graph, op);
                    }
                    *v0 = version;
                    self.incremental.fetch_add(1, Ordering::Relaxed);
                    current = !graph.needs_compaction();
                }
            }
        }
        if !current {
            let items = self.dump_vecs(rtxn)?;
            *guard = Some((version, Hnsw::build(self.metric, items)));
            self.rebuilds.fetch_add(1, Ordering::Relaxed);
            self.dirty.store(true, Ordering::Relaxed);
        }
        let graph = &guard.as_ref().expect("graph is current").1;
        Ok(graph.search(query, k, EF_SEARCH).into_iter().collect())
    }

    /// Exact top-k by scanning every vector, with bounded O(k) memory.
    fn search_exact(&self, rtxn: &ReadTxn<'_>, query: &[f32], k: usize) -> Result<Vec<(EntityId, f32)>, KvError> {
        #[derive(Clone, Copy)]
        struct Hit(f32, EntityId);
        impl PartialEq for Hit {
            fn eq(&self, other: &Self) -> bool {
                self.0 == other.0 && self.1 == other.1
            }
        }
        impl Eq for Hit {}
        impl PartialOrd for Hit {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }
        impl Ord for Hit {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                // Max-heap by distance so we can evict the worst candidate in O(log k)
                self.0
                    .partial_cmp(&other.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| self.1.cmp(&other.1))
            }
        }

        let mut heap: std::collections::BinaryHeap<Hit> = std::collections::BinaryHeap::with_capacity(k + 1);
        let mut candidate = Vec::with_capacity(query.len());
        for item in self.vectors.iter(rtxn)? {
            let (key, val) = item?;
            if key.len() != 8 || val.len() != query.len() * 4 {
                continue;
            }
            let eid = u64::from_be_bytes(key.as_ref().try_into().unwrap());
            candidate.clear();
            for chunk in val.chunks_exact(4) {
                candidate.push(f32::from_le_bytes(chunk.try_into().unwrap()));
            }
            let dist = compute_distance(self.metric, query, &candidate);
            if heap.len() < k {
                heap.push(Hit(dist, eid));
            } else if let Some(worst) = heap.peek() {
                if dist < worst.0 {
                    heap.pop();
                    heap.push(Hit(dist, eid));
                }
            }
        }
        let mut out: Vec<(EntityId, f32)> = heap.into_iter().map(|h| (h.1, h.0)).collect();
        out.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(out)
    }
}

/// Replays one logged vector write onto the graph.
fn apply_op(graph: &mut Hnsw, op: &[u8]) {
    match op.first() {
        Some(&OP_UPSERT) if op.len() >= 9 => {
            let eid = u64::from_be_bytes(op[1..9].try_into().unwrap());
            let vec: Vec<f32> = op[9..].chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
            graph.upsert(eid, vec);
        }
        Some(&OP_REMOVE) if op.len() == 9 => {
            graph.remove(u64::from_be_bytes(op[1..9].try_into().unwrap()));
        }
        _ => {}
    }
}

fn compute_distance(metric: Metric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        Metric::Euclidean => a
            .iter()
            .zip(b.iter())
            .map(|(x, y)| {
                let d = x - y;
                d * d
            })
            .sum::<f32>()
            .sqrt(),
        Metric::DotProduct => -a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f32>(),
        Metric::Cosine => {
            let mut dot = 0.0_f32;
            let mut na = 0.0_f32;
            let mut nb = 0.0_f32;
            for (x, y) in a.iter().zip(b.iter()) {
                dot += x * y;
                na += x * x;
                nb += y * y;
            }
            let denom = (na.sqrt() * nb.sqrt()).max(1e-12);
            1.0 - (dot / denom)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn bm25_update_and_remove() {
        let dir = tempdir().unwrap();
        let store = KvStore::open(dir.path(), 16 * 1024 * 1024, 8).unwrap();
        let mut wtxn = store.write_txn().unwrap();
        let fts = FullTextIndex::open(&store, &mut wtxn).unwrap();
        fts.index_doc(&mut wtxn, 1, "Apple Inc.").unwrap();
        fts.index_doc(&mut wtxn, 2, "Microsoft Corporation").unwrap();
        // Update entity 1 to a different name; "apple" should no longer match entity 1
        fts.index_doc(&mut wtxn, 1, "Alphabet Inc.").unwrap();
        wtxn.commit().unwrap();

        let rtxn = store.read_txn().unwrap();
        assert!(fts.search(&rtxn, "apple", 10).unwrap().is_empty());
        let alpha = fts.search(&rtxn, "alphabet", 10).unwrap();
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].0, 1);
    }

    #[test]
    fn vector_index_cosine_search() {
        let dir = tempdir().unwrap();
        let store = KvStore::open(dir.path(), 16 * 1024 * 1024, 8).unwrap();
        let mut wtxn = store.write_txn().unwrap();
        let vidx = VectorIndex::open(&store, &mut wtxn, "vec_sec", Metric::Cosine).unwrap();
        vidx.add_vec(&mut wtxn, 10, &[1.0, 0.0, 0.0]).unwrap();
        vidx.add_vec(&mut wtxn, 20, &[0.0, 1.0, 0.0]).unwrap();
        vidx.add_vec(&mut wtxn, 30, &[0.9, 0.1, 0.0]).unwrap();
        wtxn.commit().unwrap();

        let rtxn = store.read_txn().unwrap();
        let hits = vidx.search_vec(&rtxn, &[1.0, 0.05, 0.0], 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, 10);
        assert_eq!(hits[1].0, 30);
    }
}
