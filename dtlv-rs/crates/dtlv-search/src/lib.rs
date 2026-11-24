use std::collections::HashMap;
use dtlv_codec::EntityId;
use dtlv_kv::{KvError, KvStore, RawDb, ReadTxn, WriteTxn};

pub struct FullTextIndex {
    postings: RawDb,
    doc_lens: RawDb,
    doc_terms: RawDb,
    meta: RawDb,
}

const DOC_COUNT_KEY: &[u8] = b"__doc_count";
const TOTAL_LEN_KEY: &[u8] = b"__total_len";

pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

fn read_u64(db: &RawDb, rtxn: &ReadTxn<'_>, key: &[u8]) -> Result<u64, KvError> {
    Ok(match db.get(rtxn, key)? {
        Some(b) if b.len() == 8 => u64::from_be_bytes(b.as_ref().try_into().unwrap()),
        _ => 0,
    })
}

fn read_u64_rw(db: &RawDb, wtxn: &WriteTxn<'_>, key: &[u8]) -> Result<u64, KvError> {
    Ok(match db.get_rw(wtxn, key)? {
        Some(b) if b.len() == 8 => u64::from_be_bytes(b.as_ref().try_into().unwrap()),
        _ => 0,
    })
}

fn posting_key(term: &str, eid: EntityId, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(term.as_bytes());
    out.push(0x00);
    out.extend_from_slice(&eid.to_be_bytes());
}

fn term_prefix(term: &str, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(term.as_bytes());
    out.push(0x00);
}

fn encode_terms_blob(terms: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, t) in terms.iter().enumerate() {
        if i > 0 {
            out.push(0x00);
        }
        out.extend_from_slice(t.as_bytes());
    }
    out
}

impl FullTextIndex {
    pub fn open(store: &KvStore, wtxn: &mut WriteTxn<'_>) -> Result<Self, KvError> {
        let postings = store.open_db(wtxn, "fts_postings")?;
        let doc_lens = store.open_db(wtxn, "fts_doc_lens")?;
        let doc_terms = store.open_db(wtxn, "fts_doc_terms")?;
        let meta = store.open_db(wtxn, "fts_meta")?;
        Ok(Self {
            postings,
            doc_lens,
            doc_terms,
            meta,
        })
    }

    /// Remove an entity's previous full-text postings and adjust global stats.
    pub fn remove_doc(&self, wtxn: &mut WriteTxn<'_>, eid: EntityId) -> Result<(), KvError> {
        let eid_key = eid.to_be_bytes();
        let old_len = read_u64_rw(&self.doc_lens, wtxn, &eid_key)?;
        if let Some(blob) = self.doc_terms.get_rw(wtxn, &eid_key)? {
            let terms_bytes = blob.to_vec();
            let mut key = Vec::with_capacity(64);
            for term_slice in terms_bytes.split(|&b| b == 0x00) {
                if !term_slice.is_empty() {
                    key.clear();
                    key.extend_from_slice(term_slice);
                    key.push(0x00);
                    key.extend_from_slice(&eid_key);
                    self.postings.delete(wtxn, &key)?;
                }
            }
            self.doc_terms.delete(wtxn, &eid_key)?;
            self.doc_lens.delete(wtxn, &eid_key)?;

            let n = read_u64_rw(&self.meta, wtxn, DOC_COUNT_KEY)?;
            let total = read_u64_rw(&self.meta, wtxn, TOTAL_LEN_KEY)?;
            self.meta
                .put(wtxn, DOC_COUNT_KEY, &n.saturating_sub(1).to_be_bytes())?;
            self.meta.put(
                wtxn,
                TOTAL_LEN_KEY,
                &total.saturating_sub(old_len).to_be_bytes(),
            )?;
        }
        Ok(())
    }

    pub fn index_doc(&self, wtxn: &mut WriteTxn<'_>, eid: EntityId, text: &str) -> Result<(), KvError> {
        // Ensure idempotent replacement if entity already had indexed text
        self.remove_doc(wtxn, eid)?;

        let tokens = tokenize(text);
        if tokens.is_empty() {
            return Ok(());
        }
        let mut counts: HashMap<String, u32> = HashMap::new();
        for tok in &tokens {
            *counts.entry(tok.clone()).or_insert(0) += 1;
        }
        let mut key = Vec::with_capacity(64);
        let mut unique_terms = Vec::with_capacity(counts.len());
        for (term, tf) in counts {
            posting_key(&term, eid, &mut key);
            self.postings.put(wtxn, &key, &tf.to_be_bytes())?;
            unique_terms.push(term);
        }
        let eid_key = eid.to_be_bytes();
        let doc_len = tokens.len() as u64;
        self.doc_lens.put(wtxn, &eid_key, &doc_len.to_be_bytes())?;
        self.doc_terms
            .put(wtxn, &eid_key, &encode_terms_blob(&unique_terms))?;

        let n = read_u64_rw(&self.meta, wtxn, DOC_COUNT_KEY)? + 1;
        let total = read_u64_rw(&self.meta, wtxn, TOTAL_LEN_KEY)? + doc_len;
        self.meta.put(wtxn, DOC_COUNT_KEY, &n.to_be_bytes())?;
        self.meta.put(wtxn, TOTAL_LEN_KEY, &total.to_be_bytes())?;
        Ok(())
    }

    /// BM25 full-text search returning (EntityId, score) sorted descending.
    pub fn search(
        &self,
        rtxn: &ReadTxn<'_>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(EntityId, f64)>, KvError> {
        let terms = tokenize(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let n_docs = read_u64(&self.meta, rtxn, DOC_COUNT_KEY)? as f64;
        if n_docs == 0.0 {
            return Ok(Vec::new());
        }
        let total_len = read_u64(&self.meta, rtxn, TOTAL_LEN_KEY)? as f64;
        let avg_dl = (total_len / n_docs).max(1.0);
        let k1 = 1.2_f64;
        let b = 0.75_f64;

        let mut scores: HashMap<EntityId, f64> = HashMap::new();
        let mut pref = Vec::with_capacity(64);
        for term in terms {
            term_prefix(&term, &mut pref);
            let mut hits: Vec<(EntityId, u32)> = Vec::new();
            for item in self.postings.prefix_iter(rtxn, &pref)? {
                let (k, v) = item?;
                if k.len() >= 9 && v.len() == 4 {
                    let eid = u64::from_be_bytes(k[k.len() - 8..].try_into().unwrap());
                    let tf = u32::from_be_bytes(v.as_ref().try_into().unwrap());
                    hits.push((eid, tf));
                }
            }
            let df = hits.len() as f64;
            if df == 0.0 {
                continue;
            }
            let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
            for (eid, tf) in hits {
                let dl = read_u64(&self.doc_lens, rtxn, &eid.to_be_bytes())? as f64;
                let tf_f = tf as f64;
                let denom = tf_f + k1 * (1.0 - b + b * (dl / avg_dl));
                let bm25 = idf * (tf_f * (k1 + 1.0)) / denom;
                *scores.entry(eid).or_insert(0.0) += bm25;
            }
        }
        let mut ranked: Vec<(EntityId, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(limit);
        Ok(ranked)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Cosine,
    Euclidean,
    DotProduct,
}

/// LMDB-persisted vector index for entity embeddings (entity resolution & semantic lookup).
pub struct VectorIndex {
    vectors: RawDb,
    metric: Metric,
}

impl VectorIndex {
    pub fn open(store: &KvStore, wtxn: &mut WriteTxn<'_>, name: &str, metric: Metric) -> Result<Self, KvError> {
        let vectors = store.open_db(wtxn, name)?;
        Ok(Self { vectors, metric })
    }

    pub fn add_vec(&self, wtxn: &mut WriteTxn<'_>, eid: EntityId, vec: &[f32]) -> Result<(), KvError> {
        let mut bytes = Vec::with_capacity(vec.len() * 4);
        for &f in vec {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
        self.vectors.put(wtxn, &eid.to_be_bytes(), &bytes)?;
        Ok(())
    }

    pub fn remove_vec(&self, wtxn: &mut WriteTxn<'_>, eid: EntityId) -> Result<(), KvError> {
        self.vectors.delete(wtxn, &eid.to_be_bytes())?;
        Ok(())
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

    /// Returns top-k `(EntityId, distance)` sorted ascending (smaller = closer) using bounded O(k) memory.
    pub fn search_vec(
        &self,
        rtxn: &ReadTxn<'_>,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(EntityId, f32)>, KvError> {
        if k == 0 {
            return Ok(Vec::new());
        }
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
