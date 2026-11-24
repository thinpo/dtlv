pub mod parser;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use dtlv_codec::{
    decode_ave_entity, decode_value, encode_av_prefix, encode_ave_key, encode_ea_prefix,
    encode_eav_key, encode_vae_key, AttrId, CodecError, EntityId, Value,
};
use dtlv_kv::{KvError, KvStore, RawDb, ReadTxn, WriteTxn};
use dtlv_search::{FullTextIndex, Metric, VectorIndex};
use parser::{
    parse_query, parse_query_template, parse_rules, AggOp, Clause, CmpOp, ExprTerm, FindElem,
    FindSpec, OrderKey, ParseError, ParsedQuery, RuleDef,
};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    One,
    Many,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unique {
    Identity,
    Value,
}

#[derive(Debug, Clone)]
pub struct AttrSpec {
    pub name: String,
    pub cardinality: Cardinality,
    pub unique: Option<Unique>,
    pub index: bool,
    pub fulltext: bool,
    pub is_component: bool,
}

impl AttrSpec {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            cardinality: Cardinality::One,
            unique: None,
            index: false,
            fulltext: false,
            is_component: false,
        }
    }

    pub fn unique_identity(mut self) -> Self {
        self.unique = Some(Unique::Identity);
        self.index = true;
        self
    }

    pub fn unique_value(mut self) -> Self {
        self.unique = Some(Unique::Value);
        self.index = true;
        self
    }

    pub fn indexed(mut self) -> Self {
        self.index = true;
        self
    }

    pub fn fulltext(mut self) -> Self {
        self.fulltext = true;
        self
    }

    pub fn many(mut self) -> Self {
        self.cardinality = Cardinality::Many;
        self
    }

    pub fn component(mut self) -> Self {
        self.is_component = true;
        self
    }
}

#[derive(Debug, Error)]
pub enum DatalogError {
    #[error("kv error: {0}")]
    Kv(#[from] KvError),
    #[error("codec error: {0}")]
    Codec(#[from] CodecError),
    #[error("unknown attribute: {0}")]
    UnknownAttr(String),
    #[error("unique constraint violation on {attr} for entity {existing}")]
    UniqueViolation { attr: String, existing: EntityId },
    #[error("unbound query variable: {0}")]
    UnboundVar(String),
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("unknown rule: {0}")]
    UnknownRule(String),
    #[error("compare-and-swap failed for entity {eid} attr {attr}")]
    CasFailed { eid: EntityId, attr: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    Rel(Vec<Vec<Value>>),
    Scalar(Option<Value>),
    Coll(Vec<Value>),
    Tuple(Option<Vec<Value>>),
    Maps(Vec<BTreeMap<String, Value>>),
    PulledColl(Vec<BTreeMap<String, PullValue>>),
}

#[derive(Debug, Clone)]
pub enum EntityRef {
    New,
    Id(EntityId),
    TempId(String),
    LookupRef(String, Value),
}

#[derive(Debug, Clone)]
pub enum TxVal {
    Val(Value),
    TempRef(String),
    LookupRef(String, Value),
}

pub enum TxItem {
    /// Assert a map of (attr_name, Value) pairs. If `:db/id` is omitted, allocates or upserts.
    Assert(Option<EntityId>, Vec<(String, Value)>),
    /// Assert with tempid / lookup-ref resolution for both entity and ref values.
    AssertRef(EntityRef, Vec<(String, TxVal)>),
    Retract(EntityId, String, Value),
    RetractEntity(EntityId),
    /// Atomic compare-and-swap: replace `expected` with `new_val` on `(eid, attr)`.
    Cas(EntityId, String, Value, Value),
}

#[derive(Debug, Clone)]
pub enum PullSelector {
    All,
    Attr(String),
    Reverse(String),
    Nested(String, Vec<PullSelector>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PullValue {
    Single(Value),
    Many(Vec<Value>),
    Map(BTreeMap<String, PullValue>),
    ManyMaps(Vec<BTreeMap<String, PullValue>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    Eavt,
    Avet,
    Vaet,
}

#[derive(Debug, Clone)]
pub enum Term {
    Var(String),
    Val(Value),
    Entity(EntityId),
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Datom(Term, String, Term),
    Fulltext(String, String), // (query_text, entity_var)
}

#[derive(Debug, Clone)]
pub enum Pred {
    Le(String, String),
    Lt(String, String),
    Ge(String, String),
    Gt(String, String),
}

#[derive(Debug, Clone)]
pub struct Query {
    pub find: Vec<String>,
    pub patterns: Vec<Pattern>,
    pub preds: Vec<Pred>,
}

const NEXT_EID_KEY: &[u8] = b"__next_eid";
const NEXT_AID_KEY: &[u8] = b"__next_aid";

pub struct Conn {
    store: KvStore,
    eav: RawDb,
    ave: RawDb,
    vae: RawDb,
    meta: RawDb,
    fts: FullTextIndex,
    vec_idx: VectorIndex,
    attr_ids: HashMap<String, AttrId>,
    aid_names: HashMap<AttrId, String>,
    attrs: HashMap<AttrId, AttrSpec>,
}

impl Conn {
    /// Open or create an LMDB-backed Datalog connection at `path`.
    pub fn open<P: AsRef<Path>>(path: P, schema: Vec<AttrSpec>) -> Result<Self, DatalogError> {
        let store = KvStore::open(path, 512 * 1024 * 1024, 16)?;
        Self::init_from_store(store, schema)
    }

    /// Open or create a single-file SQLite-backed Datalog connection at `path` (WAL mode).
    pub fn open_sqlite<P: AsRef<Path>>(path: P, schema: Vec<AttrSpec>) -> Result<Self, DatalogError> {
        let store = KvStore::open_sqlite(path)?;
        Self::init_from_store(store, schema)
    }

    /// Open or create a pure-Rust single-file `redb`-backed Datalog connection at `path`.
    pub fn open_redb<P: AsRef<Path>>(path: P, schema: Vec<AttrSpec>) -> Result<Self, DatalogError> {
        let store = KvStore::open_redb(path)?;
        Self::init_from_store(store, schema)
    }

    /// Create an ephemeral pure-Rust in-memory (`BTreeMap`) Datalog connection.
    pub fn open_mem(schema: Vec<AttrSpec>) -> Result<Self, DatalogError> {
        Self::init_from_store(KvStore::open_mem(), schema)
    }

    fn init_from_store(store: KvStore, schema: Vec<AttrSpec>) -> Result<Self, DatalogError> {
        let mut wtxn = store.write_txn()?;
        let eav = store.open_db(&mut wtxn, "eav")?;
        let ave = store.open_db(&mut wtxn, "ave")?;
        let vae = store.open_db(&mut wtxn, "vae")?;
        let meta = store.open_db(&mut wtxn, "meta")?;
        let fts = FullTextIndex::open(&store, &mut wtxn)?;
        let vec_idx = VectorIndex::open(&store, &mut wtxn, "vec_idx", Metric::Cosine)?;

        if meta.get_rw(&wtxn, NEXT_EID_KEY)?.is_none() {
            meta.put(&mut wtxn, NEXT_EID_KEY, &1u64.to_be_bytes())?;
        }
        let mut next_aid: AttrId = match meta.get_rw(&wtxn, NEXT_AID_KEY)? {
            Some(b) if b.len() == 4 => u32::from_be_bytes(b.as_ref().try_into().unwrap()),
            _ => 1,
        };

        let mut attr_ids = HashMap::new();
        let mut aid_names = HashMap::new();
        let mut attrs = HashMap::new();

        // Load existing attr_name -> aid mappings from meta ("attr:<name>")
        for item in meta.prefix_iter_rw(&wtxn, b"attr:")? {
            let (k, v) = item?;
            if v.len() == 4 {
                let name = std::str::from_utf8(&k[5..])
                    .map_err(|_| CodecError::InvalidUtf8)?
                    .to_owned();
                let aid = u32::from_be_bytes(v.as_ref().try_into().unwrap());
                attr_ids.insert(name.clone(), aid);
                aid_names.insert(aid, name);
            }
        }

        for spec in schema {
            let aid = if let Some(&existing) = attr_ids.get(&spec.name) {
                existing
            } else {
                let aid = next_aid;
                next_aid += 1;
                let mut key = Vec::with_capacity(5 + spec.name.len());
                key.extend_from_slice(b"attr:");
                key.extend_from_slice(spec.name.as_bytes());
                meta.put(&mut wtxn, &key, &aid.to_be_bytes())?;
                attr_ids.insert(spec.name.clone(), aid);
                aid_names.insert(aid, spec.name.clone());
                aid
            };
            attrs.insert(aid, spec);
        }
        meta.put(&mut wtxn, NEXT_AID_KEY, &next_aid.to_be_bytes())?;
        wtxn.commit()?;

        Ok(Self {
            store,
            eav,
            ave,
            vae,
            meta,
            fts,
            vec_idx,
            attr_ids,
            aid_names,
            attrs,
        })
    }

    /// Persist a consistent snapshot of `meta`, `eav`, `ave`, `vae`, and `vec_idx`
    /// from this connection into a portable SQLite file at `sqlite_path`.
    pub fn save_to_sqlite<P: AsRef<Path>>(
        &self,
        rtxn: &ReadTxn<'_>,
        sqlite_path: P,
    ) -> Result<(), DatalogError> {
        let dst = KvStore::open_sqlite(sqlite_path)?;
        let mut wtxn = dst.write_txn()?;
        for (dbi_name, s_db) in [
            ("meta", &self.meta),
            ("eav", &self.eav),
            ("ave", &self.ave),
            ("vae", &self.vae),
        ] {
            let dst_db = dst.open_db(&mut wtxn, dbi_name)?;
            for item in s_db.iter(rtxn)? {
                let (k, v) = item?;
                dst_db.put(&mut wtxn, k.as_ref(), v.as_ref())?;
            }
        }
        let dst_vec = dst.open_db(&mut wtxn, "vec_idx")?;
        for (eid, vec) in self.vec_idx.dump_vecs(rtxn)? {
            let mut bytes = Vec::with_capacity(vec.len() * 4);
            for f in vec {
                bytes.extend_from_slice(&f.to_le_bytes());
            }
            dst_vec.put(&mut wtxn, &eid.to_be_bytes(), &bytes)?;
        }
        wtxn.commit()?;
        dst.sync()?;
        Ok(())
    }

    /// Restore an LMDB-backed `Conn` at `lmdb_dir` from a SQLite snapshot at `sqlite_path`.
    pub fn load_from_sqlite<P: AsRef<Path>, Q: AsRef<Path>>(
        lmdb_dir: P,
        sqlite_path: Q,
        schema: Vec<AttrSpec>,
    ) -> Result<Self, DatalogError> {
        let src = Self::open_sqlite(sqlite_path, schema.clone())?;
        let srtxn = src.read_txn()?;
        let dst = Self::open(lmdb_dir, schema)?;
        let mut wtxn = dst.store.write_txn()?;
        for (src_db, dst_db) in [
            (&src.meta, &dst.meta),
            (&src.eav, &dst.eav),
            (&src.ave, &dst.ave),
            (&src.vae, &dst.vae),
        ] {
            for item in src_db.iter(&srtxn)? {
                let (k, v) = item?;
                dst_db.put(&mut wtxn, k.as_ref(), v.as_ref())?;
            }
        }
        wtxn.commit()?;
        for (eid, vec) in src.vec_idx.dump_vecs(&srtxn)? {
            dst.add_vec(eid, &vec)?;
        }
        // Rebuild FTS index from EAV for all fulltext attributes
        let datoms = dst.datoms(&dst.read_txn()?, IndexKind::Eavt)?;
        let mut fwtxn = dst.store.write_txn()?;
        for (eid, attr_name, val) in datoms {
            if let Ok(aid) = dst.attr_id(&attr_name) {
                if dst.attrs[&aid].fulltext {
                    if let Some(txt) = val.as_str() {
                        dst.fts.index_doc(&mut fwtxn, eid, txt)?;
                    }
                }
            }
        }
        fwtxn.commit()?;
        Ok(dst)
    }

    #[inline]
    pub fn read_txn(&self) -> Result<ReadTxn<'_>, DatalogError> {
        Ok(self.store.read_txn()?)
    }

    #[inline]
    pub fn attr_id(&self, name: &str) -> Result<AttrId, DatalogError> {
        self.attr_ids
            .get(name)
            .copied()
            .ok_or_else(|| DatalogError::UnknownAttr(name.to_owned()))
    }

    /// Zero-allocation fast lookup of EntityId by (attr, value) via AVE index.
    #[inline]
    pub fn lookup_entity_by_av(
        &self,
        rtxn: &ReadTxn<'_>,
        attr: &str,
        val: &Value,
    ) -> Result<Option<EntityId>, DatalogError> {
        thread_local! {
            static PREF_BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::with_capacity(64));
        }
        let aid = self.attr_id(attr)?;
        PREF_BUF.with(|cell| {
            let mut pref = cell.borrow_mut();
            encode_av_prefix(aid, val, &mut pref);
            if let Some(res) = self.ave.prefix_iter(rtxn, &pref)?.next() {
                let (k, _) = res?;
                return Ok(Some(decode_ave_entity(&k)?));
            }
            Ok(None)
        })
    }

    /// Direct EAV attribute read for a known EntityId.
    #[inline]
    pub fn get_attr(
        &self,
        rtxn: &ReadTxn<'_>,
        eid: EntityId,
        attr: &str,
    ) -> Result<Option<Value>, DatalogError> {
        let aid = self.attr_id(attr)?;
        let mut pref = [0u8; 12];
        pref[..8].copy_from_slice(&eid.to_be_bytes());
        pref[8..12].copy_from_slice(&aid.to_be_bytes());
        if let Some(res) = self.eav.prefix_iter(rtxn, &pref)?.next() {
            let (k, _) = res?;
            return Ok(Some(decode_value(&k[12..])?));
        }
        Ok(None)
    }

    /// Alias for `lookup_entity_by_av` (matches Datalevin `d/entid`).
    #[inline]
    pub fn entid(
        &self,
        rtxn: &ReadTxn<'_>,
        attr: &str,
        val: &Value,
    ) -> Result<Option<EntityId>, DatalogError> {
        self.lookup_entity_by_av(rtxn, attr, val)
    }

    pub fn sync(&self) -> Result<(), DatalogError> {
        Ok(self.store.sync()?)
    }

    /// Return all datoms `(eid, attr_name, value)` from the specified index.
    pub fn datoms(
        &self,
        rtxn: &ReadTxn<'_>,
        index: IndexKind,
    ) -> Result<Vec<(EntityId, String, Value)>, DatalogError> {
        let mut out = Vec::new();
        match index {
            IndexKind::Eavt => {
                for item in self.eav.iter(rtxn)? {
                    let (k, _) = item?;
                    if k.len() < 12 {
                        continue;
                    }
                    let eid = u64::from_be_bytes(k[0..8].try_into().unwrap());
                    let aid = u32::from_be_bytes(k[8..12].try_into().unwrap());
                    let val = decode_value(&k[12..])?;
                    if let Some(name) = self.aid_names.get(&aid) {
                        out.push((eid, name.clone(), val));
                    }
                }
            }
            IndexKind::Avet => {
                for item in self.ave.iter(rtxn)? {
                    let (k, _) = item?;
                    if k.len() < 14 {
                        continue;
                    }
                    let aid = u32::from_be_bytes(k[0..4].try_into().unwrap());
                    let eid = decode_ave_entity(&k)?;
                    let (raw_val, _) = dtlv_codec::decode_escaped_bytes(&k[4..k.len() - 8])?;
                    let val = decode_value(&raw_val)?;
                    if let Some(name) = self.aid_names.get(&aid) {
                        out.push((eid, name.clone(), val));
                    }
                }
            }
            IndexKind::Vaet => {
                for item in self.vae.iter(rtxn)? {
                    let (k, _) = item?;
                    if k.len() != 20 {
                        continue;
                    }
                    let target = u64::from_be_bytes(k[0..8].try_into().unwrap());
                    let aid = u32::from_be_bytes(k[8..12].try_into().unwrap());
                    let src = u64::from_be_bytes(k[12..20].try_into().unwrap());
                    if let Some(name) = self.aid_names.get(&aid) {
                        out.push((src, name.clone(), Value::Ref(target)));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Bounded AVE range scan `[start..=end]` for an indexed attribute.
    pub fn index_range(
        &self,
        rtxn: &ReadTxn<'_>,
        attr: &str,
        start: &Value,
        end: &Value,
    ) -> Result<Vec<(EntityId, String, Value)>, DatalogError> {
        let aid = self.attr_id(attr)?;
        let mut s_key = Vec::with_capacity(32);
        let mut e_key = Vec::with_capacity(32);
        encode_av_prefix(aid, start, &mut s_key);
        encode_av_prefix(aid, end, &mut e_key);
        e_key.extend_from_slice(&u64::MAX.to_be_bytes());
        let mut out = Vec::new();
        for item in self
            .ave
            .range(rtxn, &(std::ops::Bound::Included(s_key.as_slice()), std::ops::Bound::Included(e_key.as_slice())))?
        {
            let (k, _) = item?;
            let eid = decode_ave_entity(&k)?;
            let (raw_val, _) = dtlv_codec::decode_escaped_bytes(&k[4..k.len() - 8])?;
            let val = decode_value(&raw_val)?;
            out.push((eid, attr.to_owned(), val));
        }
        Ok(out)
    }

    /// Seek AVE datoms for `attr` starting at `>= start`.
    pub fn seek_datoms(
        &self,
        rtxn: &ReadTxn<'_>,
        attr: &str,
        start: &Value,
    ) -> Result<Vec<(EntityId, String, Value)>, DatalogError> {
        let aid = self.attr_id(attr)?;
        let mut s_key = Vec::with_capacity(32);
        encode_av_prefix(aid, start, &mut s_key);
        let mut end_prefix = aid.to_be_bytes().to_vec();
        end_prefix.extend_from_slice(&[0xff; 8]);
        let mut out = Vec::new();
        for item in self.ave.range(
            rtxn,
            &(
                std::ops::Bound::Included(s_key.as_slice()),
                std::ops::Bound::Unbounded,
            ),
        )? {
            let (k, _) = item?;
            if k.len() < 14 || u32::from_be_bytes(k[0..4].try_into().unwrap()) != aid {
                break;
            }
            let eid = decode_ave_entity(&k)?;
            let (raw_val, _) = dtlv_codec::decode_escaped_bytes(&k[4..k.len() - 8])?;
            let val = decode_value(&raw_val)?;
            out.push((eid, attr.to_owned(), val));
        }
        Ok(out)
    }

    /// Count entities matching `(attr, val)` via AVE index.
    pub fn count_datoms(
        &self,
        rtxn: &ReadTxn<'_>,
        attr: &str,
        val: &Value,
    ) -> Result<u64, DatalogError> {
        let aid = self.attr_id(attr)?;
        let mut pref = Vec::with_capacity(32);
        encode_av_prefix(aid, val, &mut pref);
        let mut cnt = 0u64;
        for item in self.ave.prefix_iter(rtxn, &pref)? {
            let _ = item?;
            cnt += 1;
        }
        Ok(cnt)
    }

    pub fn add_vec(&self, eid: EntityId, vec: &[f32]) -> Result<(), DatalogError> {
        let mut wtxn = self.store.write_txn()?;
        self.vec_idx.add_vec(&mut wtxn, eid, vec)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn remove_vec(&self, eid: EntityId) -> Result<(), DatalogError> {
        let mut wtxn = self.store.write_txn()?;
        self.vec_idx.remove_vec(&mut wtxn, eid)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn search_vec(
        &self,
        rtxn: &ReadTxn<'_>,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(EntityId, f32)>, DatalogError> {
        Ok(self.vec_idx.search_vec(rtxn, query, k)?)
    }

    /// Get all values for `(eid, attr)` (useful for `:db.cardinality/many`).
    pub fn get_attr_many(
        &self,
        rtxn: &ReadTxn<'_>,
        eid: EntityId,
        attr: &str,
    ) -> Result<Vec<Value>, DatalogError> {
        let aid = self.attr_id(attr)?;
        let mut pref = [0u8; 12];
        pref[..8].copy_from_slice(&eid.to_be_bytes());
        pref[8..12].copy_from_slice(&aid.to_be_bytes());
        let mut out = Vec::new();
        for res in self.eav.prefix_iter(rtxn, &pref)? {
            let (k, _) = res?;
            out.push(decode_value(&k[12..])?);
        }
        Ok(out)
    }

    /// Pull a map of requested attributes for an EntityId.
    pub fn pull(
        &self,
        rtxn: &ReadTxn<'_>,
        eid: EntityId,
        attr_names: &[&str],
    ) -> Result<BTreeMap<String, Value>, DatalogError> {
        let mut out = BTreeMap::new();
        for &name in attr_names {
            if let Some(v) = self.get_attr(rtxn, eid, name)? {
                out.insert(name.to_owned(), v);
            }
        }
        Ok(out)
    }

    /// Full Datalevin-style pull supporting `*`, `:db.cardinality/many`, reverse `:_attr`, and nested refs.
    pub fn pull_pattern(
        &self,
        rtxn: &ReadTxn<'_>,
        eid: EntityId,
        selectors: &[PullSelector],
    ) -> Result<BTreeMap<String, PullValue>, DatalogError> {
        let mut out = BTreeMap::new();
        for sel in selectors {
            match sel {
                PullSelector::All => {
                    let prefix = eid.to_be_bytes();
                    let mut grouped: BTreeMap<AttrId, Vec<Value>> = BTreeMap::new();
                    for item in self.eav.prefix_iter(rtxn, &prefix)? {
                        let (k, _) = item?;
                        if k.len() < 12 {
                            continue;
                        }
                        let aid = u32::from_be_bytes(k[8..12].try_into().unwrap());
                        let val = decode_value(&k[12..])?;
                        grouped.entry(aid).or_default().push(val);
                    }
                    for (aid, mut vals) in grouped {
                        if let (Some(name), Some(spec)) = (self.aid_names.get(&aid), self.attrs.get(&aid)) {
                            if spec.cardinality == Cardinality::Many {
                                out.insert(name.clone(), PullValue::Many(vals));
                            } else if let Some(v) = vals.pop() {
                                out.insert(name.clone(), PullValue::Single(v));
                            }
                        }
                    }
                }
                PullSelector::Attr(name) => {
                    let aid = self.attr_id(name)?;
                    let spec = &self.attrs[&aid];
                    let vals = self.get_attr_many(rtxn, eid, name)?;
                    if !vals.is_empty() {
                        if spec.cardinality == Cardinality::Many {
                            out.insert(name.clone(), PullValue::Many(vals));
                        } else {
                            out.insert(name.clone(), PullValue::Single(vals.into_iter().next().unwrap()));
                        }
                    }
                }
                PullSelector::Reverse(attr_name) => {
                    let aid = self.attr_id(attr_name)?;
                    let mut pref = [0u8; 12];
                    pref[..8].copy_from_slice(&eid.to_be_bytes());
                    pref[8..12].copy_from_slice(&aid.to_be_bytes());
                    let mut refs = Vec::new();
                    for item in self.vae.prefix_iter(rtxn, &pref)? {
                        let (k, _) = item?;
                        if k.len() == 20 {
                            let src = u64::from_be_bytes(k[12..20].try_into().unwrap());
                            refs.push(Value::Ref(src));
                        }
                    }
                    if !refs.is_empty() {
                        out.insert(format!(":_{}", attr_name.trim_start_matches(':')), PullValue::Many(refs));
                    }
                }
                PullSelector::Nested(name, sub) => {
                    let aid = self.attr_id(name)?;
                    let spec = &self.attrs[&aid];
                    let vals = self.get_attr_many(rtxn, eid, name)?;
                    let mut maps = Vec::new();
                    for v in vals {
                        if let Value::Ref(child_id) = v {
                            maps.push(self.pull_pattern(rtxn, child_id, sub)?);
                        }
                    }
                    if !maps.is_empty() {
                        if spec.cardinality == Cardinality::Many {
                            out.insert(name.clone(), PullValue::ManyMaps(maps));
                        } else {
                            out.insert(name.clone(), PullValue::Map(maps.into_iter().next().unwrap()));
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn alloc_eid(&self, wtxn: &mut WriteTxn<'_>) -> Result<EntityId, DatalogError> {
        let next = match self.meta.get_rw(wtxn, NEXT_EID_KEY)? {
            Some(b) if b.len() == 8 => u64::from_be_bytes(b.as_ref().try_into().unwrap()),
            _ => 1,
        };
        self.meta
            .put(wtxn, NEXT_EID_KEY, &(next + 1).to_be_bytes())?;
        Ok(next)
    }

    fn lookup_ave_rw(
        &self,
        wtxn: &WriteTxn<'_>,
        aid: AttrId,
        val: &Value,
        buf: &mut Vec<u8>,
    ) -> Result<Option<EntityId>, DatalogError> {
        encode_av_prefix(aid, val, buf);
        if let Some(res) = self.ave.prefix_iter_rw(wtxn, buf)?.next() {
            let (k, _) = res?;
            return Ok(Some(decode_ave_entity(&k)?));
        }
        Ok(None)
    }

    fn remove_ea_cardinality_one(
        &self,
        wtxn: &mut WriteTxn<'_>,
        eid: EntityId,
        aid: AttrId,
        spec: &AttrSpec,
        kbuf: &mut Vec<u8>,
        ave_buf: &mut Vec<u8>,
    ) -> Result<(), DatalogError> {
        encode_ea_prefix(eid, aid, kbuf);
        let existing: Vec<Vec<u8>> = self
            .eav
            .prefix_iter_rw(wtxn, kbuf)?
            .map(|r| r.map(|(k, _)| k.to_vec()))
            .collect::<Result<_, _>>()?;
        for old_eav in existing {
            let old_val = decode_value(&old_eav[12..])?;
            self.eav.delete(wtxn, &old_eav)?;
            if spec.index {
                encode_ave_key(aid, &old_val, eid, ave_buf);
                self.ave.delete(wtxn, ave_buf)?;
            }
            if let Value::Ref(target) = old_val {
                encode_vae_key(target, aid, eid, ave_buf);
                self.vae.delete(wtxn, ave_buf)?;
            }
        }
        Ok(())
    }

    fn retract_entity_recursive(
        &self,
        wtxn: &mut WriteTxn<'_>,
        eid: EntityId,
        ave_buf: &mut Vec<u8>,
    ) -> Result<(), DatalogError> {
        let prefix = eid.to_be_bytes();
        let entries: Vec<Vec<u8>> = self
            .eav
            .prefix_iter_rw(wtxn, &prefix)?
            .map(|r| r.map(|(k, _)| k.to_vec()))
            .collect::<Result<_, _>>()?;
        let mut component_children = Vec::new();
        let mut had_fulltext = false;
        for k in entries {
            let aid = u32::from_be_bytes(k[8..12].try_into().unwrap());
            let val = decode_value(&k[12..])?;
            self.eav.delete(wtxn, &k)?;
            if let Some(spec) = self.attrs.get(&aid) {
                if spec.index {
                    encode_ave_key(aid, &val, eid, ave_buf);
                    self.ave.delete(wtxn, ave_buf)?;
                }
                if spec.fulltext {
                    had_fulltext = true;
                }
                if spec.is_component {
                    if let Value::Ref(child) = val {
                        component_children.push(child);
                    }
                }
            }
            if let Value::Ref(target) = val {
                encode_vae_key(target, aid, eid, ave_buf);
                self.vae.delete(wtxn, ave_buf)?;
            }
        }
        if had_fulltext {
            self.fts.remove_doc(wtxn, eid)?;
        }
        for child in component_children {
            self.retract_entity_recursive(wtxn, child, ave_buf)?;
        }
        Ok(())
    }

    pub fn transact(&self, items: &[TxItem]) -> Result<Vec<EntityId>, DatalogError> {
        let mut wtxn = self.store.write_txn()?;
        let mut eav_buf = Vec::with_capacity(64);
        let mut ave_buf = Vec::with_capacity(64);
        let mut touched = Vec::with_capacity(items.len());
        let mut tempids: HashMap<String, EntityId> = HashMap::new();

        for item in items {
            match item {
                TxItem::Assert(explicit_eid, pairs) => {
                    let mut resolved_eid = *explicit_eid;
                    if resolved_eid.is_none() {
                        for (attr_name, val) in pairs {
                            let aid = self.attr_id(attr_name)?;
                            let spec = &self.attrs[&aid];
                            if spec.unique == Some(Unique::Identity) {
                                if let Some(found) =
                                    self.lookup_ave_rw(&wtxn, aid, val, &mut ave_buf)?
                                {
                                    resolved_eid = Some(found);
                                    break;
                                }
                            }
                        }
                    }
                    let eid = match resolved_eid {
                        Some(id) => id,
                        None => self.alloc_eid(&mut wtxn)?,
                    };
                    touched.push(eid);

                    for (attr_name, val) in pairs {
                        self.assert_datom(&mut wtxn, eid, attr_name, val, &mut eav_buf, &mut ave_buf)?;
                    }
                }
                TxItem::AssertRef(eref, pairs) => {
                    let eid = match eref {
                        EntityRef::Id(id) => *id,
                        EntityRef::TempId(tid) => {
                            if let Some(&id) = tempids.get(tid) {
                                id
                            } else {
                                let id = self.alloc_eid(&mut wtxn)?;
                                tempids.insert(tid.clone(), id);
                                id
                            }
                        }
                        EntityRef::LookupRef(attr_name, val) => {
                            let aid = self.attr_id(attr_name)?;
                            if let Some(found) = self.lookup_ave_rw(&wtxn, aid, val, &mut ave_buf)? {
                                found
                            } else {
                                self.alloc_eid(&mut wtxn)?
                            }
                        }
                        EntityRef::New => {
                            let mut found_id = None;
                            for (attr_name, txv) in pairs {
                                if let TxVal::Val(val) = txv {
                                    let aid = self.attr_id(attr_name)?;
                                    if self.attrs[&aid].unique == Some(Unique::Identity) {
                                        if let Some(f) = self.lookup_ave_rw(&wtxn, aid, val, &mut ave_buf)? {
                                            found_id = Some(f);
                                            break;
                                        }
                                    }
                                }
                            }
                            match found_id {
                                Some(id) => id,
                                None => self.alloc_eid(&mut wtxn)?,
                            }
                        }
                    };
                    touched.push(eid);

                    for (attr_name, txv) in pairs {
                        let resolved_val = match txv {
                            TxVal::Val(v) => v.clone(),
                            TxVal::TempRef(tid) => {
                                let target = if let Some(&id) = tempids.get(tid) {
                                    id
                                } else {
                                    let id = self.alloc_eid(&mut wtxn)?;
                                    tempids.insert(tid.clone(), id);
                                    id
                                };
                                Value::Ref(target)
                            }
                            TxVal::LookupRef(lattr, lval) => {
                                let laid = self.attr_id(lattr)?;
                                let target = match self.lookup_ave_rw(&wtxn, laid, lval, &mut ave_buf)? {
                                    Some(id) => id,
                                    None => self.alloc_eid(&mut wtxn)?,
                                };
                                Value::Ref(target)
                            }
                        };
                        self.assert_datom(&mut wtxn, eid, attr_name, &resolved_val, &mut eav_buf, &mut ave_buf)?;
                    }
                }
                TxItem::Retract(eid, attr_name, val) => {
                    let aid = self.attr_id(attr_name)?;
                    let spec = &self.attrs[&aid];
                    encode_eav_key(*eid, aid, val, &mut eav_buf);
                    self.eav.delete(&mut wtxn, &eav_buf)?;
                    if spec.index {
                        encode_ave_key(aid, val, *eid, &mut ave_buf);
                        self.ave.delete(&mut wtxn, &ave_buf)?;
                    }
                    if let Value::Ref(target) = val {
                        encode_vae_key(*target, aid, *eid, &mut ave_buf);
                        self.vae.delete(&mut wtxn, &ave_buf)?;
                    }
                    if spec.fulltext {
                        self.fts.remove_doc(&mut wtxn, *eid)?;
                    }
                }
                TxItem::RetractEntity(eid) => {
                    self.retract_entity_recursive(&mut wtxn, *eid, &mut ave_buf)?;
                }
                TxItem::Cas(eid, attr_name, expected, new_val) => {
                    let aid = self.attr_id(attr_name)?;
                    let mut pref = [0u8; 12];
                    pref[..8].copy_from_slice(&eid.to_be_bytes());
                    pref[8..12].copy_from_slice(&aid.to_be_bytes());
                    let cur = if let Some(res) = self.eav.prefix_iter_rw(&wtxn, &pref)?.next() {
                        let (k, _) = res?;
                        Some(decode_value(&k[12..])?)
                    } else {
                        None
                    };
                    if cur.as_ref() != Some(expected) {
                        return Err(DatalogError::CasFailed {
                            eid: *eid,
                            attr: attr_name.clone(),
                        });
                    }
                    self.assert_datom(&mut wtxn, *eid, attr_name, new_val, &mut eav_buf, &mut ave_buf)?;
                    touched.push(*eid);
                }
            }
        }

        wtxn.commit()?;
        Ok(touched)
    }

    fn assert_datom(
        &self,
        wtxn: &mut WriteTxn<'_>,
        eid: EntityId,
        attr_name: &str,
        val: &Value,
        eav_buf: &mut Vec<u8>,
        ave_buf: &mut Vec<u8>,
    ) -> Result<(), DatalogError> {
        let aid = self.attr_id(attr_name)?;
        let spec = &self.attrs[&aid];

        if let Some(u) = spec.unique {
            if let Some(existing) = self.lookup_ave_rw(wtxn, aid, val, ave_buf)? {
                if existing != eid && u == Unique::Value {
                    return Err(DatalogError::UniqueViolation {
                        attr: attr_name.to_owned(),
                        existing,
                    });
                }
            }
        }

        if spec.cardinality == Cardinality::One {
            self.remove_ea_cardinality_one(wtxn, eid, aid, spec, eav_buf, ave_buf)?;
        }

        encode_eav_key(eid, aid, val, eav_buf);
        self.eav.put(wtxn, eav_buf, &[])?;

        if spec.index {
            encode_ave_key(aid, val, eid, ave_buf);
            self.ave.put(wtxn, ave_buf, &[])?;
        }
        if let Value::Ref(target) = val {
            encode_vae_key(*target, aid, eid, ave_buf);
            self.vae.put(wtxn, ave_buf, &[])?;
        }
        if spec.fulltext {
            if let Some(text) = val.as_str() {
                self.fts.index_doc(wtxn, eid, text)?;
            }
        }
        Ok(())
    }

    pub fn fulltext_search(
        &self,
        rtxn: &ReadTxn<'_>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(EntityId, f64)>, DatalogError> {
        Ok(self.fts.search(rtxn, query, limit)?)
    }

    /// Execute a Datalog query using index-nested-loop joins over EAV/AVE/FTS.
    pub fn q(&self, rtxn: &ReadTxn<'_>, query: &Query) -> Result<Vec<Vec<Value>>, DatalogError> {
        type Env = HashMap<String, Value>;
        let mut envs: Vec<Env> = vec![HashMap::new()];
        let mut buf = Vec::with_capacity(64);

        for pat in &query.patterns {
            let mut next_envs = Vec::new();
            match pat {
                Pattern::Fulltext(q_str, e_var) => {
                    let hits = self.fts.search(rtxn, q_str, 100)?;
                    for env in &envs {
                        for &(eid, _) in &hits {
                            let mut e2 = env.clone();
                            if let Some(bound) = e2.get(e_var) {
                                if bound != &Value::Ref(eid) {
                                    continue;
                                }
                            } else {
                                e2.insert(e_var.clone(), Value::Ref(eid));
                            }
                            next_envs.push(e2);
                        }
                    }
                }
                Pattern::Datom(e_term, attr_name, v_term) => {
                    let aid = self.attr_id(attr_name)?;
                    let spec = &self.attrs[&aid];

                    for env in &envs {
                        let bound_e: Option<EntityId> = match e_term {
                            Term::Entity(id) => Some(*id),
                            Term::Var(var) => env.get(var).and_then(|v| v.as_ref()),
                            Term::Val(v) => v.as_ref(),
                        };
                        let bound_v: Option<&Value> = match v_term {
                            Term::Val(v) => Some(v),
                            Term::Entity(id) => {
                                // Handled via Ref
                                let _ = id;
                                None
                            }
                            Term::Var(var) => env.get(var),
                        };

                        match (bound_e, bound_v) {
                            (Some(eid), Some(val)) => {
                                encode_eav_key(eid, aid, val, &mut buf);
                                if self.eav.get(rtxn, &buf)?.is_some() {
                                    next_envs.push(env.clone());
                                }
                            }
                            (Some(eid), None) => {
                                encode_ea_prefix(eid, aid, &mut buf);
                                for item in self.eav.prefix_iter(rtxn, &buf)? {
                                    let (k, _) = item?;
                                    let val = decode_value(&k[12..])?;
                                    let mut e2 = env.clone();
                                    if let Term::Var(vname) = v_term {
                                        e2.insert(vname.clone(), val);
                                    }
                                    next_envs.push(e2);
                                }
                            }
                            (None, Some(val)) if spec.index => {
                                encode_av_prefix(aid, val, &mut buf);
                                for item in self.ave.prefix_iter(rtxn, &buf)? {
                                    let (k, _) = item?;
                                    let eid = decode_ave_entity(&k)?;
                                    let mut e2 = env.clone();
                                    if let Term::Var(ename) = e_term {
                                        e2.insert(ename.clone(), Value::Ref(eid));
                                    }
                                    next_envs.push(e2);
                                }
                            }
                            (None, _) => {
                                // Full EAV scan filtered by aid
                                for item in self.eav.iter(rtxn)? {
                                    let (k, _) = item?;
                                    if k.len() < 12 {
                                        continue;
                                    }
                                    let k_aid = u32::from_be_bytes(k[8..12].try_into().unwrap());
                                    if k_aid != aid {
                                        continue;
                                    }
                                    let eid = u64::from_be_bytes(k[0..8].try_into().unwrap());
                                    let val = decode_value(&k[12..])?;
                                    if let Some(bv) = bound_v {
                                        if &val != bv {
                                            continue;
                                        }
                                    }
                                    let mut e2 = env.clone();
                                    if let Term::Var(ename) = e_term {
                                        e2.insert(ename.clone(), Value::Ref(eid));
                                    }
                                    if let Term::Var(vname) = v_term {
                                        e2.insert(vname.clone(), val);
                                    }
                                    next_envs.push(e2);
                                }
                            }
                        }
                    }
                }
            }
            envs = next_envs;
        }

        // Apply predicates
        envs.retain(|env| {
            query.preds.iter().all(|p| match p {
                Pred::Le(a, b) => cmp_vars(env, a, b).is_some_and(|o| o != std::cmp::Ordering::Greater),
                Pred::Lt(a, b) => cmp_vars(env, a, b).is_some_and(|o| o == std::cmp::Ordering::Less),
                Pred::Ge(a, b) => cmp_vars(env, a, b).is_some_and(|o| o != std::cmp::Ordering::Less),
                Pred::Gt(a, b) => cmp_vars(env, a, b).is_some_and(|o| o == std::cmp::Ordering::Greater),
            })
        });

        let mut rows = Vec::with_capacity(envs.len());
        for env in envs {
            let mut row = Vec::with_capacity(query.find.len());
            for var in &query.find {
                let v = env
                    .get(var)
                    .ok_or_else(|| DatalogError::UnboundVar(var.clone()))?
                    .clone();
                row.push(v);
            }
            rows.push(row);
        }
        Ok(rows)
    }

    /// Execute an EDN Datalog query string (e.g. `"[:find ?isin :in $ ?t :where [?e :sec/ticker ?t] [?e :sec/isin ?isin]]"`).
    pub fn q_str(
        &self,
        rtxn: &ReadTxn<'_>,
        query_str: &str,
        inputs: &[Value],
    ) -> Result<QueryResult, DatalogError> {
        self.q_str_with_rules(rtxn, query_str, inputs, "[]")
    }

    /// Execute an EDN Datalog query string with EDN rule definitions (supports recursive rules, `or`, `not`, aggregates).
    pub fn q_str_with_rules(
        &self,
        rtxn: &ReadTxn<'_>,
        query_str: &str,
        inputs: &[Value],
        rules_str: &str,
    ) -> Result<QueryResult, DatalogError> {
        let parsed = parse_query(query_str, inputs)?;
        let rules = parse_rules(rules_str)?;
        let mut rule_map: HashMap<String, Vec<RuleDef>> = HashMap::new();
        for r in rules {
            rule_map.entry(r.name.clone()).or_default().push(r);
        }
        let rule_tables = if rule_map.is_empty() {
            HashMap::new()
        } else {
            self.eval_all_rules(rtxn, &rule_map)?
        };

        let envs = self.eval_clauses(rtxn, &parsed.clauses, vec![HashMap::new()], &rule_tables)?;
        self.finalize_query_result(rtxn, &parsed, envs)
    }

    fn finalize_query_result(
        &self,
        rtxn: &ReadTxn<'_>,
        parsed: &ParsedQuery,
        mut envs: Vec<HashMap<String, Value>>,
    ) -> Result<QueryResult, DatalogError> {
        let has_agg = parsed
            .find
            .iter()
            .any(|e| matches!(e, FindElem::Agg(_, _)));

        if has_agg {
            let rows = aggregate_envs(&envs, &parsed.find)?;
            return Ok(match parsed.find_spec {
                FindSpec::Rel => QueryResult::Rel(rows),
                FindSpec::Scalar => QueryResult::Scalar(rows.into_iter().next().and_then(|r| r.into_iter().next())),
                FindSpec::Coll => QueryResult::Coll(rows.into_iter().filter_map(|r| r.into_iter().next()).collect()),
                FindSpec::Tuple => QueryResult::Tuple(rows.into_iter().next()),
            });
        }

        // Apply :order-by before :offset/:limit and projection (upstream order_test parity)
        if !parsed.order_by.is_empty() {
            envs.sort_by(|a, b| {
                for spec in &parsed.order_by {
                    let var_name = match &spec.key {
                        OrderKey::Var(v) => Some(v.as_str()),
                        OrderKey::Col(idx) => parsed.find.get(*idx).and_then(|fe| match fe {
                            FindElem::Var(v) => Some(v.as_str()),
                            FindElem::Pull(v, _) => Some(v.as_str()),
                            FindElem::Agg(_, v) => Some(v.as_str()),
                        }),
                    };
                    if let Some(vn) = var_name {
                        if let (Some(va), Some(vb)) = (a.get(vn), b.get(vn)) {
                            if let Some(ord) = cmp_vals(va, vb) {
                                if ord != std::cmp::Ordering::Equal {
                                    return if spec.desc { ord.reverse() } else { ord };
                                }
                            }
                        }
                    }
                }
                std::cmp::Ordering::Equal
            });
        }

        let start = parsed.offset.unwrap_or(0).max(0) as usize;
        let envs_slice: Vec<HashMap<String, Value>> = if start >= envs.len() {
            Vec::new()
        } else {
            let sliced = &envs[start..];
            match parsed.limit {
                Some(lim) if lim >= 0 => sliced.iter().take(lim as usize).cloned().collect(),
                _ => sliced.to_vec(),
            }
        };

        // Handle (pull ?e [...]) inside :find [[(pull ?e [...]) ...]]
        if parsed.find.len() == 1 {
            if let FindElem::Pull(var, attrs) = &parsed.find[0] {
                let selectors: Vec<PullSelector> = attrs
                    .iter()
                    .map(|a| {
                        if a == "*" {
                            PullSelector::All
                        } else {
                            PullSelector::Attr(a.clone())
                        }
                    })
                    .collect();
                let mut pulled = Vec::with_capacity(envs_slice.len());
                for env in &envs_slice {
                    let eid = env
                        .get(var)
                        .and_then(|v| v.as_ref())
                        .ok_or_else(|| DatalogError::UnboundVar(var.clone()))?;
                    pulled.push(self.pull_pattern(rtxn, eid, &selectors)?);
                }
                return Ok(QueryResult::PulledColl(pulled));
            }
        }

        let mut rows = Vec::with_capacity(envs_slice.len());
        let dedup = parsed.order_by.is_empty() && envs_slice.len() > 1;
        let mut seen = if dedup {
            HashSet::with_capacity(envs_slice.len())
        } else {
            HashSet::new()
        };
        for env in &envs_slice {
            let mut row = Vec::with_capacity(parsed.find.len());
            for elem in &parsed.find {
                if let FindElem::Var(v) = elem {
                    let val = env
                        .get(v)
                        .ok_or_else(|| DatalogError::UnboundVar(v.clone()))?
                        .clone();
                    row.push(val);
                }
            }
            if dedup {
                let key = format!("{:?}", row);
                if seen.insert(key) {
                    rows.push(row);
                }
            } else {
                rows.push(row);
            }
        }

        if let Some(keys) = &parsed.return_keys {
            let maps = rows
                .into_iter()
                .map(|r| keys.iter().cloned().zip(r).collect::<BTreeMap<_, _>>())
                .collect();
            return Ok(QueryResult::Maps(maps));
        }

        Ok(match parsed.find_spec {
            FindSpec::Rel => QueryResult::Rel(rows),
            FindSpec::Scalar => QueryResult::Scalar(rows.into_iter().next().and_then(|r| r.into_iter().next())),
            FindSpec::Coll => QueryResult::Coll(rows.into_iter().filter_map(|r| r.into_iter().next()).collect()),
            FindSpec::Tuple => QueryResult::Tuple(rows.into_iter().next()),
        })
    }

    fn eval_all_rules(
        &self,
        rtxn: &ReadTxn<'_>,
        rule_map: &HashMap<String, Vec<RuleDef>>,
    ) -> Result<HashMap<String, Vec<Vec<Value>>>, DatalogError> {
        let mut tables: HashMap<String, Vec<Vec<Value>>> = HashMap::new();
        let mut seen: HashMap<String, HashSet<String>> = HashMap::new();
        for name in rule_map.keys() {
            tables.insert(name.clone(), Vec::new());
            seen.insert(name.clone(), HashSet::new());
        }

        // Fixed-point semi-naive iteration for recursive rules
        loop {
            let mut changed = false;
            for (name, defs) in rule_map {
                for def in defs {
                    let envs = self.eval_clauses(rtxn, &def.body, vec![HashMap::new()], &tables)?;
                    for env in envs {
                        let mut tuple = Vec::with_capacity(def.params.len());
                        let mut ok = true;
                        for p in &def.params {
                            if let Some(v) = env.get(p) {
                                tuple.push(v.clone());
                            } else {
                                ok = false;
                                break;
                            }
                        }
                        if ok {
                            let key = format!("{:?}", tuple);
                            if seen.get_mut(name).unwrap().insert(key) {
                                tables.get_mut(name).unwrap().push(tuple);
                                changed = true;
                            }
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
        Ok(tables)
    }

    fn eval_clauses(
        &self,
        rtxn: &ReadTxn<'_>,
        clauses: &[Clause],
        mut envs: Vec<HashMap<String, Value>>,
        rule_tables: &HashMap<String, Vec<Vec<Value>>>,
    ) -> Result<Vec<HashMap<String, Value>>, DatalogError> {
        let mut buf = Vec::with_capacity(64);
        for clause in clauses {
            let mut next_envs = Vec::new();
            match clause {
                Clause::Datom(e_term, attr_name, v_term) => {
                    let aid = self.attr_id(attr_name)?;
                    let spec = &self.attrs[&aid];
                    for env in &envs {
                        let bound_e: Option<EntityId> = match e_term {
                            ExprTerm::Val(v) => v.as_ref(),
                            ExprTerm::Var(var) => env.get(var).and_then(|v| v.as_ref()),
                            ExprTerm::Wildcard => None,
                        };
                        let bound_v: Option<&Value> = match v_term {
                            ExprTerm::Val(v) => Some(v),
                            ExprTerm::Var(var) => env.get(var),
                            ExprTerm::Wildcard => None,
                        };
                        match (bound_e, bound_v) {
                            (Some(eid), Some(val)) => {
                                encode_eav_key(eid, aid, val, &mut buf);
                                if self.eav.get(rtxn, &buf)?.is_some() {
                                    next_envs.push(env.clone());
                                }
                            }
                            (Some(eid), None) => {
                                encode_ea_prefix(eid, aid, &mut buf);
                                for item in self.eav.prefix_iter(rtxn, &buf)? {
                                    let (k, _) = item?;
                                    let val = decode_value(&k[12..])?;
                                    let mut e2 = env.clone();
                                    if let ExprTerm::Var(vname) = v_term {
                                        e2.insert(vname.clone(), val);
                                    }
                                    next_envs.push(e2);
                                }
                            }
                            (None, Some(val)) if spec.index => {
                                encode_av_prefix(aid, val, &mut buf);
                                for item in self.ave.prefix_iter(rtxn, &buf)? {
                                    let (k, _) = item?;
                                    let eid = decode_ave_entity(&k)?;
                                    let mut e2 = env.clone();
                                    if let ExprTerm::Var(ename) = e_term {
                                        e2.insert(ename.clone(), Value::Ref(eid));
                                    }
                                    next_envs.push(e2);
                                }
                            }
                            (None, _) => {
                                for item in self.eav.iter(rtxn)? {
                                    let (k, _) = item?;
                                    if k.len() < 12 {
                                        continue;
                                    }
                                    let k_aid = u32::from_be_bytes(k[8..12].try_into().unwrap());
                                    if k_aid != aid {
                                        continue;
                                    }
                                    let eid = u64::from_be_bytes(k[0..8].try_into().unwrap());
                                    let val = decode_value(&k[12..])?;
                                    if let Some(bv) = bound_v {
                                        if &val != bv {
                                            continue;
                                        }
                                    }
                                    let mut e2 = env.clone();
                                    if let ExprTerm::Var(ename) = e_term {
                                        e2.insert(ename.clone(), Value::Ref(eid));
                                    }
                                    if let ExprTerm::Var(vname) = v_term {
                                        e2.insert(vname.clone(), val);
                                    }
                                    next_envs.push(e2);
                                }
                            }
                        }
                    }
                }
                Clause::Fulltext(q_term, e_var) => {
                    for env in &envs {
                        let q_val = match q_term {
                            ExprTerm::Val(v) => v.as_str().map(|s| s.to_owned()),
                            ExprTerm::Var(v) => env.get(v).and_then(|val| val.as_str().map(|s| s.to_owned())),
                            ExprTerm::Wildcard => None,
                        };
                        if let Some(q_str) = q_val {
                            let hits = self.fts.search(rtxn, &q_str, 100)?;
                            for &(eid, _) in &hits {
                                let mut e2 = env.clone();
                                if let Some(bound) = e2.get(e_var) {
                                    if bound != &Value::Ref(eid) {
                                        continue;
                                    }
                                } else {
                                    e2.insert(e_var.clone(), Value::Ref(eid));
                                }
                                next_envs.push(e2);
                            }
                        }
                    }
                }
                Clause::Pred(op, lhs, rhs) => {
                    for mut env in envs {
                        let lv = resolve_term(&env, lhs).cloned();
                        let rv = resolve_term(&env, rhs).cloned();
                        match (op, lv, rv) {
                            (CmpOp::Eq, Some(a), None) => {
                                if let ExprTerm::Var(v) = rhs {
                                    env.insert(v.clone(), a);
                                    next_envs.push(env);
                                }
                            }
                            (CmpOp::Eq, None, Some(b)) => {
                                if let ExprTerm::Var(v) = lhs {
                                    env.insert(v.clone(), b);
                                    next_envs.push(env);
                                }
                            }
                            (_, Some(ref a), Some(ref b)) => {
                                let pass = match op {
                                    CmpOp::Eq => a == b,
                                    CmpOp::Ne => a != b,
                                    CmpOp::Le => cmp_vals(a, b).is_some_and(|o| o != std::cmp::Ordering::Greater),
                                    CmpOp::Lt => cmp_vals(a, b).is_some_and(|o| o == std::cmp::Ordering::Less),
                                    CmpOp::Ge => cmp_vals(a, b).is_some_and(|o| o != std::cmp::Ordering::Less),
                                    CmpOp::Gt => cmp_vals(a, b).is_some_and(|o| o == std::cmp::Ordering::Greater),
                                    CmpOp::Like => match (a.as_str(), b.as_str()) {
                                        (Some(text), Some(pat)) => like_match(text, pat),
                                        _ => false,
                                    },
                                };
                                if pass {
                                    next_envs.push(env);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Clause::Or(branches) => {
                    for branch in branches {
                        let branch_envs = self.eval_clauses(rtxn, branch, envs.clone(), rule_tables)?;
                        next_envs.extend(branch_envs);
                    }
                }
                Clause::Not(inner) => {
                    for env in envs {
                        let matches = self.eval_clauses(rtxn, inner, vec![env.clone()], rule_tables)?;
                        if matches.is_empty() {
                            next_envs.push(env);
                        }
                    }
                }
                Clause::RuleCall(name, args) => {
                    let tuples = rule_tables
                        .get(name)
                        .ok_or_else(|| DatalogError::UnknownRule(name.clone()))?;
                    for env in &envs {
                        for tup in tuples {
                            if tup.len() != args.len() {
                                continue;
                            }
                            let mut e2 = env.clone();
                            let mut matched = true;
                            for (arg, val) in args.iter().zip(tup.iter()) {
                                match arg {
                                    ExprTerm::Wildcard => {}
                                    ExprTerm::Val(v) => {
                                        if v != val {
                                            matched = false;
                                            break;
                                        }
                                    }
                                    ExprTerm::Var(var) => {
                                        if let Some(existing) = e2.get(var) {
                                            if existing != val {
                                                matched = false;
                                                break;
                                            }
                                        } else {
                                            e2.insert(var.clone(), val.clone());
                                        }
                                    }
                                }
                            }
                            if matched {
                                next_envs.push(e2);
                            }
                        }
                    }
                }
            }
            envs = next_envs;
        }
        Ok(envs)
    }
}

fn resolve_term<'a>(env: &'a HashMap<String, Value>, term: &'a ExprTerm) -> Option<&'a Value> {
    match term {
        ExprTerm::Val(v) => Some(v),
        ExprTerm::Var(var) => env.get(var),
        ExprTerm::Wildcard => None,
    }
}

fn cmp_vals(va: &Value, vb: &Value) -> Option<std::cmp::Ordering> {
    match (va, vb) {
        (Value::I64(x), Value::I64(y))
        | (Value::Instant(x), Value::Instant(y))
        | (Value::I64(x), Value::Instant(y))
        | (Value::Instant(x), Value::I64(y)) => Some(x.cmp(y)),
        (Value::F64(x), Value::F64(y)) => x.partial_cmp(y),
        (Value::Str(x), Value::Str(y)) | (Value::Keyword(x), Value::Keyword(y)) => Some(x.cmp(y)),
        (Value::Ref(x), Value::Ref(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// SQL-style `LIKE` wildcard matching (`%` = 0+ chars, `_` = 1 char), UTF-8 char-aware.
pub fn like_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (n, m) = (t.len(), p.len());
    let mut dp = vec![false; m + 1];
    dp[0] = true;
    for j in 1..=m {
        if p[j - 1] == '%' {
            dp[j] = dp[j - 1];
        }
    }
    for i in 1..=n {
        let mut next = vec![false; m + 1];
        for j in 1..=m {
            if p[j - 1] == '%' {
                next[j] = next[j - 1] || dp[j];
            } else if p[j - 1] == '_' || p[j - 1] == t[i - 1] {
                next[j] = dp[j - 1];
            }
        }
        dp = next;
    }
    dp[m]
}

pub struct PreparedPull<'conn> {
    conn: &'conn Conn,
    selectors: Vec<PullSelector>,
}

impl<'conn> PreparedPull<'conn> {
    pub fn execute(&self, rtxn: &ReadTxn<'_>, eid: EntityId) -> Result<BTreeMap<String, PullValue>, DatalogError> {
        self.conn.pull_pattern(rtxn, eid, &self.selectors)
    }

    pub fn execute_by_lookup(
        &self,
        rtxn: &ReadTxn<'_>,
        attr: &str,
        val: &Value,
    ) -> Result<Option<BTreeMap<String, PullValue>>, DatalogError> {
        if let Some(eid) = self.conn.lookup_entity_by_av(rtxn, attr, val)? {
            Ok(Some(self.conn.pull_pattern(rtxn, eid, &self.selectors)?))
        } else {
            Ok(None)
        }
    }
}

pub struct PreparedQuery<'conn> {
    conn: &'conn Conn,
    parsed: parser::ParsedQuery,
    rule_map: HashMap<String, Vec<RuleDef>>,
}

impl<'conn> PreparedQuery<'conn> {
    pub fn execute(&self, rtxn: &ReadTxn<'_>, inputs: &[Value]) -> Result<QueryResult, DatalogError> {
        if self.parsed.in_vars.len() != inputs.len() {
            return Err(DatalogError::Parse(ParseError::InvalidQuery(format!(
                "expected {} :in arguments, got {}",
                self.parsed.in_vars.len(),
                inputs.len()
            ))));
        }
        let mut init_env = HashMap::with_capacity(inputs.len());
        for (var, val) in self.parsed.in_vars.iter().zip(inputs.iter()) {
            init_env.insert(var.clone(), val.clone());
        }
        let rule_tables = if self.rule_map.is_empty() {
            HashMap::new()
        } else {
            self.conn.eval_all_rules(rtxn, &self.rule_map)?
        };
        let envs = self
            .conn
            .eval_clauses(rtxn, &self.parsed.clauses, vec![init_env], &rule_tables)?;
        self.conn.finalize_query_result(rtxn, &self.parsed, envs)
    }
}

impl Conn {
    pub fn prepare_pull(&self, selectors: Vec<PullSelector>) -> PreparedPull<'_> {
        PreparedPull {
            conn: self,
            selectors,
        }
    }

    pub fn prepare_query(&self, query_str: &str, rules_str: &str) -> Result<PreparedQuery<'_>, DatalogError> {
        // Parse template with dummy placeholders for :in args
        let parsed_tmp = parse_query_template(query_str)?;
        let rules = parse_rules(rules_str)?;
        let mut rule_map: HashMap<String, Vec<RuleDef>> = HashMap::new();
        for r in rules {
            rule_map.entry(r.name.clone()).or_default().push(r);
        }
        Ok(PreparedQuery {
            conn: self,
            parsed: parsed_tmp,
            rule_map,
        })
    }
}

fn value_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::I64(x) | Value::Instant(x) => Some(*x as f64),
        Value::F64(x) => Some(*x),
        _ => None,
    }
}

fn aggregate_envs(
    envs: &[HashMap<String, Value>],
    find: &[FindElem],
) -> Result<Vec<Vec<Value>>, DatalogError> {
    if envs.is_empty() {
        return Ok(Vec::new());
    }
    let group_vars: Vec<String> = find
        .iter()
        .filter_map(|e| match e {
            FindElem::Var(v) => Some(v.clone()),
            _ => None,
        })
        .collect();

    let mut groups: BTreeMap<String, Vec<&HashMap<String, Value>>> = BTreeMap::new();
    let mut group_keys_vals: HashMap<String, Vec<Value>> = HashMap::new();

    for env in envs {
        let mut gvals = Vec::with_capacity(group_vars.len());
        for gv in &group_vars {
            let v = env
                .get(gv)
                .ok_or_else(|| DatalogError::UnboundVar(gv.clone()))?
                .clone();
            gvals.push(v);
        }
        let gkey = format!("{:?}", gvals);
        group_keys_vals.entry(gkey.clone()).or_insert(gvals);
        groups.entry(gkey).or_default().push(env);
    }

    let mut rows = Vec::with_capacity(groups.len());
    for (gkey, group_envs) in groups {
        let gvals = &group_keys_vals[&gkey];
        let mut g_idx = 0;
        let mut row = Vec::with_capacity(find.len());
        for elem in find {
            match elem {
                FindElem::Var(_) => {
                    row.push(gvals[g_idx].clone());
                    g_idx += 1;
                }
                FindElem::Agg(op, var) => {
                    let vals: Vec<&Value> = group_envs
                        .iter()
                        .filter_map(|e| e.get(var))
                        .collect();
                    let agg_val = match op {
                        AggOp::Count => Value::I64(vals.len() as i64),
                        AggOp::Sum => {
                            let s: f64 = vals.iter().filter_map(|v| value_as_f64(v)).sum();
                            if vals.iter().all(|v| matches!(v, Value::I64(_))) {
                                Value::I64(s as i64)
                            } else {
                                Value::F64(s)
                            }
                        }
                        AggOp::Avg => {
                            let nums: Vec<f64> = vals.iter().filter_map(|v| value_as_f64(v)).collect();
                            Value::F64(nums.iter().sum::<f64>() / (nums.len().max(1) as f64))
                        }
                        AggOp::Min => vals
                            .iter()
                            .copied()
                            .min_by(|a, b| cmp_vals(a, b).unwrap_or(std::cmp::Ordering::Equal))
                            .cloned()
                            .unwrap_or(Value::I64(0)),
                        AggOp::Max => vals
                            .iter()
                            .copied()
                            .max_by(|a, b| cmp_vals(a, b).unwrap_or(std::cmp::Ordering::Equal))
                            .cloned()
                            .unwrap_or(Value::I64(0)),
                    };
                    row.push(agg_val);
                }
                FindElem::Pull(_, _) => {}
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

fn cmp_vars(env: &HashMap<String, Value>, a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let va = env.get(a)?;
    let vb = env.get(b)?;
    match (va, vb) {
        (Value::I64(x), Value::I64(y))
        | (Value::Instant(x), Value::Instant(y))
        | (Value::I64(x), Value::Instant(y))
        | (Value::Instant(x), Value::I64(y)) => Some(x.cmp(y)),
        (Value::F64(x), Value::F64(y)) => x.partial_cmp(y),
        (Value::Str(x), Value::Str(y)) | (Value::Keyword(x), Value::Keyword(y)) => Some(x.cmp(y)),
        (Value::Ref(x), Value::Ref(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_schema() -> Vec<AttrSpec> {
        vec![
            AttrSpec::new(":sec/id").unique_identity(),
            AttrSpec::new(":sec/ticker").unique_identity(),
            AttrSpec::new(":sec/isin").indexed(),
            AttrSpec::new(":sec/name").fulltext(),
            AttrSpec::new(":sec/lot-size"),
            AttrSpec::new(":sec/split-factor"),
        ]
    }

    #[test]
    fn crud_upsert_query_and_reopen() {
        let dir = tempdir().unwrap();
        {
            let conn = Conn::open(dir.path(), test_schema()).unwrap();
            conn.transact(&[
                TxItem::Assert(
                    None,
                    vec![
                        (":sec/id".into(), Value::Str("SEC-00000".into())),
                        (":sec/ticker".into(), Value::Str("AAPL".into())),
                        (":sec/isin".into(), Value::Str("US0378331005".into())),
                        (":sec/name".into(), Value::Str("Apple Inc.".into())),
                        (":sec/lot-size".into(), Value::I64(100)),
                        (":sec/split-factor".into(), Value::F64(1.0)),
                    ],
                ),
            ])
            .unwrap();

            // Upsert via :sec/ticker unique identity
            conn.transact(&[TxItem::Assert(
                None,
                vec![
                    (":sec/ticker".into(), Value::Str("AAPL".into())),
                    (":sec/split-factor".into(), Value::F64(4.0)),
                ],
            )])
            .unwrap();

            let rtxn = conn.read_txn().unwrap();
            let q = Query {
                find: vec!["?isin".into(), "?sf".into()],
                patterns: vec![
                    Pattern::Datom(
                        Term::Var("?e".into()),
                        ":sec/ticker".into(),
                        Term::Val(Value::Str("AAPL".into())),
                    ),
                    Pattern::Datom(
                        Term::Var("?e".into()),
                        ":sec/isin".into(),
                        Term::Var("?isin".into()),
                    ),
                    Pattern::Datom(
                        Term::Var("?e".into()),
                        ":sec/split-factor".into(),
                        Term::Var("?sf".into()),
                    ),
                ],
                preds: vec![],
            };
            let rows = conn.q(&rtxn, &q).unwrap();
            assert_eq!(
                rows,
                vec![vec![
                    Value::Str("US0378331005".into()),
                    Value::F64(4.0)
                ]]
            );

            // Fulltext search via Datalog
            let fts_q = Query {
                find: vec!["?t".into()],
                patterns: vec![
                    Pattern::Fulltext("apple".into(), "?e".into()),
                    Pattern::Datom(
                        Term::Var("?e".into()),
                        ":sec/ticker".into(),
                        Term::Var("?t".into()),
                    ),
                ],
                preds: vec![],
            };
            let fts_rows = conn.q(&rtxn, &fts_q).unwrap();
            assert_eq!(fts_rows, vec![vec![Value::Str("AAPL".into())]]);
        }

        // Reopen from disk and verify persistence
        let conn2 = Conn::open(dir.path(), test_schema()).unwrap();
        let rtxn2 = conn2.read_txn().unwrap();
        let eid = conn2
            .lookup_entity_by_av(&rtxn2, ":sec/ticker", &Value::Str("AAPL".into()))
            .unwrap()
            .unwrap();
        assert_eq!(
            conn2.get_attr(&rtxn2, eid, ":sec/split-factor").unwrap(),
            Some(Value::F64(4.0))
        );
    }

    #[test]
    fn edn_parser_or_not_aggregates_and_recursive_rules() {
        let dir = tempdir().unwrap();
        let mut schema = test_schema();
        schema.push(AttrSpec::new(":sec/exchange").indexed());
        schema.push(AttrSpec::new(":sec/status"));
        schema.push(AttrSpec::new(":corp/parent").indexed());

        let conn = Conn::open(dir.path(), schema).unwrap();
        let ids = conn
            .transact(&[
                TxItem::Assert(
                    None,
                    vec![
                        (":sec/id".into(), Value::Str("SEC-1".into())),
                        (":sec/ticker".into(), Value::Str("AAPL".into())),
                        (":sec/isin".into(), Value::Str("US0378331005".into())),
                        (":sec/name".into(), Value::Str("Apple Inc.".into())),
                        (":sec/exchange".into(), Value::Str("XNAS".into())),
                        (":sec/lot-size".into(), Value::I64(100)),
                        (":sec/status".into(), Value::Str("ACTIVE".into())),
                    ],
                ),
                TxItem::Assert(
                    None,
                    vec![
                        (":sec/id".into(), Value::Str("SEC-2".into())),
                        (":sec/ticker".into(), Value::Str("IBM".into())),
                        (":sec/isin".into(), Value::Str("US4592001014".into())),
                        (":sec/name".into(), Value::Str("IBM Corp".into())),
                        (":sec/exchange".into(), Value::Str("XNYS".into())),
                        (":sec/lot-size".into(), Value::I64(50)),
                        (":sec/status".into(), Value::Str("ACTIVE".into())),
                    ],
                ),
                TxItem::Assert(
                    None,
                    vec![
                        (":sec/id".into(), Value::Str("SEC-3".into())),
                        (":sec/ticker".into(), Value::Str("HALT".into())),
                        (":sec/isin".into(), Value::Str("US0000000003".into())),
                        (":sec/name".into(), Value::Str("Halted Co".into())),
                        (":sec/exchange".into(), Value::Str("XNYS".into())),
                        (":sec/lot-size".into(), Value::I64(10)),
                        (":sec/status".into(), Value::Str("HALTED".into())),
                    ],
                ),
                TxItem::Assert(
                    None,
                    vec![
                        (":sec/id".into(), Value::Str("SEC-4".into())),
                        (":sec/ticker".into(), Value::Str("VOD".into())),
                        (":sec/isin".into(), Value::Str("GB00BH4HKS39".into())),
                        (":sec/name".into(), Value::Str("Vodafone Group".into())),
                        (":sec/exchange".into(), Value::Str("XLON".into())),
                        (":sec/lot-size".into(), Value::I64(1000)),
                        (":sec/status".into(), Value::Str("ACTIVE".into())),
                    ],
                ),
            ])
            .unwrap();

        // Corporate hierarchy: SEC-1 -> SEC-2 -> SEC-3 (parent chain)
        conn.transact(&[
            TxItem::Assert(Some(ids[1]), vec![(":corp/parent".into(), Value::Ref(ids[0]))]),
            TxItem::Assert(Some(ids[2]), vec![(":corp/parent".into(), Value::Ref(ids[1]))]),
        ])
        .unwrap();

        let rtxn = conn.read_txn().unwrap();

        // 1. Scalar :find with :in
        let res = conn
            .q_str(
                &rtxn,
                r#"[:find ?isin . :in $ ?t :where [?e :sec/ticker ?t] [?e :sec/isin ?isin]]"#,
                &[Value::Str("AAPL".into())],
            )
            .unwrap();
        assert_eq!(res, QueryResult::Scalar(Some(Value::Str("US0378331005".into()))));

        // 2. `or` + `not` + collection :find
        let res = conn
            .q_str(
                &rtxn,
                r#"[:find [?t ...]
                    :where
                    (or [?e :sec/exchange "XNAS"]
                        [?e :sec/exchange "XNYS"])
                    (not [?e :sec/status "HALTED"])
                    [?e :sec/ticker ?t]]"#,
                &[],
            )
            .unwrap();
        if let QueryResult::Coll(mut tickers) = res {
            tickers.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            assert_eq!(
                tickers,
                vec![Value::Str("AAPL".into()), Value::Str("IBM".into())]
            );
        } else {
            panic!("expected Coll");
        }

        // 3. Aggregates: count and sum(lot-size) by exchange
        let res = conn
            .q_str(
                &rtxn,
                r#"[:find ?ex (count ?e) (sum ?lot)
                    :where
                    [?e :sec/exchange ?ex]
                    [?e :sec/lot-size ?lot]]"#,
                &[],
            )
            .unwrap();
        if let QueryResult::Rel(rows) = res {
            assert!(rows.contains(&vec![Value::Str("XNAS".into()), Value::I64(1), Value::I64(100)]));
            assert!(rows.contains(&vec![Value::Str("XNYS".into()), Value::I64(2), Value::I64(60)]));
            assert!(rows.contains(&vec![Value::Str("XLON".into()), Value::I64(1), Value::I64(1000)]));
        } else {
            panic!("expected Rel");
        }

        // 4. Recursive rules: transitive corporate ownership (ancestor ?anc ?sub)
        let rules = r#"[
            [(ancestor ?anc ?sub)
             [?sub :corp/parent ?anc]]
            [(ancestor ?anc ?sub)
             [?mid :corp/parent ?anc]
             (ancestor ?mid ?sub)]
        ]"#;
        let res = conn
            .q_str_with_rules(
                &rtxn,
                r#"[:find [?t ...]
                    :in $ ?root-ticker
                    :where
                    [?root :sec/ticker ?root-ticker]
                    (ancestor ?root ?sub)
                    [?sub :sec/ticker ?t]]"#,
                &[Value::Str("AAPL".into())],
                rules,
            )
            .unwrap();
        if let QueryResult::Coll(mut subs) = res {
            subs.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            assert_eq!(
                subs,
                vec![Value::Str("HALT".into()), Value::Str("IBM".into())]
            );
        } else {
            panic!("expected Coll");
        }

        // 5. Fulltext via EDN string
        let res = conn
            .q_str(
                &rtxn,
                r#"[:find ?t . :where [(fulltext $ "apple") [[?e _ _]]] [?e :sec/ticker ?t]]"#,
                &[],
            )
            .unwrap();
        assert_eq!(res, QueryResult::Scalar(Some(Value::Str("AAPL".into()))));
    }

    #[test]
    fn completeness_pull_tempids_components_and_vector_test() {
        let dir = tempdir().unwrap();
        let mut schema = test_schema();
        schema.push(AttrSpec::new(":sec/tag").many().indexed());
        schema.push(AttrSpec::new(":sec/venue").component());
        schema.push(AttrSpec::new(":venue/mic").unique_value());
        schema.push(AttrSpec::new(":venue/country"));

        let conn = Conn::open(dir.path(), schema).unwrap();

        // 1. Tempid + component + cardinality/many
        let ids = conn
            .transact(&[
                TxItem::AssertRef(
                    EntityRef::TempId("v1".into()),
                    vec![
                        (":venue/mic".into(), TxVal::Val(Value::Str("XNAS".into()))),
                        (":venue/country".into(), TxVal::Val(Value::Str("US".into()))),
                    ],
                ),
                TxItem::AssertRef(
                    EntityRef::TempId("s1".into()),
                    vec![
                        (":sec/id".into(), TxVal::Val(Value::Str("SEC-1".into()))),
                        (":sec/ticker".into(), TxVal::Val(Value::Str("AAPL".into()))),
                        (":sec/isin".into(), TxVal::Val(Value::Str("US0378331005".into()))),
                        (":sec/name".into(), TxVal::Val(Value::Str("Apple Inc.".into()))),
                        (":sec/tag".into(), TxVal::Val(Value::Str("TECH".into()))),
                        (":sec/tag".into(), TxVal::Val(Value::Str("SP500".into()))),
                        (":sec/venue".into(), TxVal::TempRef("v1".into())),
                    ],
                ),
            ])
            .unwrap();
        let venue_eid = ids[0];
        let sec_eid = ids[1];

        // 2. Unique::Value conflict check
        let dup_err = conn.transact(&[TxItem::Assert(
            None,
            vec![(":venue/mic".into(), Value::Str("XNAS".into()))],
        )]);
        assert!(matches!(dup_err, Err(DatalogError::UniqueViolation { .. })));

        // 3. Full pull_pattern: nested, cardinality/many, reverse :_sec/venue
        {
            let rtxn = conn.read_txn().unwrap();
            let pulled = conn
                .pull_pattern(
                    &rtxn,
                    sec_eid,
                    &[
                        PullSelector::Attr(":sec/ticker".into()),
                        PullSelector::Attr(":sec/tag".into()),
                        PullSelector::Nested(
                            ":sec/venue".into(),
                            vec![PullSelector::All],
                        ),
                    ],
                )
                .unwrap();
            assert_eq!(pulled.get(":sec/ticker"), Some(&PullValue::Single(Value::Str("AAPL".into()))));
            assert_eq!(
                pulled.get(":sec/tag"),
                Some(&PullValue::Many(vec![
                    Value::Str("SP500".into()),
                    Value::Str("TECH".into()),
                ]))
            );
            assert!(matches!(pulled.get(":sec/venue"), Some(PullValue::Map(_))));

            // Reverse pull on venue_eid
            let rev = conn
                .pull_pattern(&rtxn, venue_eid, &[PullSelector::Reverse(":sec/venue".into())])
                .unwrap();
            assert_eq!(
                rev.get(":_sec/venue"),
                Some(&PullValue::Many(vec![Value::Ref(sec_eid)]))
            );

            // or-join & not-join
            let oj = conn
                .q_str(
                    &rtxn,
                    r#"[:find ?t . :where (or-join [?e ?t] [?e :sec/ticker ?t]) (not-join [?e] [?e :sec/ticker "MSFT"])]"#,
                    &[],
                )
                .unwrap();
            assert_eq!(oj, QueryResult::Scalar(Some(Value::Str("AAPL".into()))));

            // datoms(Eavt)
            assert!(!conn.datoms(&rtxn, IndexKind::Eavt).unwrap().is_empty());
        }

        // 4. Vector search integration
        conn.add_vec(sec_eid, &[1.0, 0.0, 0.0]).unwrap();
        {
            let rtxn = conn.read_txn().unwrap();
            let vhits = conn.search_vec(&rtxn, &[0.95, 0.05, 0.0], 1).unwrap();
            assert_eq!(vhits.len(), 1);
            assert_eq!(vhits[0].0, sec_eid);
        }

        // 5. Component cascade delete: retracting sec_eid also retracts venue_eid & FTS
        conn.transact(&[TxItem::RetractEntity(sec_eid)]).unwrap();
        let rtxn2 = conn.read_txn().unwrap();
        assert_eq!(conn.get_attr(&rtxn2, venue_eid, ":venue/mic").unwrap(), None);
        assert!(conn.fulltext_search(&rtxn2, "apple", 5).unwrap().is_empty());
    }
}
