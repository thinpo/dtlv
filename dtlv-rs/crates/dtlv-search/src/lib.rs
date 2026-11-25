pub mod fulltext;
pub mod hnsw;
pub mod stem;

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
const VERSION_KEY: &[u8] = b"version";

/// LMDB-persisted vector index for entity embeddings (entity resolution & semantic lookup).
///
/// Vectors live in LMDB. Searches over more than `BRUTE_FORCE_MAX` vectors use an in-memory HNSW graph,
/// which is rebuilt whenever the persisted version counter differs from the one it was built at. This
/// keeps results consistent with aborts and older read snapshots. The cost is one rebuild after writes.
pub struct VectorIndex {
    vectors: RawDb,
    meta: RawDb,
    metric: Metric,
    graph: Mutex<Option<(u64, Hnsw)>>,
}

impl VectorIndex {
    pub fn open(store: &KvStore, wtxn: &mut WriteTxn<'_>, name: &str, metric: Metric) -> Result<Self, KvError> {
        let vectors = store.open_db(wtxn, name)?;
        let meta = store.open_db(wtxn, &format!("{name}__meta"))?;
        Ok(Self { vectors, meta, metric, graph: Mutex::new(None) })
    }

    fn bump_version(&self, wtxn: &mut WriteTxn<'_>) -> Result<(), KvError> {
        let cur = match self.meta.get_rw(wtxn, VERSION_KEY)? {
            Some(b) if b.len() == 8 => u64::from_be_bytes(b.as_ref().try_into().unwrap()),
            _ => 0,
        };
        self.meta.put(wtxn, VERSION_KEY, &(cur + 1).to_be_bytes())
    }

    pub fn add_vec(&self, wtxn: &mut WriteTxn<'_>, eid: EntityId, vec: &[f32]) -> Result<(), KvError> {
        let mut bytes = Vec::with_capacity(vec.len() * 4);
        for &f in vec {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
        self.vectors.put(wtxn, &eid.to_be_bytes(), &bytes)?;
        self.bump_version(wtxn)
    }

    pub fn remove_vec(&self, wtxn: &mut WriteTxn<'_>, eid: EntityId) -> Result<(), KvError> {
        self.vectors.delete(wtxn, &eid.to_be_bytes())?;
        self.bump_version(wtxn)
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
        if guard.as_ref().map(|(v, _)| *v) != Some(version) {
            let items = self.dump_vecs(rtxn)?;
            *guard = Some((version, Hnsw::build(self.metric, items)));
        }
        let graph = &guard.as_ref().expect("graph built above").1;
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
