//! BM25 full-text index with Datalevin-style analysis: optional English stopwords (default on),
//! optional Snowball stemming, and optional term positions for phrase queries.

use std::collections::{HashMap, HashSet};

use dtlv_codec::EntityId;
use dtlv_kv::{KvError, KvStore, RawDb, ReadTxn, WriteTxn};

use crate::stem::stem_en;

/// Analysis settings. They are fixed once documents are indexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FtsConfig {
    /// Drop the 35 English stopwords (Datalevin's default analyzer does).
    pub stopwords: bool,
    /// Stem each token with the English Snowball stemmer.
    pub stem: bool,
    /// Store token positions, which phrase queries need.
    pub index_positions: bool,
}

impl Default for FtsConfig {
    fn default() -> Self {
        Self { stopwords: true, stem: false, index_positions: false }
    }
}

impl FtsConfig {
    fn to_bits(self) -> u8 {
        (self.stopwords as u8) | ((self.stem as u8) << 1) | ((self.index_positions as u8) << 2)
    }

    fn from_bits(b: u8) -> Self {
        Self { stopwords: b & 1 != 0, stem: b & 2 != 0, index_positions: b & 4 != 0 }
    }
}

/// Query tree. `Text` is analyzed like documents, `Term` is matched exactly, `Phrase` needs positions.
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    Text(String),
    Term(String),
    Phrase(String),
    And(Vec<Query>),
    Or(Vec<Query>),
    Not(Box<Query>),
}

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it", "no", "not",
    "of", "on", "or", "such", "that", "the", "their", "then", "there", "these", "they", "this", "to", "was",
    "will", "with",
];

/// Splits text into `(term, position)` pairs. Stopwords are dropped but still advance the position,
/// so phrases keep their original word gaps.
pub fn analyze(text: &str, cfg: FtsConfig) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut pos = 0u32;
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let p = pos;
        pos += 1;
        let lower = raw.to_lowercase();
        if cfg.stopwords && STOPWORDS.contains(&lower.as_str()) {
            continue;
        }
        let term = if cfg.stem { stem_en(&lower) } else { lower };
        out.push((term, p));
    }
    out
}

/// Plain lowercase alphanumeric tokens, with no stopwords or stemming.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

const DOC_COUNT_KEY: &[u8] = b"__doc_count";
const TOTAL_LEN_KEY: &[u8] = b"__total_len";
const CONFIG_KEY: &[u8] = b"__config";
const K1: f64 = 1.2;
const B: f64 = 0.75;

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

fn posting_key(term: &str, doc: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(term.len() + 1 + doc.len());
    key.extend_from_slice(term.as_bytes());
    key.push(0x00);
    key.extend_from_slice(doc);
    key
}

/// Entity id of a document key (its first eight bytes).
pub fn doc_entity(doc: &[u8]) -> EntityId {
    u64::from_be_bytes(doc[..8].try_into().expect("document keys start with an entity id"))
}

/// Posting value: term frequency (u32 BE), then one u32 BE position per occurrence when positions are kept.
struct Posting {
    doc: Vec<u8>,
    tf: u32,
    positions: Vec<u32>,
}

#[derive(Clone, Copy)]
struct Stats {
    n_docs: f64,
    avg_dl: f64,
}

pub struct FullTextIndex {
    postings: RawDb,
    doc_lens: RawDb,
    doc_terms: RawDb,
    meta: RawDb,
    cfg: FtsConfig,
}

impl FullTextIndex {
    pub fn open(store: &KvStore, wtxn: &mut WriteTxn<'_>) -> Result<Self, KvError> {
        let postings = store.open_db(wtxn, "fts_postings")?;
        let doc_lens = store.open_db(wtxn, "fts_doc_lens")?;
        let doc_terms = store.open_db(wtxn, "fts_doc_terms")?;
        let meta = store.open_db(wtxn, "fts_meta")?;
        let cfg = match meta.get_rw(wtxn, CONFIG_KEY)? {
            Some(b) if b.len() == 1 => FtsConfig::from_bits(b[0]),
            _ => FtsConfig::default(),
        };
        Ok(Self { postings, doc_lens, doc_terms, meta, cfg })
    }

    pub fn config(&self) -> FtsConfig {
        self.cfg
    }

    /// Stores new analysis settings. Refused once any document is indexed, since existing postings
    /// would no longer match the analyzer.
    pub fn set_config(&mut self, wtxn: &mut WriteTxn<'_>, cfg: FtsConfig) -> Result<(), KvError> {
        if read_u64_rw(&self.meta, wtxn, DOC_COUNT_KEY)? > 0 && cfg != self.cfg {
            return Err(KvError::Config("cannot change analysis settings after documents are indexed".into()));
        }
        self.meta.put(wtxn, CONFIG_KEY, &[cfg.to_bits()])?;
        self.cfg = cfg;
        Ok(())
    }

    /// Removes one indexed document (a datom's text, keyed by `doc`) and adjusts global stats.
    pub fn remove_entry(&self, wtxn: &mut WriteTxn<'_>, doc: &[u8]) -> Result<(), KvError> {
        let old_len = read_u64_rw(&self.doc_lens, wtxn, doc)?;
        if let Some(blob) = self.doc_terms.get_rw(wtxn, doc)? {
            let terms_bytes = blob.to_vec();
            for term_slice in terms_bytes.split(|&b| b == 0x00) {
                if !term_slice.is_empty() {
                    let key = posting_key(std::str::from_utf8(term_slice).unwrap_or_default(), doc);
                    self.postings.delete(wtxn, &key)?;
                }
            }
            self.doc_terms.delete(wtxn, doc)?;
            self.doc_lens.delete(wtxn, doc)?;

            let n = read_u64_rw(&self.meta, wtxn, DOC_COUNT_KEY)?;
            let total = read_u64_rw(&self.meta, wtxn, TOTAL_LEN_KEY)?;
            self.meta.put(wtxn, DOC_COUNT_KEY, &n.saturating_sub(1).to_be_bytes())?;
            self.meta.put(wtxn, TOTAL_LEN_KEY, &total.saturating_sub(old_len).to_be_bytes())?;
        }
        Ok(())
    }

    /// Removes every document of an entity (all of its fulltext values). Prefer `remove_entry` per datom.
    pub fn remove_doc(&self, wtxn: &mut WriteTxn<'_>, eid: EntityId) -> Result<(), KvError> {
        let docs: Vec<Vec<u8>> = self
            .doc_lens
            .prefix_iter_rw(wtxn, &eid.to_be_bytes())?
            .map(|r| r.map(|(k, _)| k.to_vec()))
            .collect::<Result<_, _>>()?;
        for doc in docs {
            self.remove_entry(wtxn, &doc)?;
        }
        Ok(())
    }

    /// Indexes `text` as the document `doc`. Re-indexing the same key replaces its text. `doc` must start
    /// with the entity id, so several values of one entity stay separate documents.
    pub fn index_entry(&self, wtxn: &mut WriteTxn<'_>, doc: &[u8], text: &str) -> Result<(), KvError> {
        self.remove_entry(wtxn, doc)?;

        let tokens = analyze(text, self.cfg);
        if tokens.is_empty() {
            return Ok(());
        }
        let mut by_term: HashMap<&str, Vec<u32>> = HashMap::new();
        for (term, pos) in &tokens {
            by_term.entry(term.as_str()).or_default().push(*pos);
        }
        for (term, positions) in &by_term {
            let mut value = Vec::with_capacity(4 + 4 * positions.len());
            value.extend_from_slice(&(positions.len() as u32).to_be_bytes());
            if self.cfg.index_positions {
                for p in positions {
                    value.extend_from_slice(&p.to_be_bytes());
                }
            }
            self.postings.put(wtxn, &posting_key(term, doc), &value)?;
        }
        let doc_len = tokens.len() as u64;
        self.doc_lens.put(wtxn, doc, &doc_len.to_be_bytes())?;
        let unique: Vec<&str> = by_term.keys().copied().collect();
        self.doc_terms.put(wtxn, doc, unique.join("\0").as_bytes())?;

        let n = read_u64_rw(&self.meta, wtxn, DOC_COUNT_KEY)? + 1;
        let total = read_u64_rw(&self.meta, wtxn, TOTAL_LEN_KEY)? + doc_len;
        self.meta.put(wtxn, DOC_COUNT_KEY, &n.to_be_bytes())?;
        self.meta.put(wtxn, TOTAL_LEN_KEY, &total.to_be_bytes())?;
        Ok(())
    }

    /// Indexes an entity's whole text as one document (the entity-only key).
    pub fn index_doc(&self, wtxn: &mut WriteTxn<'_>, eid: EntityId, text: &str) -> Result<(), KvError> {
        self.index_entry(wtxn, &eid.to_be_bytes(), text)
    }

    fn stats(&self, rtxn: &ReadTxn<'_>) -> Result<Stats, KvError> {
        let n_docs = read_u64(&self.meta, rtxn, DOC_COUNT_KEY)? as f64;
        let total = read_u64(&self.meta, rtxn, TOTAL_LEN_KEY)? as f64;
        let avg_dl = if n_docs > 0.0 { (total / n_docs).max(1.0) } else { 1.0 };
        Ok(Stats { n_docs, avg_dl })
    }

    fn postings_of(&self, rtxn: &ReadTxn<'_>, term: &str) -> Result<Vec<Posting>, KvError> {
        let mut prefix = Vec::with_capacity(term.len() + 1);
        prefix.extend_from_slice(term.as_bytes());
        prefix.push(0x00);
        let mut out = Vec::new();
        for item in self.postings.prefix_iter(rtxn, &prefix)? {
            let (k, v) = item?;
            if k.len() <= prefix.len() || v.len() < 4 {
                continue;
            }
            let tf = u32::from_be_bytes(v[0..4].try_into().unwrap());
            let positions = v[4..].chunks_exact(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect();
            out.push(Posting { doc: k[prefix.len()..].to_vec(), tf, positions });
        }
        Ok(out)
    }

    /// BM25 contributions of one term, keyed by document.
    fn bm25_term(&self, rtxn: &ReadTxn<'_>, stats: Stats, postings: &[Posting]) -> Result<HashMap<Vec<u8>, f64>, KvError> {
        let df = postings.len() as f64;
        let idf = ((stats.n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
        let mut out = HashMap::with_capacity(postings.len());
        for p in postings {
            let dl = read_u64(&self.doc_lens, rtxn, &p.doc)? as f64;
            let tf = p.tf as f64;
            let denom = tf + K1 * (1.0 - B + B * (dl / stats.avg_dl));
            out.insert(p.doc.clone(), idf * (tf * (K1 + 1.0)) / denom);
        }
        Ok(out)
    }

    /// Documents matching one exact term, with BM25 scores.
    fn term_docs(&self, rtxn: &ReadTxn<'_>, term: &str, stats: Stats) -> Result<HashMap<Vec<u8>, f64>, KvError> {
        let postings = self.postings_of(rtxn, term)?;
        if postings.is_empty() {
            return Ok(HashMap::new());
        }
        self.bm25_term(rtxn, stats, &postings)
    }

    /// Collapses document scores to entities: an entity's score is the sum over its matching documents.
    fn collapse(docs: HashMap<Vec<u8>, f64>) -> HashMap<EntityId, f64> {
        let mut out: HashMap<EntityId, f64> = HashMap::new();
        for (doc, score) in docs {
            *out.entry(doc_entity(&doc)).or_insert(0.0) += score;
        }
        out
    }

    /// Entity-level evaluation. Leaves match documents and collapse to entities, so boolean operators see
    /// each entity once no matter how many of its values match.
    fn eval(&self, rtxn: &ReadTxn<'_>, q: &Query, stats: Stats) -> Result<HashMap<EntityId, f64>, KvError> {
        match q {
            Query::Text(text) => {
                let mut out: HashMap<EntityId, f64> = HashMap::new();
                for (term, _) in analyze(text, self.cfg) {
                    merge_or(&mut out, Self::collapse(self.term_docs(rtxn, &term, stats)?));
                }
                Ok(out)
            }
            Query::Term(term) => Ok(Self::collapse(self.term_docs(rtxn, term, stats)?)),
            Query::Phrase(text) => Ok(Self::collapse(self.eval_phrase(rtxn, text, stats)?)),
            Query::Or(qs) => {
                let mut out = HashMap::new();
                for sub in qs {
                    merge_or(&mut out, self.eval(rtxn, sub, stats)?);
                }
                Ok(out)
            }
            Query::And(qs) => {
                let mut iter = qs.iter();
                let Some(first) = iter.next() else {
                    return Ok(HashMap::new());
                };
                let mut out = self.eval(rtxn, first, stats)?;
                for sub in iter {
                    let other = self.eval(rtxn, sub, stats)?;
                    out.retain(|eid, _| other.contains_key(eid));
                    for (eid, score) in other {
                        if let Some(s) = out.get_mut(&eid) {
                            *s += score;
                        }
                    }
                }
                Ok(out)
            }
            Query::Not(inner) => {
                let excluded = self.eval(rtxn, inner, stats)?;
                let mut out = HashMap::new();
                for item in self.doc_lens.iter(rtxn)? {
                    let (k, _) = item?;
                    let eid = doc_entity(&k);
                    if !excluded.contains_key(&eid) {
                        out.insert(eid, 0.0);
                    }
                }
                Ok(out)
            }
        }
    }

    /// Documents containing the analyzed phrase at consecutive positions (after stopword gaps).
    fn eval_phrase(&self, rtxn: &ReadTxn<'_>, text: &str, stats: Stats) -> Result<HashMap<Vec<u8>, f64>, KvError> {
        if !self.cfg.index_positions {
            return Err(KvError::Config("phrase search requires index_positions".into()));
        }
        let tokens = analyze(text, self.cfg);
        let Some(&(_, base)) = tokens.first() else {
            return Ok(HashMap::new());
        };
        let mut per_term: Vec<(u32, HashMap<Vec<u8>, Vec<u32>>, Vec<Posting>)> = Vec::new();
        for (term, pos) in &tokens {
            let postings = self.postings_of(rtxn, term)?;
            if postings.is_empty() {
                return Ok(HashMap::new());
            }
            let by_doc = postings.iter().map(|p| (p.doc.clone(), p.positions.clone())).collect();
            per_term.push((pos - base, by_doc, postings));
        }
        // Candidate documents contain every term; keep those where some start position lines the terms up.
        let mut candidates: HashSet<Vec<u8>> = per_term[0].1.keys().cloned().collect();
        for (_, by_doc, _) in per_term.iter().skip(1) {
            candidates.retain(|doc| by_doc.contains_key(doc));
        }
        let mut hits: Vec<Vec<u8>> = Vec::new();
        for doc in candidates {
            let starts = per_term[0].1.get(&doc).cloned().unwrap_or_default();
            let matched = starts.iter().any(|&p0| {
                per_term
                    .iter()
                    .all(|(offset, by_doc, _)| by_doc.get(&doc).is_some_and(|ps| ps.contains(&(p0 + offset))))
            });
            if matched {
                hits.push(doc);
            }
        }
        let mut out: HashMap<Vec<u8>, f64> = HashMap::new();
        for (_, _, postings) in &per_term {
            let scores = self.bm25_term(rtxn, stats, postings)?;
            for doc in &hits {
                *out.entry(doc.clone()).or_insert(0.0) += scores.get(doc).copied().unwrap_or(0.0);
            }
        }
        Ok(out)
    }

    /// BM25 search for free text (same as `Query::Text`), returning `(EntityId, score)` sorted descending.
    pub fn search(&self, rtxn: &ReadTxn<'_>, query: &str, limit: usize) -> Result<Vec<(EntityId, f64)>, KvError> {
        self.search_query(rtxn, &Query::Text(query.to_owned()), limit)
    }

    /// Evaluates a query tree and returns the top `limit` entities by score.
    pub fn search_query(&self, rtxn: &ReadTxn<'_>, query: &Query, limit: usize) -> Result<Vec<(EntityId, f64)>, KvError> {
        let stats = self.stats(rtxn)?;
        if stats.n_docs == 0.0 {
            return Ok(Vec::new());
        }
        let scores = self.eval(rtxn, query, stats)?;
        Ok(rank(scores.into_iter().collect(), limit))
    }

    /// Document-level search for free text: `(document key, score)` sorted descending. The key starts with
    /// the entity id and carries whatever suffix the indexer chose (for datoms: attribute and value).
    pub fn search_docs(&self, rtxn: &ReadTxn<'_>, query: &str, limit: usize) -> Result<Vec<(Vec<u8>, f64)>, KvError> {
        let stats = self.stats(rtxn)?;
        if stats.n_docs == 0.0 {
            return Ok(Vec::new());
        }
        let mut docs: HashMap<Vec<u8>, f64> = HashMap::new();
        for (term, _) in analyze(query, self.cfg) {
            for (doc, score) in self.term_docs(rtxn, &term, stats)? {
                *docs.entry(doc).or_insert(0.0) += score;
            }
        }
        let mut ranked: Vec<(Vec<u8>, f64)> = docs.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        ranked.truncate(limit);
        Ok(ranked)
    }
}

/// Sorts `(id, score)` pairs by descending score, breaking ties by id, and keeps the top `limit`.
fn rank<K: Ord + Clone>(mut items: Vec<(K, f64)>, limit: usize) -> Vec<(EntityId, f64)>
where
    K: Into<EntityId>,
{
    items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    items.truncate(limit);
    items.into_iter().map(|(k, s)| (k.into(), s)).collect()
}

fn merge_or(into: &mut HashMap<EntityId, f64>, from: HashMap<EntityId, f64>) {
    for (eid, score) in from {
        *into.entry(eid).or_insert(0.0) += score;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzer_drops_stopwords_but_keeps_position_gaps() {
        let toks = analyze("The quick fox, and the dog", FtsConfig::default());
        let terms: Vec<(&str, u32)> = toks.iter().map(|(t, p)| (t.as_str(), *p)).collect();
        assert_eq!(terms, vec![("quick", 1), ("fox", 2), ("dog", 5)]);
    }

    #[test]
    fn stemming_option_is_applied_to_tokens() {
        let cfg = FtsConfig { stem: true, ..FtsConfig::default() };
        let terms: Vec<String> = analyze("Running dogs", cfg).into_iter().map(|(t, _)| t).collect();
        assert_eq!(terms, vec!["run", "dog"]);
    }

    #[test]
    fn config_bits_round_trip() {
        for bits in 0..8u8 {
            assert_eq!(FtsConfig::from_bits(bits).to_bits(), bits);
        }
    }
}
