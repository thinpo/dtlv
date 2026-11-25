//! Indexed documents (idoc): JSON-shaped values with a path index.
//!
//! A document is stored as canonical JSON text in a `Value::Doc`. Every scalar leaf is indexed under three
//! path forms: the concrete path (array indices kept), the collapsed path (array indices removed, so a
//! scalar matches any element), and the wildcard path (indices replaced by `*`). Lookups pick the form that
//! matches the predicate's path. Mixed forms (some indices explicit, others wildcard) are not indexed.
//!
//! Index keys are `aid | escaped(path) | escaped(value) | eid`, so all entities with a given path and
//! value form one contiguous range.

use std::collections::BTreeSet;

use dtlv_codec::{encode_escaped_bytes, encode_value, AttrId, EntityId, Value};
use dtlv_kv::{ReadTxn, WriteTxn};
use serde_json::Value as Json;

use crate::parser::{parse_edn, Edn};
use crate::{Conn, DatalogError};

const JOB_PUT: u8 = 0;
const JOB_DEL: u8 = 1;
const IDOC_JOB_SEQ_KEY: &[u8] = b"__idoc_job_seq";

/// One step of a document path. `Index` addresses one array element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSeg {
    Key(String),
    Index(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Key(String),
    Idx(u32),
}

/// JSON null is indexed as the keyword `:json/null`, as in Datalevin.
pub const JSON_NULL: &str = ":json/null";

/// Parses JSON text and returns the stored form: canonical JSON in a `Value::Doc`.
/// The root must be an object or an array.
pub fn idoc_value(text: &str) -> Result<Value, DatalogError> {
    let doc: Json = serde_json::from_str(text).map_err(|e| DatalogError::Type(format!("invalid idoc JSON: {e}")))?;
    if !(doc.is_object() || doc.is_array()) {
        return Err(DatalogError::Type("idoc root must be an object or an array".into()));
    }
    let canonical = serde_json::to_string(&doc).map_err(|e| DatalogError::Type(e.to_string()))?;
    Ok(Value::Doc(canonical))
}

fn scalar_value(doc: &Json) -> Value {
    match doc {
        Json::Null => Value::Keyword(JSON_NULL.into()),
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::I64(i),
            None => Value::F64(n.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(s) => Value::Str(s.clone()),
        Json::Array(_) | Json::Object(_) => unreachable!("only scalars reach scalar_value"),
    }
}

fn collect_leaves(doc: &Json, path: &mut Vec<Seg>, out: &mut Vec<(Vec<Seg>, Value)>) {
    match doc {
        Json::Object(m) => {
            for (k, v) in m {
                path.push(Seg::Key(k.clone()));
                collect_leaves(v, path, out);
                path.pop();
            }
        }
        Json::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                path.push(Seg::Idx(i as u32));
                collect_leaves(v, path, out);
                path.pop();
            }
        }
        scalar => out.push((path.clone(), scalar_value(scalar))),
    }
}

fn encode_path(segs: &[Seg]) -> Vec<u8> {
    let mut out = Vec::new();
    for seg in segs {
        match seg {
            Seg::Key(k) => {
                out.push(0x01);
                encode_escaped_bytes(k.as_bytes(), &mut out);
            }
            Seg::Idx(i) => {
                out.push(0x02);
                out.extend_from_slice(&i.to_be_bytes());
            }

        }
    }
    out
}

/// The three indexed forms of one leaf path, without duplicates.
/// Array levels beyond this are indexed only at the two extremes (all indices kept, all dropped).
const MAX_INDEXED_ARRAY_LEVELS: usize = 6;

/// The indexed forms of one leaf path. Each array index on the path is either kept (a query can name that
/// position) or dropped (a query can match any element there), so a query with any mix of named and free
/// positions finds the leaf. There are 2^d forms for d array levels.
fn path_forms(segs: &[Seg]) -> Vec<Vec<Seg>> {
    let positions: Vec<usize> = segs
        .iter()
        .enumerate()
        .filter_map(|(i, s)| matches!(s, Seg::Idx(_)).then_some(i))
        .collect();
    if positions.len() > MAX_INDEXED_ARRAY_LEVELS {
        let all_kept = segs.to_vec();
        let all_dropped: Vec<Seg> = segs.iter().filter(|s| !matches!(s, Seg::Idx(_))).cloned().collect();
        return vec![all_kept, all_dropped];
    }
    (0..1usize << positions.len())
        .map(|mask| {
            let drop: Vec<usize> = positions
                .iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) == 0)
                .map(|(_, &pos)| pos)
                .collect();
            segs.iter()
                .enumerate()
                .filter(|(i, _)| !drop.contains(i))
                .map(|(_, s)| s.clone())
                .collect()
        })
        .collect()
}

/// Prefix for `(aid, path, value)`; the eid suffix follows.
fn lookup_prefix(aid: AttrId, path: &[Seg], val: &Value) -> Vec<u8> {
    let mut key = aid.to_be_bytes().to_vec();
    encode_escaped_bytes(&encode_path(path), &mut key);
    let mut vbytes = Vec::new();
    encode_value(val, &mut vbytes);
    encode_escaped_bytes(&vbytes, &mut key);
    key
}

fn index_key(aid: AttrId, path: &[Seg], val: &Value, eid: EntityId) -> Vec<u8> {
    let mut key = lookup_prefix(aid, path, val);
    key.extend_from_slice(&eid.to_be_bytes());
    key
}

/// Whether a selector addresses a leaf path. A selector without positions matches across array elements,
/// so its keys are compared against the leaf's keys with array indices removed.
fn selector_matches(sel: &[PathSeg], leaf: &[Seg]) -> bool {
    let has_index = sel.iter().any(|s| matches!(s, PathSeg::Index(_)));
    let leaf: Vec<Seg> = if has_index {
        leaf.to_vec()
    } else {
        leaf.iter().filter(|s| !matches!(s, Seg::Idx(_))).cloned().collect()
    };
    sel.len() <= leaf.len()
        && sel.iter().zip(&leaf).all(|(p, l)| match (p, l) {
            (PathSeg::Key(a), Seg::Key(b)) => a == b,
            (PathSeg::Index(a), Seg::Idx(b)) => a == b,
            _ => false,
        })
}

/// Walks a document along `path`. Returns the subvalue, or `None` if the path is absent.
pub fn idoc_get(doc_text: &str, path: &[PathSeg]) -> Option<Json> {
    let mut cur: Json = serde_json::from_str(doc_text).ok()?;
    for seg in path {
        cur = match (seg, cur) {
            (PathSeg::Key(k), Json::Object(mut m)) => m.remove(k)?,
            (PathSeg::Index(i), Json::Array(mut a)) => {
                let i = *i as usize;
                if i < a.len() {
                    a.swap_remove(i)
                } else {
                    return None;
                }
            }
            _ => return None,
        };
    }
    Some(cur)
}

/// Converts a JSON object pattern such as `{"profile": {"age": 30}}` into conjunctive leaf predicates.
/// Arrays are not allowed in patterns; use explicit `PathSeg::Index` predicates for positions.
pub fn pattern_predicates(pattern: &Json) -> Result<Vec<(Vec<PathSeg>, Value)>, DatalogError> {
    fn walk(p: &Json, path: &mut Vec<PathSeg>, out: &mut Vec<(Vec<PathSeg>, Value)>) -> Result<(), DatalogError> {
        match p {
            Json::Object(m) => {
                for (k, v) in m {
                    path.push(PathSeg::Key(k.clone()));
                    walk(v, path, out)?;
                    path.pop();
                }
                Ok(())
            }
            Json::Array(_) => Err(DatalogError::Type("idoc patterns cannot contain arrays".into())),
            scalar => {
                out.push((path.clone(), scalar_value(scalar)));
                Ok(())
            }
        }
    }
    if !pattern.is_object() {
        return Err(DatalogError::Type("idoc pattern must be an object".into()));
    }
    let mut out = Vec::new();
    walk(pattern, &mut Vec::new(), &mut out)?;
    Ok(out)
}

fn to_segs(path: &[PathSeg]) -> Vec<Seg> {
    path.iter()
        .map(|s| match s {
            PathSeg::Key(k) => Seg::Key(k.clone()),
            PathSeg::Index(i) => Seg::Idx(*i),
        })
        .collect()
}

impl Conn {
    /// Index keys for every leaf of `doc_text` under attribute `aid`.
    fn idoc_keys(&self, aid: AttrId, eid: EntityId, doc_text: &str) -> Result<Vec<Vec<u8>>, DatalogError> {
        let doc: Json = serde_json::from_str(doc_text).map_err(|e| DatalogError::Type(e.to_string()))?;
        let mut leaves = Vec::new();
        collect_leaves(&doc, &mut Vec::new(), &mut leaves);
        let spec = self.attrs.get(&aid);
        // An attribute's own selectors win; otherwise its named domain's apply.
        let domain = spec.and_then(|s| s.idoc_domain.as_deref()).and_then(|d| self.idoc_domains.get(d));
        let (indexed, excluded): (Option<&[Vec<PathSeg>]>, &[Vec<PathSeg>]) = match spec {
            Some(s) if s.indexed_paths.is_some() || !s.excluded_paths.is_empty() => {
                (s.indexed_paths.as_deref(), s.excluded_paths.as_slice())
            }
            _ => match domain {
                Some(d) => (d.indexed.as_deref(), d.excluded.as_slice()),
                None => (None, &[]),
            },
        };
        let mut keys = Vec::new();
        for (segs, val) in &leaves {
            let in_scope = indexed.is_none_or(|sels| sels.iter().any(|sel| selector_matches(sel, segs)));
            if !in_scope || excluded.iter().any(|sel| selector_matches(sel, segs)) {
                continue;
            }
            for form in path_forms(segs) {
                keys.push(index_key(aid, &form, val, eid));
            }
        }
        Ok(keys)
    }

    /// Index changes for `aid`'s documents. Synchronous attributes are indexed in this transaction; async ones
    /// record a job that `process_idoc_jobs` applies later. Either way the change commits with the source write.
    pub(crate) fn idoc_put(
        &self,
        wtxn: &mut WriteTxn<'_>,
        aid: AttrId,
        eid: EntityId,
        doc_text: &str,
    ) -> Result<(), DatalogError> {
        if self.is_async_idoc(aid) {
            return self.enqueue_idoc_job(wtxn, aid, eid, JOB_PUT, doc_text);
        }
        self.idoc_put_now(wtxn, aid, eid, doc_text)
    }

    pub(crate) fn idoc_del(
        &self,
        wtxn: &mut WriteTxn<'_>,
        aid: AttrId,
        eid: EntityId,
        doc_text: &str,
    ) -> Result<(), DatalogError> {
        if self.is_async_idoc(aid) {
            return self.enqueue_idoc_job(wtxn, aid, eid, JOB_DEL, doc_text);
        }
        self.idoc_del_now(wtxn, aid, eid, doc_text)
    }

    fn is_async_idoc(&self, aid: AttrId) -> bool {
        self.attrs.get(&aid).is_some_and(|s| s.idoc && s.idoc_async)
    }

    fn enqueue_idoc_job(
        &self,
        wtxn: &mut WriteTxn<'_>,
        aid: AttrId,
        eid: EntityId,
        op: u8,
        doc_text: &str,
    ) -> Result<(), DatalogError> {
        let seq = match self.meta.get_rw(wtxn, IDOC_JOB_SEQ_KEY)? {
            Some(b) if b.len() == 8 => u64::from_be_bytes(b.as_ref().try_into().unwrap()),
            _ => 0,
        };
        let mut val = aid.to_be_bytes().to_vec();
        val.extend_from_slice(&eid.to_be_bytes());
        val.push(op);
        val.extend_from_slice(doc_text.as_bytes());
        self.idoc_jobs.put(wtxn, &seq.to_be_bytes(), &val)?;
        self.meta.put(wtxn, IDOC_JOB_SEQ_KEY, &(seq + 1).to_be_bytes())?;
        Ok(())
    }

    /// Number of index jobs waiting for `process_idoc_jobs`.
    pub fn idoc_jobs_pending(&self) -> Result<u64, DatalogError> {
        let rtxn = self.store.read_txn()?;
        let mut n = 0;
        for item in self.idoc_jobs.iter(&rtxn)? {
            item?;
            n += 1;
        }
        Ok(n)
    }

    /// Applies pending index jobs in the order they were written, in one transaction. Returns how many ran.
    pub fn process_idoc_jobs(&self) -> Result<usize, DatalogError> {
        let mut wtxn = self.store.write_txn()?;
        let jobs = self.idoc_jobs.iter_rw(&wtxn)?;
        let n = jobs.len();
        for (seq, val) in jobs {
            let aid = u32::from_be_bytes(val[0..4].try_into().unwrap());
            let eid = u64::from_be_bytes(val[4..12].try_into().unwrap());
            let op = val[12];
            let text = std::str::from_utf8(&val[13..]).map_err(|e| DatalogError::Type(e.to_string()))?;
            match op {
                JOB_PUT => self.idoc_put_now(&mut wtxn, aid, eid, text)?,
                _ => self.idoc_del_now(&mut wtxn, aid, eid, text)?,
            }
            self.idoc_jobs.delete(&mut wtxn, &seq)?;
        }
        wtxn.commit()?;
        Ok(n)
    }

    pub(crate) fn idoc_put_now(
        &self,
        wtxn: &mut WriteTxn<'_>,
        aid: AttrId,
        eid: EntityId,
        doc_text: &str,
    ) -> Result<(), DatalogError> {
        for key in self.idoc_keys(aid, eid, doc_text)? {
            self.idoc_idx.put(wtxn, &key, &[])?;
        }
        Ok(())
    }

    pub(crate) fn idoc_del_now(
        &self,
        wtxn: &mut WriteTxn<'_>,
        aid: AttrId,
        eid: EntityId,
        doc_text: &str,
    ) -> Result<(), DatalogError> {
        for key in self.idoc_keys(aid, eid, doc_text)? {
            self.idoc_idx.delete(wtxn, &key)?;
        }
        Ok(())
    }

    /// Entities whose documents satisfy every predicate (conjunction), as `(eid, attr, doc)` triples.
    /// With `attr: None`, every idoc attribute is searched.
    pub fn idoc_match(
        &self,
        rtxn: &ReadTxn<'_>,
        attr: Option<&str>,
        preds: &[(Vec<PathSeg>, Value)],
    ) -> Result<Vec<(EntityId, String, Value)>, DatalogError> {
        let targets: Vec<(AttrId, String)> = match attr {
            Some(name) => {
                let aid = self.attr_id(name)?;
                if !self.attrs.get(&aid).is_some_and(|s| s.idoc) {
                    return Err(DatalogError::Type(format!("{name} is not an idoc attribute")));
                }
                vec![(aid, name.to_owned())]
            }
            None => self
                .attrs
                .iter()
                .filter(|(_, s)| s.idoc)
                .map(|(aid, s)| (*aid, s.name.clone()))
                .collect(),
        };
        let mut out = Vec::new();
        for (aid, name) in targets {
            let mut matched: Option<BTreeSet<EntityId>> = None;
            for (path, val) in preds {
                // The predicate's path as written: named positions are kept, and array levels it leaves unnamed
                // were indexed with those positions dropped.
                let segs = to_segs(path);
                let prefix = lookup_prefix(aid, &segs, val);
                let mut eids = BTreeSet::new();
                for item in self.idoc_idx.prefix_iter(rtxn, &prefix)? {
                    let (k, _) = item?;
                    eids.insert(u64::from_be_bytes(k[k.len() - 8..].try_into().unwrap()));
                }
                matched = Some(match matched {
                    None => eids,
                    Some(prev) => prev.intersection(&eids).copied().collect(),
                });
            }
            for eid in matched.unwrap_or_default() {
                if let Some(doc) = self.get_attr(rtxn, eid, &name)? {
                    out.push((eid, name.clone(), doc));
                }
            }
        }
        Ok(out)
    }
}

/// One `:db.fn/patchIdoc` operation. Paths address existing containers, except that `Set` and `Assoc` may
/// add a new map key.
#[derive(Debug, Clone, PartialEq)]
pub enum PatchOp {
    /// Replace the value at a path (an empty path replaces the whole document).
    Set(Vec<PathSeg>, Json),
    /// Remove a map key or vector element. Absent keys are ignored.
    Unset(Vec<PathSeg>),
    /// Append to the vector at a path.
    Conj(Vec<PathSeg>, Json),
    /// Merge a map into the map at a path.
    Merge(Vec<PathSeg>, Json),
    /// Set one key of the map at a path.
    Assoc(Vec<PathSeg>, String, Json),
    /// Remove one key of the map at a path.
    Dissoc(Vec<PathSeg>, String),
    /// Add an integer to the number at a path.
    Inc(Vec<PathSeg>, i64),
    /// Subtract an integer from the number at a path.
    Dec(Vec<PathSeg>, i64),
}

fn patch_err(msg: impl Into<String>) -> DatalogError {
    DatalogError::Type(format!("patchIdoc: {}", msg.into()))
}

fn navigate<'a>(root: &'a mut Json, path: &[PathSeg]) -> Result<&'a mut Json, DatalogError> {
    let mut cur = root;
    for seg in path {
        cur = match (seg, cur) {
            (PathSeg::Key(k), Json::Object(m)) => m.get_mut(k).ok_or_else(|| patch_err(format!("no key {k}")))?,
            (PathSeg::Index(i), Json::Array(a)) => {
                a.get_mut(*i as usize).ok_or_else(|| patch_err(format!("no index {i}")))?
            }
            _ => return Err(patch_err("path does not match the document shape")),
        };
    }
    Ok(cur)
}

fn apply_op(doc: &mut Json, op: &PatchOp) -> Result<(), DatalogError> {
    match op {
        PatchOp::Set(path, v) => match path.split_last() {
            None => *doc = v.clone(),
            Some((last, parent_path)) => {
                let parent = navigate(doc, parent_path)?;
                match (last, parent) {
                    (PathSeg::Key(k), Json::Object(m)) => {
                        m.insert(k.clone(), v.clone());
                    }
                    (PathSeg::Index(i), Json::Array(a)) if (*i as usize) < a.len() => a[*i as usize] = v.clone(),
                    _ => return Err(patch_err("set target does not exist")),
                }
            }
        },
        PatchOp::Unset(path) => {
            if let Some((last, parent_path)) = path.split_last() {
                match (last, navigate(doc, parent_path)?) {
                    (PathSeg::Key(k), Json::Object(m)) => {
                        m.remove(k);
                    }
                    (PathSeg::Index(i), Json::Array(a)) => {
                        if (*i as usize) < a.len() {
                            a.remove(*i as usize);
                        }
                    }
                    _ => return Err(patch_err("unset target has the wrong shape")),
                }
            } else {
                return Err(patch_err("cannot unset the whole document"));
            }
        }
        PatchOp::Conj(path, v) => match navigate(doc, path)? {
            Json::Array(a) => a.push(v.clone()),
            _ => return Err(patch_err("conj target is not a vector")),
        },
        PatchOp::Merge(path, v) => match (navigate(doc, path)?, v) {
            (Json::Object(m), Json::Object(add)) => {
                for (k, val) in add {
                    m.insert(k.clone(), val.clone());
                }
            }
            _ => return Err(patch_err("merge needs a map target and a map value")),
        },
        PatchOp::Assoc(path, k, v) => match navigate(doc, path)? {
            Json::Object(m) => {
                m.insert(k.clone(), v.clone());
            }
            _ => return Err(patch_err("assoc target is not a map")),
        },
        PatchOp::Dissoc(path, k) => match navigate(doc, path)? {
            Json::Object(m) => {
                m.remove(k);
            }
            _ => return Err(patch_err("dissoc target is not a map")),
        },
        PatchOp::Inc(path, n) | PatchOp::Dec(path, n) => {
            let delta = if matches!(op, PatchOp::Inc(..)) { *n } else { n.checked_neg().ok_or_else(|| patch_err("overflow"))? };
            let target = navigate(doc, path)?;
            let cur = target.as_i64().ok_or_else(|| patch_err("inc/dec target is not an integer"))?;
            let next = cur.checked_add(delta).ok_or_else(|| patch_err("integer overflow"))?;
            *target = Json::from(next);
        }
    }
    Ok(())
}

/// Applies patch operations to a stored document and returns its canonical text.
/// The root must stay an object or an array.
pub fn apply_patch(doc_text: &str, ops: &[PatchOp]) -> Result<String, DatalogError> {
    let mut doc: Json = serde_json::from_str(doc_text).map_err(|e| patch_err(e.to_string()))?;
    for op in ops {
        apply_op(&mut doc, op)?;
    }
    if !(doc.is_object() || doc.is_array()) {
        return Err(patch_err("the document root must stay an object or an array"));
    }
    serde_json::to_string(&doc).map_err(|e| patch_err(e.to_string()))
}

/// Parses EDN text (`:edn` idoc format) into the stored form. Keyword map keys lose their colon, so
/// `{:status "active"}` is stored as `{"status": "active"}`. Keyword values, lists, and sets are rejected,
/// since the stored form is JSON.
pub fn idoc_value_edn(text: &str) -> Result<Value, DatalogError> {
    fn convert(e: &Edn) -> Result<Json, DatalogError> {
        Ok(match e {
            Edn::Str(s) => Json::String(s.clone()),
            Edn::Int(i) => Json::from(*i),
            Edn::Float(f) => serde_json::Number::from_f64(*f)
                .map(Json::Number)
                .ok_or_else(|| DatalogError::Type("idoc EDN float must be finite".into()))?,
            Edn::Bool(b) => Json::Bool(*b),
            Edn::Vec(items) => Json::Array(items.iter().map(convert).collect::<Result<_, _>>()?),
            Edn::Map(pairs) => {
                let mut m = serde_json::Map::new();
                for (k, v) in pairs {
                    let key = match k {
                        Edn::Keyword(k) => k.trim_start_matches(':').to_owned(),
                        Edn::Str(s) => s.clone(),
                        _ => return Err(DatalogError::Type("idoc map keys must be keywords or strings".into())),
                    };
                    m.insert(key, convert(v)?);
                }
                Json::Object(m)
            }
            Edn::Keyword(_) | Edn::Symbol(_) => {
                return Err(DatalogError::Type("idoc EDN values cannot be keywords or symbols".into()))
            }
            Edn::List(_) => return Err(DatalogError::Type("idoc does not allow lists".into())),
        })
    }
    let edn = parse_edn(text).map_err(|e| DatalogError::Type(format!("invalid idoc EDN: {e}")))?;
    if !matches!(edn, Edn::Map(_) | Edn::Vec(_)) {
        return Err(DatalogError::Type("idoc root must be a map or a vector".into()));
    }
    let json = convert(&edn)?;
    let canonical = serde_json::to_string(&json).map_err(|e| DatalogError::Type(e.to_string()))?;
    Ok(Value::Doc(canonical))
}

/// Normalizes a Markdown header into a map key, as Datalevin does: lowercase, leading list numbering removed,
/// characters other than letters, digits, spaces and hyphens dropped, spaces to hyphens, hyphens collapsed
/// and trimmed. An empty result is an error.
pub fn normalize_header(title: &str) -> Result<String, DatalogError> {
    let lower = title.trim().to_lowercase();
    // Remove leading numbering such as "1. " or "2) ".
    let trimmed = lower.trim_start_matches(|c: char| c.is_ascii_digit());
    let stripped = if trimmed.len() != lower.len() {
        trimmed.trim_start_matches(|c: char| c == '.' || c == ')' || c == '-' || c.is_whitespace())
    } else {
        lower.as_str()
    };
    let kept: String = stripped
        .chars()
        .filter(|c| c.is_alphabetic() || c.is_numeric() || c.is_whitespace() || *c == '-')
        .collect();
    let mut out = String::new();
    for word in kept.split_whitespace() {
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(word);
    }
    let mut collapsed = String::new();
    for c in out.chars() {
        if !(c == '-' && collapsed.ends_with('-')) {
            collapsed.push(c);
        }
    }
    let result = collapsed.trim_matches('-').to_owned();
    if result.is_empty() {
        return Err(DatalogError::Type(format!("Markdown header normalizes to an empty key: {title}")));
    }
    Ok(result)
}

/// An ATX header line (`#` to `######`, then a space). Returns `(level, title)`.
fn atx_header(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
        Some((hashes, line[hashes..].trim().trim_end_matches('#').trim_end()))
    } else {
        None
    }
}

/// Parses Markdown into the nested-map document Datalevin stores for `:markdown` idoc values.
///
/// Headers become keys (see `normalize_header`) and nest by level. A section's text becomes a string under its
/// header's key; a section with subheaders holds a map instead. Text before the first header, a section with both
/// text and subheaders, a repeated key under one parent, and an empty header are errors. Inline formatting is
/// not interpreted: the text is the plain text of each non-blank line, joined by newlines.
pub fn idoc_value_markdown(text: &str) -> Result<Value, DatalogError> {
    fn ensure_map<'a>(root: &'a mut Json, path: &[String]) -> Result<&'a mut Json, DatalogError> {
        let mut cur = root;
        for key in path {
            let obj = cur
                .as_object_mut()
                .ok_or_else(|| DatalogError::Type("Markdown header has both content and subheaders".into()))?;
            cur = obj.entry(key.clone()).or_insert_with(|| Json::Object(serde_json::Map::new()));
        }
        Ok(cur)
    }
    fn flush(root: &mut Json, path: &[String], lines: &mut Vec<String>) -> Result<(), DatalogError> {
        let content = lines.join("\n");
        lines.clear();
        if path.is_empty() {
            if !content.trim().is_empty() {
                return Err(DatalogError::Type("Markdown content appears before any header".into()));
            }
            return Ok(());
        }
        if content.trim().is_empty() {
            return Ok(());
        }
        let (last, parent) = path.split_last().expect("non-empty path");
        let parent_map = ensure_map(root, parent)?
            .as_object_mut()
            .ok_or_else(|| DatalogError::Type("Markdown header has both content and subheaders".into()))?;
        if parent_map.get(last).is_some_and(|v| v.is_object()) {
            return Err(DatalogError::Type("Markdown header has both content and subheaders".into()));
        }
        parent_map.insert(last.clone(), Json::String(content));
        Ok(())
    }

    let mut root = Json::Object(serde_json::Map::new());
    let mut path: Vec<String> = Vec::new();
    let mut levels: Vec<usize> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for raw in text.lines() {
        if let Some((level, title)) = atx_header(raw.trim_start()) {
            flush(&mut root, &path, &mut lines)?;
            while levels.last().is_some_and(|&l| l >= level) {
                levels.pop();
                path.pop();
            }
            let key = normalize_header(title)?;
            let parent = ensure_map(&mut root, &path)?;
            let parent_map = parent
                .as_object_mut()
                .ok_or_else(|| DatalogError::Type("Markdown header has both content and subheaders".into()))?;
            if parent_map.contains_key(&key) {
                return Err(DatalogError::Type(format!("Markdown header collision after normalization: {title}")));
            }
            path.push(key);
            levels.push(level);
        } else if !raw.trim().is_empty() {
            if path.is_empty() {
                return Err(DatalogError::Type("Markdown content appears before any header".into()));
            }
            lines.push(raw.trim().to_owned());
        }
    }
    flush(&mut root, &path, &mut lines)?;
    let canonical = serde_json::to_string(&root).map_err(|e| DatalogError::Type(e.to_string()))?;
    Ok(Value::Doc(canonical))
}
