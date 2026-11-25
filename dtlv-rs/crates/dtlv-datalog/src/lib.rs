pub mod parser;
mod idoc;
mod entity;
mod optimizer;
mod magic;

pub use optimizer::{Access, AnalyzeStep, PlanStep};

pub use entity::Entity;
mod wal;

pub use wal::{read_records as read_wal_records, Durability};

pub use idoc::{apply_patch, idoc_get, idoc_value, idoc_value_edn, idoc_value_markdown, normalize_header, pattern_predicates, PatchOp, PathSeg, JSON_NULL};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use dtlv_codec::{
    decode_ave_entity, decode_value, encode_av_prefix, encode_ave_key, encode_ea_prefix, encode_value,
    encode_eav_key, encode_vae_key, AttrId, CodecError, EntityId, Value,
};
use dtlv_kv::{KvError, KvStore, RawDb, ReadTxn, WriteTxn};
use dtlv_search::{FullTextIndex, FtsConfig, Metric, VectorIndex};
pub use dtlv_search::{FtsConfig as FulltextConfig, Query as FtsQuery};
pub use parser::QueryInput;
use parser::{
    parse_query_inputs, parse_query_template, parse_rules, AggOp, Clause, CmpOp, ExprTerm,
    FindElem, FindSpec, FnOut, InBinding, OrderKey, ParseError, ParsedQuery, RuleDef,
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
    /// Source attribute names when this is a composite tuple (`:db/tupleAttrs`).
    pub tuple_sources: Option<Vec<String>>,
    /// Indexed-document attribute: values must be `Value::Doc` and are path-indexed.
    pub idoc: bool,
    /// Idoc path selectors to index. `None` indexes every path.
    pub indexed_paths: Option<Vec<Vec<PathSeg>>>,
    /// Idoc path selectors to omit. Excluded paths win over indexed ones.
    pub excluded_paths: Vec<Vec<PathSeg>>,
    /// Named idoc domain whose selectors apply when this attribute sets none of its own.
    pub idoc_domain: Option<String>,
    /// `:db/noindex`: values are kept on the entity (pull and entity reads work) but are not in AVE, so
    /// queries that constrain the value are rejected.
    pub noindex: bool,
    /// Index changes are applied by `Conn::process_idoc_jobs` rather than in the writing transaction.
    pub idoc_async: bool,
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
            tuple_sources: None,
            idoc: false,
            indexed_paths: None,
            excluded_paths: Vec::new(),
            idoc_domain: None,
            noindex: false,
            idoc_async: false,
        }
    }

    /// Declares an indexed-document attribute (see `idoc_value` for the accepted input).
    pub fn idoc(mut self) -> Self {
        self.idoc = true;
        self
    }

    /// Restricts idoc indexing to paths under these selectors (`:db.idoc/indexedPaths`).
    pub fn idoc_indexed_paths(mut self, paths: Vec<Vec<PathSeg>>) -> Self {
        self.indexed_paths = Some(paths);
        self
    }

    /// Idoc attribute whose index is maintained by background jobs (`process_idoc_jobs`). Source writes commit
    /// immediately; `idoc_match` sees a document once its job has run.
    pub fn idoc_async(mut self) -> Self {
        self.idoc_async = true;
        self
    }

    /// Keeps the attribute out of the value index. Entity and pull reads still see it; value-constrained queries
    /// are rejected rather than scanning.
    /// Unique attributes stay indexed, as in Datalevin.
    pub fn noindex(mut self) -> Self {
        self.noindex = true;
        if self.unique.is_none() {
            self.index = false;
        }
        self
    }

    /// Puts this attribute in a named idoc domain. Its selectors come from `Conn::set_idoc_domain` unless
    /// the attribute sets its own.
    pub fn idoc_domain(mut self, name: &str) -> Self {
        self.idoc_domain = Some(name.to_owned());
        self
    }

    /// Omits paths under these selectors from idoc indexing (`:db.idoc/excludedPaths`).
    pub fn idoc_excluded_paths(mut self, paths: Vec<Vec<PathSeg>>) -> Self {
        self.excluded_paths = paths;
        self
    }

    /// Declares a composite tuple attribute over cardinality-one source attributes.
    pub fn tuple_of(mut self, sources: &[&str]) -> Self {
        self.tuple_sources = Some(sources.iter().map(|s| (*s).to_owned()).collect());
        self
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
    #[error("integer overflow in sum aggregate")]
    SumOverflow,
    #[error("query function error: {0}")]
    Function(String),
    #[error("invalid schema: {0}")]
    Schema(String),
    #[error("tuple attribute {0} is derived from its sources and cannot be written directly")]
    TupleWrite(String),
    #[error("unknown transaction function: {0}")]
    UnknownTxFn(String),
    #[error("type error: {0}")]
    Type(String),
    #[error("write-ahead log error: {0}")]
    Wal(String),
    #[error("unknown ensure predicate: {0}")]
    UnknownEnsureFn(String),
    #[error("ensure {pred} failed for arguments {args:?}")]
    EnsureFailed { pred: String, args: Vec<Value> },
}

/// Removes the given suffixed variants of a SQLite path (e.g. `""`, `"-wal"`, `"-shm"`);
/// missing files are fine.
fn remove_sqlite_files(path: &Path, suffixes: &[&str]) -> Result<(), DatalogError> {
    for suffix in suffixes {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(dtlv_kv::KvError::from(e).into()),
        }
    }
    Ok(())
}

fn sibling_tmp_path(path: &Path) -> std::path::PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".tmp");
    p.into()
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

#[derive(Debug, Clone)]
pub enum TxItem {
    /// Assert a map of (attr_name, Value) pairs. If `:db/id` is omitted, allocates or upserts.
    Assert(Option<EntityId>, Vec<(String, Value)>),
    /// Assert with tempid / lookup-ref resolution for both entity and ref values.
    AssertRef(EntityRef, Vec<(String, TxVal)>),
    Retract(EntityId, String, Value),
    RetractEntity(EntityId),
    /// Atomic compare-and-swap: replace `expected` with `new_val` on `(eid, attr)`.
    Cas(EntityId, String, Value, Value),
    /// Removes every value of `(eid, attr)` (`:db.fn/retractAttribute`).
    RetractAttribute(EntityId, String),
    /// Runs a registered transaction function; its returned items are applied in the same transaction.
    Call(String, Vec<Value>),
    /// `:db.fn/patchIdoc`: applies operations to the current document of a cardinality-one idoc attribute.
    PatchIdoc(EntityId, String, Vec<PatchOp>),
    /// `:db.fn/patchIdoc` on a cardinality-many attribute: `old` identifies which stored document to patch.
    PatchIdocOf(EntityId, String, Value, Vec<PatchOp>),
    /// `:db/ensure`: checks a registered predicate against the transaction's own state before commit; a false
    /// result fails the whole transaction. Never written to the WAL.
    Ensure(String, Vec<TxVal>),
}

/// Read view handed to a transaction function. Sees the in-progress transaction.
pub struct TxView<'a, 'w> {
    conn: &'a Conn,
    wtxn: &'a WriteTxn<'w>,
}

impl TxView<'_, '_> {
    pub fn get_attr(&self, eid: EntityId, attr: &str) -> Result<Option<Value>, DatalogError> {
        let aid = self.conn.attr_id(attr)?;
        self.conn.get_attr_rw(self.wtxn, eid, aid)
    }

    pub fn lookup(&self, attr: &str, val: &Value) -> Result<Option<EntityId>, DatalogError> {
        let aid = self.conn.attr_id(attr)?;
        let mut buf = Vec::new();
        self.conn.lookup_ave_rw(self.wtxn, aid, val, &mut buf)
    }

    /// Every value of `(eid, attr)`, for cardinality-many attributes.
    pub fn get_all(&self, eid: EntityId, attr: &str) -> Result<Vec<Value>, DatalogError> {
        let aid = self.conn.attr_id(attr)?;
        let mut pref = [0u8; 12];
        pref[..8].copy_from_slice(&eid.to_be_bytes());
        pref[8..12].copy_from_slice(&aid.to_be_bytes());
        let mut out = Vec::new();
        for res in self.conn.eav.prefix_iter_rw(self.wtxn, &pref)? {
            let (k, _) = res?;
            out.push(decode_value(&k[12..])?);
        }
        Ok(out)
    }
}

/// A user function callable from query clauses: `[(name ?x ...) ?out]`. `None` is nil.
/// Path selectors of a named idoc domain.
#[derive(Debug, Clone, Default)]
pub struct IdocDomain {
    pub indexed: Option<Vec<Vec<PathSeg>>>,
    pub excluded: Vec<Vec<PathSeg>>,
}

pub type QueryFn = Box<dyn Fn(&[Value]) -> Result<Option<Value>, DatalogError>>;

pub type TxFn = Box<dyn Fn(&TxView<'_, '_>, &[Value]) -> Result<Vec<TxItem>, DatalogError>>;

pub type EnsureFn = Box<dyn Fn(&TxView<'_, '_>, &[Value]) -> Result<bool, DatalogError>>;

/// One datom added (`added: true`) or retracted by a committed transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct TxDatum {
    pub eid: EntityId,
    pub attr: String,
    pub value: Value,
    pub added: bool,
}

/// What a committed transaction changed, delivered to listeners.
#[derive(Debug, Clone, PartialEq)]
pub struct TxReport {
    pub tx_data: Vec<TxDatum>,
}

#[derive(Default)]
struct TxState {
    /// Items as applied (calls expanded), kept only when a WAL is attached.
    logged: Option<Vec<TxItem>>,
    /// Ensure predicates to run once all items are applied.
    ensures: Vec<(String, Vec<TxVal>)>,
    changes: Vec<TxDatum>,
    touched: Vec<EntityId>,
    tempids: HashMap<String, EntityId>,
    eav_buf: Vec<u8>,
    ave_buf: Vec<u8>,
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
const APPLIED_LSN_KEY: &[u8] = b"__applied_lsn";

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
    /// `(target name, target attr, source attrs)` for each composite tuple attribute.
    tuple_specs: Vec<(String, AttrId, Vec<AttrId>)>,
    tuple_targets: HashSet<AttrId>,
    tx_fns: HashMap<String, TxFn>,
    ensure_fns: HashMap<String, EnsureFn>,
    idoc_idx: RawDb,
    idoc_jobs: RawDb,
    wal: std::sync::Mutex<Option<wal::Wal>>,
    /// Log size that triggers an automatic checkpoint after a committed write (0 = off).
    wal_checkpoint_bytes: std::sync::atomic::AtomicU64,
    listeners: Vec<(String, Box<dyn Fn(&TxReport)>)>,
    query_fns: HashMap<String, QueryFn>,
    idoc_domains: HashMap<String, IdocDomain>,
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
        let idoc_idx = store.open_db(&mut wtxn, "idoc_paths")?;
        let idoc_jobs = store.open_db(&mut wtxn, "idoc_jobs")?;
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

        for (name, &aid) in &attr_ids {
            attrs.entry(aid).or_insert_with(|| AttrSpec::new(name.clone()));
        }
        let mut tuple_specs = Vec::new();
        let mut tuple_targets = HashSet::new();
        for (&aid, spec) in &attrs {
            let Some(sources) = &spec.tuple_sources else { continue };
            let mut src_ids = Vec::with_capacity(sources.len());
            for name in sources {
                let sid = *attr_ids.get(name).ok_or_else(|| {
                    DatalogError::Schema(format!("tuple source {name} of {} is not in the schema", spec.name))
                })?;
                let src = attrs.get(&sid).ok_or_else(|| {
                    DatalogError::Schema(format!("tuple source {name} of {} is not in the schema", spec.name))
                })?;
                if src.cardinality == Cardinality::Many || src.tuple_sources.is_some() {
                    return Err(DatalogError::Schema(format!(
                        "tuple source {name} of {} must be a cardinality-one, non-tuple attribute",
                        spec.name
                    )));
                }
                src_ids.push(sid);
            }
            tuple_targets.insert(aid);
            tuple_specs.push((spec.name.clone(), aid, src_ids));
        }

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
            tuple_specs,
            tuple_targets,
            tx_fns: HashMap::new(),
            ensure_fns: HashMap::new(),
            idoc_idx,
            idoc_jobs,
            wal: std::sync::Mutex::new(None),
            wal_checkpoint_bytes: std::sync::atomic::AtomicU64::new(0),
            listeners: Vec::new(),
            query_fns: HashMap::new(),
            idoc_domains: HashMap::new(),
        })
    }

    /// Persist a consistent snapshot of `meta`, `eav`, `ave`, `vae`, and `vec_idx`
    /// from this connection into a portable SQLite file at `sqlite_path`.
    pub fn save_to_sqlite<P: AsRef<Path>>(
        &self,
        rtxn: &ReadTxn<'_>,
        sqlite_path: P,
    ) -> Result<(), DatalogError> {
        // Write to a sibling temp file and rename over the destination on success,
        // so a failed save never leaves a half-written or stale snapshot behind.
        let path = sqlite_path.as_ref();
        let tmp = sibling_tmp_path(path);
        if let Err(e) = self.write_sqlite_snapshot(rtxn, &tmp) {
            let _ = remove_sqlite_files(&tmp, &["", "-wal", "-shm"]);
            return Err(e);
        }
        // Stale sidecars of the old destination would be replayed onto the new file, so
        // drop them first. The old main file is left intact until rename replaces it
        // atomically, so any failure before this point keeps the previous snapshot.
        remove_sqlite_files(path, &["-wal", "-shm"])?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = remove_sqlite_files(&tmp, &["", "-wal", "-shm"]);
            return Err(dtlv_kv::KvError::from(e).into());
        }
        Ok(())
    }

    fn write_sqlite_snapshot(&self, rtxn: &ReadTxn<'_>, tmp: &Path) -> Result<(), DatalogError> {
        remove_sqlite_files(tmp, &["", "-wal", "-shm"])?;
        let dst = KvStore::open_sqlite(tmp)?;
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
        drop(dst); // closing the last connection checkpoints and removes the tmp WAL
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
                        dst.fts.index_entry(&mut fwtxn, &fts_key(eid, aid, &val), txt)?;
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

    /// Saves the cached vector graph so the next process's first large search skips the rebuild. The graph is also
    /// saved automatically on the first vector write after a rebuild, so calling this is only needed when no vector
    /// write follows a rebuild. A saved graph is ignored once it is older than the vector log retains.
    pub fn save_vector_graph(&self) -> Result<bool, DatalogError> {
        let mut wtxn = self.store.write_txn()?;
        let saved = self.vec_idx.save_graph(&mut wtxn)?;
        wtxn.commit()?;
        Ok(saved)
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

    /// Bulk-loads trusted `(entity, attribute, value)` datoms in one transaction; all of them apply or none do.
    /// Datoms are grouped by entity, and the usual schema checks still run.
    pub fn load_datoms(&self, datoms: &[(EntityId, String, Value)]) -> Result<(), DatalogError> {
        let mut by_eid: std::collections::BTreeMap<EntityId, Vec<(String, Value)>> = std::collections::BTreeMap::new();
        for (eid, attr, val) in datoms {
            by_eid.entry(*eid).or_default().push((attr.clone(), val.clone()));
        }
        let items: Vec<TxItem> = by_eid.into_iter().map(|(eid, pairs)| TxItem::Assert(Some(eid), pairs)).collect();
        self.transact(&items)?;
        Ok(())
    }

    /// Keeps the fresh-id counter above every explicitly supplied entity id, so later allocations never collide.
    fn bump_next_eid(&self, wtxn: &mut WriteTxn<'_>, eid: EntityId) -> Result<(), DatalogError> {
        let next = match self.meta.get_rw(wtxn, NEXT_EID_KEY)? {
            Some(b) if b.len() == 8 => u64::from_be_bytes(b.as_ref().try_into().unwrap()),
            _ => 1,
        };
        if eid >= next {
            self.meta.put(wtxn, NEXT_EID_KEY, &(eid + 1).to_be_bytes())?;
        }
        Ok(())
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

    fn remove_ea_values(
        &self,
        wtxn: &mut WriteTxn<'_>,
        eid: EntityId,
        aid: AttrId,
        spec: &AttrSpec,
        kbuf: &mut Vec<u8>,
        ave_buf: &mut Vec<u8>,
        changes: &mut Vec<TxDatum>,
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
            if let (true, Value::Doc(text)) = (spec.idoc, &old_val) {
                self.idoc_del(wtxn, aid, eid, text)?;
            }
            if spec.fulltext {
                self.fts.remove_entry(wtxn, &fts_key(eid, aid, &old_val))?;
            }
            changes.push(TxDatum { eid, attr: spec.name.clone(), value: old_val.clone(), added: false });
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
        changes: &mut Vec<TxDatum>,
    ) -> Result<(), DatalogError> {
        // Remove incoming references to this entity from other entities.
        // vae keys are target(8) | aid(4) | src(8); collect before deleting.
        let vae_prefix = eid.to_be_bytes();
        let mut incoming: Vec<(EntityId, AttrId)> = Vec::new();
        for r in self.vae.prefix_iter_rw(wtxn, &vae_prefix)? {
            let (k, _) = r?;
            if k.len() != 20 {
                return Err(CodecError::UnexpectedEof.into());
            }
            let aid = u32::from_be_bytes(k[8..12].try_into().unwrap());
            let src = u64::from_be_bytes(k[12..20].try_into().unwrap());
            incoming.push((src, aid));
        }
        for (src, aid) in incoming {
            let val = Value::Ref(eid);
            encode_eav_key(src, aid, &val, ave_buf);
            self.eav.delete(wtxn, ave_buf)?;
            changes.push(TxDatum { eid: src, attr: self.aid_names[&aid].clone(), value: val.clone(), added: false });
            if let Some(spec) = self.attrs.get(&aid) {
                if spec.index {
                    encode_ave_key(aid, &val, src, ave_buf);
                    self.ave.delete(wtxn, ave_buf)?;
                }
            }
            encode_vae_key(eid, aid, src, ave_buf);
            self.vae.delete(wtxn, ave_buf)?;
        }

        let prefix = eid.to_be_bytes();
        let entries: Vec<Vec<u8>> = self
            .eav
            .prefix_iter_rw(wtxn, &prefix)?
            .map(|r| r.map(|(k, _)| k.to_vec()))
            .collect::<Result<_, _>>()?;
        let mut component_children = Vec::new();
        for k in entries {
            let aid = u32::from_be_bytes(k[8..12].try_into().unwrap());
            let val = decode_value(&k[12..])?;
            self.eav.delete(wtxn, &k)?;
            changes.push(TxDatum { eid, attr: self.aid_names[&aid].clone(), value: val.clone(), added: false });
            if let Some(spec) = self.attrs.get(&aid) {
                if let (true, Value::Doc(text)) = (spec.idoc, &val) {
                    self.idoc_del(wtxn, aid, eid, text)?;
                }
                if spec.index {
                    encode_ave_key(aid, &val, eid, ave_buf);
                    self.ave.delete(wtxn, ave_buf)?;
                }
                if spec.fulltext {
                    self.fts.remove_entry(wtxn, &fts_key(eid, aid, &val))?;
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
        for child in component_children {
            self.retract_entity_recursive(wtxn, child, ave_buf, changes)?;
        }
        Ok(())
    }

    pub fn transact(&self, items: &[TxItem]) -> Result<Vec<EntityId>, DatalogError> {
        self.run_tx(items, None)
    }

    /// Applies one transaction. Live writes are logged to the WAL (when attached) before the LMDB commit;
    /// replayed writes (`replay: Some(lsn)`) are not logged again and record `lsn` as applied.
    fn run_tx(&self, items: &[TxItem], replay: Option<u64>) -> Result<Vec<EntityId>, DatalogError> {
        let mut wal_guard = if replay.is_none() {
            Some(self.wal.lock().unwrap_or_else(|e| e.into_inner()))
        } else {
            None
        };
        let logging = wal_guard.as_ref().is_some_and(|g| g.is_some());
        let mut wtxn = self.store.write_txn()?;
        let mut st = TxState { logged: logging.then(Vec::new), ..TxState::default() };
        for item in items {
            self.apply_item(&mut wtxn, item, &mut st)?;
        }
        self.recompute_tuples(&mut wtxn, &st.touched, &mut st.changes)?;
        self.check_ensures(&wtxn, &st.tempids, &st.ensures)?;
        let mut logged_lsn = None;
        if let Some(lsn) = replay {
            self.set_applied_lsn(&mut wtxn, lsn)?;
        } else if let (Some(guard), Some(logged)) = (wal_guard.as_mut(), st.logged.as_deref()) {
            if let Some(wal) = guard.as_mut() {
                if !logged.is_empty() {
                    let lsn = wal.append(logged)?;
                    self.set_applied_lsn(&mut wtxn, lsn)?;
                    logged_lsn = Some(lsn);
                }
            }
        }
        if let Err(e) = commit_txn(wtxn) {
            // The record is in the log but the store did not commit it: mark it aborted so replay skips it.
            if let (Some(lsn), Some(guard)) = (logged_lsn, wal_guard.as_mut()) {
                if let Some(wal) = guard.as_mut() {
                    let _ = wal.append_abort(lsn);
                }
            }
            return Err(e.into());
        }
        if !st.changes.is_empty() && !self.listeners.is_empty() {
            let report = TxReport { tx_data: std::mem::take(&mut st.changes) };
            for (_, f) in &self.listeners {
                f(&report);
            }
        }
        // Checkpoint after listeners are notified. The transaction is already committed, so a failed
        // checkpoint returns an error but does not roll it back.
        if logged_lsn.is_some() {
            let threshold = self.wal_checkpoint_bytes.load(std::sync::atomic::Ordering::Relaxed);
            if let Some(wal) = wal_guard.as_mut().and_then(|g| g.as_mut()) {
                if threshold > 0 && wal.len()? >= threshold {
                    self.checkpoint_locked(wal)?;
                }
            }
        }
        Ok(st.touched)
    }

    fn set_applied_lsn(&self, wtxn: &mut WriteTxn<'_>, lsn: u64) -> Result<(), DatalogError> {
        self.meta.put(wtxn, APPLIED_LSN_KEY, &lsn.to_be_bytes())?;
        Ok(())
    }

    /// Last WAL record applied to the store (0 if none).
    pub fn applied_lsn(&self) -> Result<u64, DatalogError> {
        let rtxn = self.store.read_txn()?;
        Ok(match self.meta.get(&rtxn, APPLIED_LSN_KEY)? {
            Some(b) if b.len() == 8 => u64::from_be_bytes(b.as_ref().try_into().unwrap()),
            _ => 0,
        })
    }

    /// Opens a connection with a write-ahead log in directory `dir`. Records the store has not applied are
    /// replayed before the connection is returned, so a crash after a log append loses nothing.
    pub fn open_wal<P: AsRef<Path>, Q: AsRef<Path>>(
        path: P,
        dir: Q,
        schema: Vec<AttrSpec>,
        durability: Durability,
    ) -> Result<Self, DatalogError> {
        let conn = Self::open(path, schema)?;
        let applied = conn.applied_lsn()?;
        let (log, records) = wal::Wal::open(&dir.as_ref().join("wal.log"), durability, applied)?;
        for (lsn, items) in records {
            if lsn > applied {
                conn.run_tx(&items, Some(lsn))?;
            }
        }
        *conn.wal.lock().unwrap_or_else(|e| e.into_inner()) = Some(log);
        Ok(conn)
    }

    /// Forces logged transactions to disk. Only meaningful for the relaxed profile.
    pub fn sync_wal(&self) -> Result<(), DatalogError> {
        if let Some(log) = self.wal.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            log.sync()?;
        }
        Ok(())
    }

    /// Makes the store durable, then drops the log, which is no longer needed for recovery.
    pub fn checkpoint(&self) -> Result<(), DatalogError> {
        if let Some(log) = self.wal.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            self.checkpoint_locked(log)?;
        }
        Ok(())
    }

    /// After a committed transaction leaves the log at or above `bytes`, the connection checkpoints
    /// (syncs the store, then truncates the log). `None` turns this off, which is the default. If the
    /// checkpoint fails, the transaction stays committed and `run_tx` returns the error.
    pub fn set_wal_checkpoint_bytes(&self, bytes: Option<u64>) {
        self.wal_checkpoint_bytes.store(bytes.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
    }

    /// Store sync then log truncate. The caller holds the WAL mutex, so `run_tx` can call it too.
    fn checkpoint_locked(&self, wal: &mut wal::Wal) -> Result<(), DatalogError> {
        self.store.sync()?;
        wal.truncate()?;
        Ok(())
    }

    fn reject_tuple_writes<'n>(&self, names: impl IntoIterator<Item = &'n str>) -> Result<(), DatalogError> {
        for name in names {
            if let Some(aid) = self.attr_ids.get(name) {
                if self.tuple_targets.contains(aid) {
                    return Err(DatalogError::TupleWrite(name.to_owned()));
                }
            }
        }
        Ok(())
    }

    fn apply_item(&self, wtxn: &mut WriteTxn<'_>, item: &TxItem, st: &mut TxState) -> Result<(), DatalogError> {
        if let Some(log) = st.logged.as_mut() {
            if !matches!(item, TxItem::Call(..) | TxItem::Ensure(..)) {
                log.push(item.clone());
            }
        }
        match item {
            TxItem::Ensure(name, args) => st.ensures.push((name.clone(), args.clone())),
            TxItem::Assert(explicit_eid, pairs) => {
                self.reject_tuple_writes(pairs.iter().map(|(a, _)| a.as_str()))?;
                let mut resolved_eid = *explicit_eid;
                if resolved_eid.is_none() {
                    for (attr_name, val) in pairs {
                        let aid = self.attr_id(attr_name)?;
                        let spec = &self.attrs[&aid];
                        if spec.unique == Some(Unique::Identity) {
                            if let Some(found) = self.lookup_ave_rw(&*wtxn, aid, val, &mut st.ave_buf)? {
                                resolved_eid = Some(found);
                                break;
                            }
                        }
                    }
                }
                let eid = match resolved_eid {
                    Some(id) => id,
                    None => self.alloc_eid(wtxn)?,
                };
                self.bump_next_eid(wtxn, eid)?;
                st.touched.push(eid);

                for (attr_name, val) in pairs {
                    self.assert_datom(wtxn, eid, attr_name, val, &mut st.eav_buf, &mut st.ave_buf, &mut st.changes)?;
                }
            }
            TxItem::AssertRef(eref, pairs) => {
                self.reject_tuple_writes(pairs.iter().map(|(a, _)| a.as_str()))?;
                let eid = match eref {
                    EntityRef::Id(id) => {
                        self.bump_next_eid(wtxn, *id)?;
                        *id
                    }
                    EntityRef::TempId(tid) => match st.tempids.get(tid) {
                        Some(&id) => id,
                        None => {
                            let id = self.alloc_eid(wtxn)?;
                            st.tempids.insert(tid.clone(), id);
                            id
                        }
                    },
                    EntityRef::LookupRef(attr_name, val) => {
                        let aid = self.attr_id(attr_name)?;
                        match self.lookup_ave_rw(&*wtxn, aid, val, &mut st.ave_buf)? {
                            Some(found) => found,
                            None => self.alloc_eid(wtxn)?,
                        }
                    }
                    EntityRef::New => {
                        let mut found_id = None;
                        for (attr_name, txv) in pairs {
                            if let TxVal::Val(val) = txv {
                                let aid = self.attr_id(attr_name)?;
                                if self.attrs[&aid].unique == Some(Unique::Identity) {
                                    if let Some(f) = self.lookup_ave_rw(&*wtxn, aid, val, &mut st.ave_buf)? {
                                        found_id = Some(f);
                                        break;
                                    }
                                }
                            }
                        }
                        match found_id {
                            Some(id) => id,
                            None => self.alloc_eid(wtxn)?,
                        }
                    }
                };
                st.touched.push(eid);

                for (attr_name, txv) in pairs {
                    let resolved_val = match txv {
                        TxVal::Val(v) => v.clone(),
                        TxVal::TempRef(tid) => {
                            let target = match st.tempids.get(tid) {
                                Some(&id) => id,
                                None => {
                                    let id = self.alloc_eid(wtxn)?;
                                    st.tempids.insert(tid.clone(), id);
                                    id
                                }
                            };
                            Value::Ref(target)
                        }
                        TxVal::LookupRef(lattr, lval) => {
                            let laid = self.attr_id(lattr)?;
                            let target = match self.lookup_ave_rw(&*wtxn, laid, lval, &mut st.ave_buf)? {
                                Some(id) => id,
                                None => self.alloc_eid(wtxn)?,
                            };
                            Value::Ref(target)
                        }
                    };
                    self.assert_datom(wtxn, eid, attr_name, &resolved_val, &mut st.eav_buf, &mut st.ave_buf, &mut st.changes)?;
                }
            }
            TxItem::Retract(eid, attr_name, val) => {
                self.reject_tuple_writes([attr_name.as_str()])?;
                let aid = self.attr_id(attr_name)?;
                let spec = &self.attrs[&aid];
                encode_eav_key(*eid, aid, val, &mut st.eav_buf);
                let mut stored = if self.eav.delete(wtxn, &st.eav_buf)? { Some(val.clone()) } else { None };
                // Legacy datoms may hold the other sign of zero.
                if let (None, Value::F64(x)) = (&stored, val) {
                    if *x == 0.0 {
                        let other = Value::F64(-*x);
                        encode_eav_key(*eid, aid, &other, &mut st.eav_buf);
                        if self.eav.delete(wtxn, &st.eav_buf)? {
                            stored = Some(other);
                        }
                    }
                }
                if let Some(stored) = stored {
                    st.changes.push(TxDatum { eid: *eid, attr: attr_name.clone(), value: stored, added: false });
                    if let (true, Value::Doc(text)) = (spec.idoc, val) {
                        self.idoc_del(wtxn, aid, *eid, text)?;
                    }
                }
                if spec.index {
                    encode_ave_key(aid, val, *eid, &mut st.ave_buf);
                    self.ave.delete(wtxn, &st.ave_buf)?;
                }
                if let Value::Ref(target) = val {
                    encode_vae_key(*target, aid, *eid, &mut st.ave_buf);
                    self.vae.delete(wtxn, &st.ave_buf)?;
                }
                if spec.fulltext {
                    self.fts.remove_entry(wtxn, &fts_key(*eid, aid, val))?;
                }
                st.touched.push(*eid);
            }
            TxItem::RetractAttribute(eid, attr_name) => {
                self.reject_tuple_writes([attr_name.as_str()])?;
                let aid = self.attr_id(attr_name)?;
                let spec = &self.attrs[&aid];
                self.remove_ea_values(wtxn, *eid, aid, spec, &mut st.eav_buf, &mut st.ave_buf, &mut st.changes)?;
                st.touched.push(*eid);
            }
            TxItem::RetractEntity(eid) => {
                self.retract_entity_recursive(wtxn, *eid, &mut st.ave_buf, &mut st.changes)?;
            }
            TxItem::Cas(eid, attr_name, expected, new_val) => {
                self.reject_tuple_writes([attr_name.as_str()])?;
                let aid = self.attr_id(attr_name)?;
                let cur = self.get_attr_rw(&*wtxn, *eid, aid)?;
                if cur.as_ref() != Some(expected) {
                    return Err(DatalogError::CasFailed {
                        eid: *eid,
                        attr: attr_name.clone(),
                    });
                }
                self.assert_datom(wtxn, *eid, attr_name, new_val, &mut st.eav_buf, &mut st.ave_buf, &mut st.changes)?;
                st.touched.push(*eid);
            }
            TxItem::PatchIdoc(eid, attr_name, ops) => {
                self.reject_tuple_writes([attr_name.as_str()])?;
                let aid = self.attr_id(attr_name)?;
                if !self.attrs.get(&aid).is_some_and(|s| s.idoc && s.cardinality == Cardinality::One) {
                    return Err(DatalogError::Type(format!("{attr_name} is not a cardinality-one idoc attribute")));
                }
                let current = match self.get_attr_rw(&*wtxn, *eid, aid)? {
                    Some(Value::Doc(text)) => text,
                    _ => return Err(DatalogError::Type(format!("entity {eid} has no {attr_name} document"))),
                };
                let patched = Value::Doc(apply_patch(&current, ops)?);
                // Cardinality one: assert_datom retracts the old document and indexes the new one.
                self.assert_datom(wtxn, *eid, attr_name, &patched, &mut st.eav_buf, &mut st.ave_buf, &mut st.changes)?;
                st.touched.push(*eid);
            }
            TxItem::PatchIdocOf(eid, attr_name, old, ops) => {
                self.reject_tuple_writes([attr_name.as_str()])?;
                let aid = self.attr_id(attr_name)?;
                if !self.attrs.get(&aid).is_some_and(|s| s.idoc) {
                    return Err(DatalogError::Type(format!("{attr_name} is not an idoc attribute")));
                }
                let Value::Doc(old_text) = old else {
                    return Err(DatalogError::Type("patchIdoc old value must be a document".into()));
                };
                let mut key = Vec::new();
                encode_eav_key(*eid, aid, old, &mut key);
                if self.eav.get_rw(&*wtxn, &key)?.is_none() {
                    return Err(DatalogError::Type(format!("entity {eid} has no matching {attr_name} document")));
                }
                let patched = Value::Doc(apply_patch(old_text, ops)?);
                // Retract the old document (and its index entries), then assert the patched one.
                self.eav.delete(wtxn, &key)?;
                if self.attrs[&aid].index {
                    encode_ave_key(aid, old, *eid, &mut st.ave_buf);
                    self.ave.delete(wtxn, &st.ave_buf)?;
                }
                self.idoc_del(wtxn, aid, *eid, old_text)?;
                st.changes.push(TxDatum { eid: *eid, attr: attr_name.clone(), value: old.clone(), added: false });
                self.assert_datom(wtxn, *eid, attr_name, &patched, &mut st.eav_buf, &mut st.ave_buf, &mut st.changes)?;
                st.touched.push(*eid);
            }
            TxItem::Call(name, args) => {
                let f = self
                    .tx_fns
                    .get(name)
                    .ok_or_else(|| DatalogError::UnknownTxFn(name.clone()))?;
                let produced = f(&TxView { conn: self, wtxn: &*wtxn }, args)?;
                for it in &produced {
                    if matches!(it, TxItem::Call(..)) {
                        return Err(DatalogError::Function(format!("transaction function {name} returned a call")));
                    }
                    self.apply_item(wtxn, it, st)?;
                }
            }
        }
        Ok(())
    }

    /// Runs the ensure predicates of a transaction against its uncommitted state. Any false result, unknown
    /// predicate or unresolvable argument aborts the transaction before anything is logged or committed.
    fn check_ensures(
        &self,
        wtxn: &WriteTxn<'_>,
        tempids: &HashMap<String, EntityId>,
        ensures: &[(String, Vec<TxVal>)],
    ) -> Result<(), DatalogError> {
        let view = TxView { conn: self, wtxn };
        for (name, args) in ensures {
            let mut vals = Vec::with_capacity(args.len());
            for arg in args {
                vals.push(match arg {
                    TxVal::Val(v) => v.clone(),
                    TxVal::TempRef(t) => {
                        let id = tempids.get(t).ok_or_else(|| DatalogError::Type(format!("ensure {name}: unresolved tempid {t}")))?;
                        Value::Ref(*id)
                    }
                    TxVal::LookupRef(attr, v) => {
                        let id = view.lookup(attr, v)?.ok_or_else(|| DatalogError::Type(format!("ensure {name}: no entity for {attr}")))?;
                        Value::Ref(id)
                    }
                });
            }
            let f = self.ensure_fns.get(name).ok_or_else(|| DatalogError::UnknownEnsureFn(name.clone()))?;
            if !f(&view, &vals)? {
                return Err(DatalogError::EnsureFailed { pred: name.clone(), args: vals });
            }
        }
        Ok(())
    }

    /// Re-derives every composite tuple attribute of the touched entities from its source attributes.
    fn recompute_tuples(
        &self,
        wtxn: &mut WriteTxn<'_>,
        touched: &[EntityId],
        changes: &mut Vec<TxDatum>,
    ) -> Result<(), DatalogError> {
        if self.tuple_specs.is_empty() {
            return Ok(());
        }
        let mut eids = touched.to_vec();
        eids.sort_unstable();
        eids.dedup();
        let (mut eav_buf, mut ave_buf) = (Vec::new(), Vec::new());
        for eid in eids {
            for (name, target, sources) in &self.tuple_specs {
                let mut parts = Vec::with_capacity(sources.len());
                let mut complete = true;
                for &s in sources {
                    match self.get_attr_rw(&*wtxn, eid, s)? {
                        Some(v) => parts.push(v),
                        None => {
                            complete = false;
                            break;
                        }
                    }
                }
                let old = self.get_attr_rw(&*wtxn, eid, *target)?;
                let new = complete.then(|| Value::Tuple(parts));
                if old != new {
                    if old.is_some() {
                        let spec = &self.attrs[target];
                        self.remove_ea_values(wtxn, eid, *target, spec, &mut eav_buf, &mut ave_buf, changes)?;
                    }
                    if let Some(v) = new {
                        self.assert_datom(wtxn, eid, name, &v, &mut eav_buf, &mut ave_buf, changes)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Reads the single current value of `(eid, aid)` inside a write transaction.
    fn get_attr_rw(&self, wtxn: &WriteTxn<'_>, eid: EntityId, aid: AttrId) -> Result<Option<Value>, DatalogError> {
        let mut pref = [0u8; 12];
        pref[..8].copy_from_slice(&eid.to_be_bytes());
        pref[8..12].copy_from_slice(&aid.to_be_bytes());
        if let Some(res) = self.eav.prefix_iter_rw(wtxn, &pref)?.next() {
            let (k, _) = res?;
            return Ok(Some(decode_value(&k[12..])?));
        }
        Ok(None)
    }

    /// Sets the path selectors of a named idoc domain (`indexed` is `None` to index every path). Domains are
    /// not persisted: re-apply them on open, as with the schema. Refused once documents are indexed, since the
    /// index would no longer match the selectors.
    pub fn set_idoc_domain(&mut self, name: &str, indexed: Option<Vec<Vec<PathSeg>>>, excluded: Vec<Vec<PathSeg>>) -> Result<(), DatalogError> {
        let rtxn = self.store.read_txn()?;
        if self.idoc_idx.iter(&rtxn)?.next().is_some() {
            return Err(DatalogError::Type("idoc domains must be set before documents are indexed".into()));
        }
        drop(rtxn);
        self.idoc_domains.insert(name.to_owned(), IdocDomain { indexed, excluded });
        Ok(())
    }

    /// Registers a function callable from query clauses by name, e.g. `[(double ?n) ?d]`.
    /// Unknown names are reported when the clause is evaluated, since the parser does not see the registry.
    pub fn register_query_fn<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&[Value]) -> Result<Option<Value>, DatalogError> + 'static,
    {
        self.query_fns.insert(name.to_owned(), Box::new(f));
    }

    /// Registers a listener that receives a `TxReport` after every committed transaction that changes data.
    /// Re-registering a key replaces the earlier listener.
    pub fn listen<F>(&mut self, key: &str, f: F)
    where
        F: Fn(&TxReport) + 'static,
    {
        self.listeners.retain(|(k, _)| k != key);
        self.listeners.push((key.to_owned(), Box::new(f)));
    }

    /// Removes a listener by key, returning whether one was registered.
    pub fn unlisten(&mut self, key: &str) -> bool {
        let before = self.listeners.len();
        self.listeners.retain(|(k, _)| k != key);
        self.listeners.len() != before
    }

    /// Registers a named transaction function, invoked with `TxItem::Call`.
    pub fn register_tx_fn<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&TxView<'_, '_>, &[Value]) -> Result<Vec<TxItem>, DatalogError> + 'static,
    {
        self.tx_fns.insert(name.to_owned(), Box::new(f));
    }

    /// Registers a `:db/ensure` predicate. Arguments are resolved to values before the call (tempids and lookup
    /// refs become `Value::Ref`). Returning `Ok(false)` fails the transaction. Not persisted: re-register on open.
    pub fn register_ensure_fn<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&TxView<'_, '_>, &[Value]) -> Result<bool, DatalogError> + 'static,
    {
        self.ensure_fns.insert(name.to_owned(), Box::new(f));
    }

    fn assert_datom(
        &self,
        wtxn: &mut WriteTxn<'_>,
        eid: EntityId,
        attr_name: &str,
        val: &Value,
        eav_buf: &mut Vec<u8>,
        ave_buf: &mut Vec<u8>,
        changes: &mut Vec<TxDatum>,
    ) -> Result<(), DatalogError> {
        let aid = self.attr_id(attr_name)?;
        let spec = &self.attrs[&aid];
        if spec.idoc && !matches!(val, Value::Doc(_)) {
            return Err(DatalogError::Type(format!("{attr_name} is an idoc attribute; use idoc_value")));
        }
        // Written zeros are stored as +0.0. A legacy -0.0 on a many-valued attribute is replaced, not duplicated.
        let canon_zero;
        let val = if matches!(val, Value::F64(x) if *x == 0.0) {
            canon_zero = Value::F64(0.0);
            if spec.cardinality == Cardinality::Many {
                encode_eav_key(eid, aid, &Value::F64(-0.0), eav_buf);
                self.eav.delete(wtxn, eav_buf)?;
            }
            &canon_zero
        } else {
            val
        };

        if spec.unique.is_some() {
            if let Some(existing) = self.lookup_ave_rw(wtxn, aid, val, ave_buf)? {
                // An identity value resolves to its entity before this point, so a different entity here
                // means an explicit id conflicts with the unique value: reject it for both kinds.
                if existing != eid {
                    return Err(DatalogError::UniqueViolation {
                        attr: attr_name.to_owned(),
                        existing,
                    });
                }
            }
        }

        if spec.cardinality == Cardinality::One {
            self.remove_ea_values(wtxn, eid, aid, spec, eav_buf, ave_buf, changes)?;
        }

        encode_eav_key(eid, aid, val, eav_buf);
        self.eav.put(wtxn, eav_buf, &[])?;
        changes.push(TxDatum { eid, attr: attr_name.to_owned(), value: val.clone(), added: true });
        if let (true, Value::Doc(text)) = (spec.idoc, val) {
            self.idoc_put(wtxn, aid, eid, text)?;
        }

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
                self.fts.index_entry(wtxn, &fts_key(eid, aid, val), text)?;
            }
        }
        Ok(())
    }

    /// Analysis settings of the connection's full-text index.
    pub fn fulltext_config(&self) -> FtsConfig {
        self.fts.config()
    }

    /// Changes the full-text analysis settings. Only allowed before any document is indexed.
    pub fn set_fulltext_config(&mut self, cfg: FtsConfig) -> Result<(), DatalogError> {
        let mut wtxn = self.store.write_txn()?;
        self.fts.set_config(&mut wtxn, cfg)?;
        wtxn.commit()?;
        Ok(())
    }

    /// Replaces the full-text analyzer (`None` restores the default). The analyzer is not persisted: register
    /// the same analyzer after every open, before indexing or searching. Changing it after documents are
    /// indexed makes old and new terms disagree (re-index to fix).
    pub fn set_fulltext_analyzer(&mut self, a: Option<dtlv_search::Analyzer>) {
        self.fts.set_analyzer(a);
    }

    /// Evaluates a boolean/phrase full-text query tree, returning `(entity, score)` pairs.
    pub fn fulltext_query(
        &self,
        rtxn: &ReadTxn<'_>,
        query: &FtsQuery,
        limit: usize,
    ) -> Result<Vec<(EntityId, f64)>, DatalogError> {
        Ok(self.fts.search_query(rtxn, query, limit)?)
    }

    /// Like `fulltext_query`, but returns ranks `offset .. offset + limit`. `filter` runs before ranking,
    /// so a filtered page is full whenever enough entities match. Pages are stable for an unchanged index.
    pub fn fulltext_query_page(
        &self,
        rtxn: &ReadTxn<'_>,
        query: &FtsQuery,
        offset: usize,
        limit: usize,
        filter: Option<&dyn Fn(dtlv_codec::EntityId) -> bool>,
    ) -> Result<Vec<(dtlv_codec::EntityId, f64)>, DatalogError> {
        Ok(self.fts.search_page(rtxn, query, offset, limit, filter)?)
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
                    let hits = self.fts.search(rtxn, q_str, usize::MAX)?;
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
                        let entity_val;
                        let bound_v: Option<&Value> = match v_term {
                            Term::Val(v) => Some(v),
                            Term::Entity(id) => {
                                entity_val = Value::Ref(*id);
                                Some(&entity_val)
                            }
                            Term::Var(var) => env.get(var),
                        };

                        match (bound_e, bound_v) {
                            (Some(eid), Some(val)) => {
                                if self.eav_has(rtxn, eid, aid, val, &mut buf)? {
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
                            (None, Some(val)) if spec.index && probe_key(val).is_some() => {
                                encode_av_prefix(aid, val, &mut buf);
                                for item in self.ave.prefix_iter(rtxn, &buf)? {
                                    let (k, _) = item?;
                                    let eid = decode_ave_entity(&k)?;
                                    let mut e2 = env.clone();
                                    if let Term::Var(ename) = e_term {
                                        if let Some(existing) = e2.get(ename) {
                                            if existing != &Value::Ref(eid) {
                                                continue;
                                            }
                                        } else {
                                            e2.insert(ename.clone(), Value::Ref(eid));
                                        }
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
                                        if let Some(existing) = e2.get(ename) {
                                            if existing != &Value::Ref(eid) {
                                                continue;
                                            }
                                        } else {
                                            e2.insert(ename.clone(), Value::Ref(eid));
                                        }
                                    }
                                    if let Term::Var(vname) = v_term {
                                        if let Some(existing) = e2.get(vname) {
                                            if existing != &val {
                                                continue;
                                            }
                                        } else {
                                            e2.insert(vname.clone(), val.clone());
                                        }
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
        let typed: Vec<QueryInput> = inputs.iter().cloned().map(QueryInput::Scalar).collect();
        self.q_str_inputs(rtxn, query_str, &typed, rules_str)
    }

    /// Like `q_str_with_rules`, but each `:in` input may be a scalar, collection, tuple, or relation.
    pub fn q_str_inputs(
        &self,
        rtxn: &ReadTxn<'_>,
        query_str: &str,
        inputs: &[QueryInput],
        rules_str: &str,
    ) -> Result<QueryResult, DatalogError> {
        let mut parsed = parse_query_inputs(query_str, inputs)?;
        let seed = seed_envs(&parsed.in_bindings, inputs)?;
        let bound: HashSet<String> = parsed.in_vars.iter().cloned().collect();
        parsed.clauses = self.plan_clauses(rtxn, &bound, std::mem::take(&mut parsed.clauses))?;
        let rules = parse_rules(rules_str)?;
        let mut rule_map: HashMap<String, Vec<RuleDef>> = HashMap::new();
        for r in rules {
            rule_map.entry(r.name.clone()).or_default().push(r);
        }
        let (eval_rules, clauses) = self.demand_rules(&rule_map, std::mem::take(&mut parsed.clauses));
        parsed.clauses = clauses;
        let rule_tables = if eval_rules.is_empty() {
            HashMap::new()
        } else {
            self.eval_all_rules(rtxn, &eval_rules)?
        };

        let envs = self.eval_clauses(rtxn, &parsed.clauses, seed, &rule_tables)?;
        self.finalize_query_result(rtxn, &parsed, envs)
    }

    /// Magic-set rewriting of the query's rule calls. Each call with a constant argument is replaced by a call to
    /// its rewritten, demand-driven rules. Returns the rules to evaluate (rewritten rules plus the original rules
    /// the remaining calls reach) and the rewritten clauses.
    fn demand_rules(
        &self,
        rule_map: &HashMap<String, Vec<RuleDef>>,
        clauses: Vec<Clause>,
    ) -> (HashMap<String, Vec<RuleDef>>, Vec<Clause>) {
        if rule_map.is_empty() {
            return (HashMap::new(), clauses);
        }
        let mut rewritten_rules: HashMap<String, Vec<RuleDef>> = HashMap::new();
        let mut clauses = clauses;
        let mut remaining: HashSet<String> = HashSet::new();
        for clause in clauses.iter_mut() {
            if let Clause::RuleCall(p, args) = clause {
                match magic::rewrite(rule_map, p, args) {
                    Some(rw) => {
                        for (name, defs) in rw.rules {
                            rewritten_rules.entry(name).or_default().extend(defs);
                        }
                        *p = rw.goal;
                    }
                    None => {
                        remaining.insert(p.clone());
                    }
                }
            } else {
                magic::called_rules(std::slice::from_ref(&*clause), &mut remaining);
            }
        }
        // Original rules are needed only when a call was not rewritten, or when a rewritten rule still calls one
        // (rewriting declined for it). Rewritten bodies name adorned rules, so they add no originals of their own.
        let mut needed = magic::reachable(rule_map, remaining);
        let mut eval: HashMap<String, Vec<RuleDef>> = HashMap::new();
        for name in needed.drain() {
            if let Some(defs) = rule_map.get(&name) {
                eval.insert(name, defs.clone());
            }
        }
        for (name, defs) in rewritten_rules {
            eval.insert(name, defs);
        }
        (eval, clauses)
    }

    /// Runs a nested query on its own bindings and returns its rows.
    fn run_nested(
        &self,
        rtxn: &ReadTxn<'_>,
        query: &ParsedQuery,
        inputs: Vec<QueryInput>,
        rule_tables: &HashMap<String, Vec<Vec<Value>>>,
    ) -> Result<Vec<Vec<Value>>, DatalogError> {
        let seed = seed_envs(&query.in_bindings, &inputs)?;
        let envs = self.eval_clauses(rtxn, &query.clauses, seed, rule_tables)?;
        Ok(match self.finalize_query_result(rtxn, query, envs)? {
            QueryResult::Rel(rows) => rows,
            QueryResult::Coll(vals) => vals.into_iter().map(|v| vec![v]).collect(),
            QueryResult::Scalar(v) => v.into_iter().map(|v| vec![v]).collect(),
            QueryResult::Tuple(t) => t.into_iter().collect(),
            QueryResult::Maps(_) | QueryResult::PulledColl(_) => {
                return Err(DatalogError::Function("nested queries must return plain values".into()))
            }
        })
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
            // Set semantics: aggregate over the distinct bindings of the find and :with variables, so
            // duplicate derivations (e.g. through `or`) do not inflate counts.
            let proj: Vec<&str> = parsed
                .find
                .iter()
                .map(|e| match e {
                    FindElem::Var(v) | FindElem::Pull(v, _) | FindElem::Agg(_, v) => v.as_str(),
                })
                .chain(parsed.with_vars.iter().map(|v| v.as_str()))
                .collect();
            let mut seen: HashSet<Vec<Option<Vec<u8>>>> = HashSet::new();
            let distinct: Vec<HashMap<String, Value>> = envs
                .into_iter()
                .filter(|env| {
                    let key = proj
                        .iter()
                        .map(|v| {
                            env.get(*v).map(|val| {
                                let mut buf = Vec::new();
                                dtlv_codec::encode_value_indexed(val, &mut buf);
                                buf
                            })
                        })
                        .collect();
                    seen.insert(key)
                })
                .collect();

            let mut rows = aggregate_envs(&distinct, &parsed.find)?;

            // :order-by names a find variable or a column index; the aggregated rows are sorted by it.
            if !parsed.order_by.is_empty() {
                let keys: Vec<(usize, bool)> = parsed
                    .order_by
                    .iter()
                    .filter_map(|spec| {
                        let col = match &spec.key {
                            OrderKey::Col(i) => Some(*i),
                            OrderKey::Var(v) => parsed
                                .find
                                .iter()
                                .position(|e| matches!(e, FindElem::Var(x) | FindElem::Pull(x, _) if x == v)),
                        };
                        col.filter(|c| *c < parsed.find.len()).map(|c| (c, spec.desc))
                    })
                    .collect();
                rows.sort_by(|a, b| {
                    for &(c, desc) in &keys {
                        if let (Some(va), Some(vb)) = (a.get(c), b.get(c)) {
                            if let Some(ord) = cmp_vals(va, vb) {
                                if ord != std::cmp::Ordering::Equal {
                                    return if desc { ord.reverse() } else { ord };
                                }
                            }
                        }
                    }
                    std::cmp::Ordering::Equal
                });
            }
            let start = parsed.offset.unwrap_or(0).max(0) as usize;
            let rows: Vec<Vec<Value>> = match parsed.limit {
                Some(lim) if lim >= 0 => rows.into_iter().skip(start).take(lim as usize).collect(),
                _ => rows.into_iter().skip(start).collect(),
            };
            // :keys shapes aggregate rows the same way as plain rows.
            if let Some(keys) = &parsed.return_keys {
                let maps = rows
                    .into_iter()
                    .map(|r| keys.iter().cloned().zip(r).collect::<BTreeMap<_, _>>())
                    .collect();
                return Ok(QueryResult::Maps(maps));
            }
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

        // Set semantics on the projected key, applied after :order-by so the sort order of
        // the first occurrence is kept. Offset and limit count distinct rows, and the scan
        // stops once the page is full, so a limited query does not format every match.
        // Pull elements key on their entity var so each pulled entity stays distinct.
        let start = parsed.offset.unwrap_or(0).max(0) as usize;
        let limit = match parsed.limit {
            Some(lim) if lim >= 0 => Some(lim as usize),
            _ => None,
        };
        let mut seen: HashSet<String> = HashSet::new();
        let mut envs_slice: Vec<HashMap<String, Value>> = Vec::new();
        for env in envs {
            let key: Vec<Option<&Value>> = parsed
                .find
                .iter()
                .map(|elem| match elem {
                    FindElem::Var(v) | FindElem::Pull(v, _) => env.get(v),
                    FindElem::Agg(_, _) => None,
                })
                .collect();
            if !seen.insert(format!("{:?}", key)) {
                continue;
            }
            if seen.len() <= start {
                continue;
            }
            if limit.is_some_and(|lim| envs_slice.len() >= lim) {
                break;
            }
            envs_slice.push(env);
        }

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
            rows.push(row);
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
        // Semi-naive evaluation. `tables[name]` holds every tuple derived so far; `tables[delta_key(name)]` holds
        // the tuples new in the previous round. A recursive rule is re-derived once per recursive call, with that
        // call reading the delta, so each round only produces tuples that use something new.
        let delta_key = |n: &str| format!("\u{1}delta:{n}");
        let mut tables: HashMap<String, Vec<Vec<Value>>> = HashMap::new();
        let mut seen: HashMap<String, HashSet<String>> = HashMap::new();
        for name in rule_map.keys() {
            tables.insert(name.clone(), Vec::new());
            tables.insert(delta_key(name), Vec::new());
            seen.insert(name.clone(), HashSet::new());
        }
        let recursive_calls = |body: &[Clause]| -> Vec<usize> {
            body.iter()
                .enumerate()
                .filter_map(|(i, c)| match c {
                    Clause::RuleCall(n, _) if rule_map.contains_key(n) => Some(i),
                    _ => None,
                })
                .collect()
        };
        fn mentions_rule(c: &Clause, rule_map: &HashMap<String, Vec<RuleDef>>) -> bool {
            match c {
                Clause::RuleCall(n, _) => rule_map.contains_key(n),
                Clause::Or(bs) | Clause::OrJoin(_, bs) => bs.iter().flatten().any(|c| mentions_rule(c, rule_map)),
                Clause::Not(inner) | Clause::NotJoin(_, inner) => inner.iter().any(|c| mentions_rule(c, rule_map)),
                _ => false,
            }
        }

        let mut first = true;
        loop {
            let mut produced: HashMap<String, Vec<Vec<Value>>> = HashMap::new();
            for (name, defs) in rule_map {
                for def in defs {
                    let calls = recursive_calls(&def.body);
                    let variants: Vec<Vec<Clause>> = if first {
                        vec![def.body.clone()]
                    } else if !calls.is_empty() {
                        calls
                            .iter()
                            .map(|&j| {
                                let mut body = def.body.clone();
                                if let Clause::RuleCall(n, args) = &body[j] {
                                    body[j] = Clause::RuleCall(delta_key(n), args.clone());
                                }
                                body
                            })
                            .collect()
                    } else if def.body.iter().any(|c| mentions_rule(c, rule_map)) {
                        // Recursion hidden inside `or`/`not` cannot be redirected to the delta; re-derive in full.
                        vec![def.body.clone()]
                    } else {
                        continue;
                    };
                    for body in variants {
                        let envs = self.eval_clauses(rtxn, &body, vec![HashMap::new()], &tables)?;
                        for env in envs {
                            let mut tuple = Vec::with_capacity(def.params.len());
                            let mut ok = true;
                            for p in &def.params {
                                match env.get(p) {
                                    Some(v) => tuple.push(v.clone()),
                                    None => {
                                        ok = false;
                                        break;
                                    }
                                }
                            }
                            if ok && seen.get_mut(name).unwrap().insert(format!("{:?}", tuple)) {
                                produced.entry(name.clone()).or_default().push(tuple);
                            }
                        }
                    }
                }
            }
            let mut any = false;
            for name in rule_map.keys() {
                let new = produced.remove(name).unwrap_or_default();
                any |= !new.is_empty();
                tables.get_mut(name).unwrap().extend(new.iter().cloned());
                tables.insert(delta_key(name), new);
            }
            if !any {
                break;
            }
            first = false;
        }
        for name in rule_map.keys() {
            tables.remove(&delta_key(name));
        }
        Ok(tables)
    }

    /// Whether entity `eid` holds `val` for `aid`. NaN equals nothing. New writes store zero as `+0.0`, but older
    /// databases may hold `-0.0`, so a zero lookup checks both signs.
    fn eav_has(&self, rtxn: &ReadTxn<'_>, eid: EntityId, aid: AttrId, val: &Value, buf: &mut Vec<u8>) -> Result<bool, DatalogError> {
        if probe_key(val).is_none() {
            return Ok(false);
        }
        let zeros: &[f64] = match val {
            Value::F64(x) if *x == 0.0 => &[0.0, -0.0],
            _ => &[],
        };
        if zeros.is_empty() {
            encode_eav_key(eid, aid, val, buf);
            return Ok(self.eav.get(rtxn, buf)?.is_some());
        }
        for &z in zeros {
            encode_eav_key(eid, aid, &Value::F64(z), buf);
            if self.eav.get(rtxn, buf)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
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
                    // The unconstrained scan of this attribute is shared by every binding, so it runs at most once.
                    let mut attr_rows: Option<Vec<(EntityId, Value)>> = None;
                    // Index scans are memoized per clause by their bound key: bindings that share a key share a scan.
                    let mut ea_memo: HashMap<EntityId, Vec<Value>> = HashMap::new();
                    let mut ave_memo: HashMap<Vec<u8>, Vec<EntityId>> = HashMap::new();
                    let mut attr_hash: Option<HashMap<Vec<u8>, Vec<EntityId>>> = None;
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
                                if self.eav_has(rtxn, eid, aid, val, &mut buf)? {
                                    next_envs.push(env.clone());
                                }
                            }
                            (Some(eid), None) => {
                                if !ea_memo.contains_key(&eid) {
                                    encode_ea_prefix(eid, aid, &mut buf);
                                    let mut vals = Vec::new();
                                    for item in self.eav.prefix_iter(rtxn, &buf)? {
                                        let (k, _) = item?;
                                        vals.push(decode_value(&k[12..])?);
                                    }
                                    ea_memo.insert(eid, vals);
                                }
                                for val in &ea_memo[&eid] {
                                    let val = val.clone();
                                    let mut e2 = env.clone();
                                    if let ExprTerm::Var(vname) = v_term {
                                        if let Some(existing) = e2.get(vname) {
                                            if existing != &val {
                                                continue;
                                            }
                                        } else {
                                            e2.insert(vname.clone(), val.clone());
                                        }
                                    }
                                    next_envs.push(e2);
                                }
                            }
                            (None, Some(val)) if spec.index && probe_key(val).is_some() => {
                                encode_av_prefix(aid, val, &mut buf);
                                if !ave_memo.contains_key(buf.as_slice()) {
                                    let mut eids = Vec::new();
                                    for item in self.ave.prefix_iter(rtxn, &buf)? {
                                        let (k, _) = item?;
                                        eids.push(decode_ave_entity(&k)?);
                                    }
                                    ave_memo.insert(buf.clone(), eids);
                                }
                                for &eid in &ave_memo[buf.as_slice()] {
                                    let mut e2 = env.clone();
                                    if let ExprTerm::Var(ename) = e_term {
                                        if let Some(existing) = e2.get(ename) {
                                            if existing != &Value::Ref(eid) {
                                                continue;
                                            }
                                        } else {
                                            e2.insert(ename.clone(), Value::Ref(eid));
                                        }
                                    }
                                    next_envs.push(e2);
                                }
                            }
                            (None, _) => {
                                // Selecting entities by value needs the value index. A value that is only bound
                                // here (a literal, an input, or an earlier clause's binding) cannot use it.
                                if spec.noindex && bound_v.is_some() {
                                    return Err(DatalogError::Type(format!(
                                        "{attr_name} is :db/noindex; entities cannot be selected by its value"
                                    )));
                                }
                                if attr_rows.is_none() {
                                    let mut rows = Vec::new();
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
                                        rows.push((eid, decode_value(&k[12..])?));
                                    }
                                    attr_rows = Some(rows);
                                }
                                if let Some(bv) = bound_v {
                                    // Hash join on the bound value: index the attribute by value once per clause,
                                    // then each binding probes the index instead of scanning the attribute.
                                    if attr_hash.is_none() {
                                        let mut map: HashMap<Vec<u8>, Vec<EntityId>> = HashMap::new();
                                        for (eid, val) in attr_rows.as_deref().unwrap_or_default() {
                                            if let Some(key) = probe_key(val) {
                                                map.entry(key).or_default().push(*eid);
                                            }
                                        }
                                        attr_hash = Some(map);
                                    }
                                    let eids: Vec<EntityId> = match probe_key(bv) {
                                        Some(key) => attr_hash.as_ref().and_then(|m| m.get(&key).cloned()).unwrap_or_default(),
                                        None => Vec::new(),
                                    };
                                    for eid in eids {
                                        if let Some(e2) = bind_datom(env, e_term, v_term, eid, bv) {
                                            next_envs.push(e2);
                                        }
                                    }
                                } else {
                                    for (eid, val) in attr_rows.as_deref().unwrap_or_default() {
                                        if let Some(e2) = bind_datom(env, e_term, v_term, *eid, val) {
                                            next_envs.push(e2);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Clause::Fulltext(arg, e_var) => {
                    for env in &envs {
                        let query = match arg {
                            parser::FtsArg::Static(q) => q.clone(),
                            parser::FtsArg::Var(v) => match env.get(v).and_then(|val| val.as_str()) {
                                Some(text) => FtsQuery::Text(text.to_owned()),
                                None => continue,
                            },
                        };
                        // No cap: a Datalog clause must return every match, not just the top results.
                        let hits = self.fts.search_query(rtxn, &query, usize::MAX)?;
                        for &(eid, _) in &hits {
                            let mut e2 = env.clone();
                            if bind_var(&mut e2, e_var, Value::Ref(eid)) {
                                next_envs.push(e2);
                            }
                        }
                    }
                }
                Clause::Nested(query, args, out) => {
                    // Uncorrelated (constant) arguments run the nested query once, not once per environment.
                    let uncorrelated = args.iter().all(|a| matches!(a, ExprTerm::Val(_)));
                    let mut shared: Option<Vec<Vec<Value>>> = None;
                    for env in &envs {
                        let rows = match (&shared, uncorrelated) {
                            (Some(rows), true) => rows.clone(),
                            _ => {
                                let mut inputs = Vec::with_capacity(args.len());
                                for a in args {
                                    match resolve_term(env, a) {
                                        Some(v) => inputs.push(QueryInput::Scalar(v.clone())),
                                        None => return Err(DatalogError::UnboundVar(format!("{a:?}"))),
                                    }
                                }
                                let rows = self.run_nested(rtxn, query, inputs, rule_tables)?;
                                if uncorrelated {
                                    shared = Some(rows.clone());
                                }
                                rows
                            }
                        };
                        for row in rows {
                            let mut e2 = env.clone();
                            let ok = out.iter().zip(row).all(|(var, val)| match var {
                                Some(v) => bind_var(&mut e2, v, val),
                                None => true,
                            });
                            if ok {
                                next_envs.push(e2);
                            }
                        }
                    }
                }
                Clause::IdocMatch(attr, preds, out) => {
                    for env in &envs {
                        let mut resolved = Vec::with_capacity(preds.len());
                        for (path, term) in preds {
                            match resolve_term(env, term) {
                                Some(v) => resolved.push((path.clone(), v.clone())),
                                None => return Err(DatalogError::UnboundVar(format!("{term:?}"))),
                            }
                        }
                        for (eid, name, doc) in self.idoc_match(rtxn, attr.as_deref(), &resolved)? {
                            let mut e2 = env.clone();
                            let bindings = [
                                (out[0].as_ref(), Value::Ref(eid)),
                                (out[1].as_ref(), Value::Keyword(name.clone())),
                                (out[2].as_ref(), doc),
                            ];
                            let ok = bindings.into_iter().all(|(var, val)| match var {
                                Some(v) => bind_var(&mut e2, v, val),
                                None => true,
                            });
                            if ok {
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
                                    CmpOp::Like(esc) | CmpOp::NotLike(esc) => match (a.as_str(), b.as_str()) {
                                        (Some(text), Some(pat)) => {
                                            let tokens = like_compile(pat, *esc).ok_or_else(|| {
                                                DatalogError::Function(format!("invalid like pattern: {pat}"))
                                            })?;
                                            let hit = like_run(text, &tokens);
                                            if matches!(op, CmpOp::NotLike(_)) { !hit } else { hit }
                                        }
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
                Clause::Fn(name, args, out) => {
                    for mut env in envs {
                        let mut vals = Vec::with_capacity(args.len());
                        for a in args {
                            match resolve_term(&env, a) {
                                Some(v) => vals.push(v.clone()),
                                None => return Err(DatalogError::UnboundVar(format!("{a:?}"))),
                            }
                        }
                        let result = self.call_fn(rtxn, name, &vals)?;
                        match (out, result) {
                            (FnOut::None, Some(v)) => {
                                if v != Value::Bool(false) {
                                    next_envs.push(env);
                                }
                            }
                            (FnOut::None, None) => {}
                            (FnOut::Var(x), Some(v)) => {
                                if bind_var(&mut env, x, v) {
                                    next_envs.push(env);
                                }
                            }
                            (FnOut::Var(_), None) => {}
                            (FnOut::Tuple(xs), Some(Value::Tuple(vs))) => {
                                if vs.len() != xs.len() {
                                    return Err(DatalogError::Function(format!(
                                        "{name} returned {} values for {} bindings",
                                        vs.len(),
                                        xs.len()
                                    )));
                                }
                                // Ignored (`_`) positions bind nothing, so they never constrain other values.
                                if xs.iter().zip(vs).all(|(x, v)| match x {
                                    Some(var) => bind_var(&mut env, var, v),
                                    None => true,
                                }) {
                                    next_envs.push(env);
                                }
                            }
                            (FnOut::Tuple(_), None) => {}
                            (FnOut::Tuple(_), Some(_)) => {
                                return Err(DatalogError::Function(format!("{name} must return a tuple")));
                            }
                            (FnOut::Coll(x), Some(Value::Tuple(items))) => {
                                for item in items {
                                    let mut e = env.clone();
                                    if bind_var(&mut e, x, item) {
                                        next_envs.push(e);
                                    }
                                }
                            }
                            (FnOut::Coll(_), None) => {}
                            (FnOut::Coll(_), Some(_)) => {
                                return Err(DatalogError::Function(format!("{name} must return a tuple")));
                            }
                            (FnOut::Rel(xs), Some(Value::Tuple(rows))) => {
                                for row in rows {
                                    let Value::Tuple(vs) = row else {
                                        return Err(DatalogError::Function(format!("{name} rows must be tuples")));
                                    };
                                    if vs.len() != xs.len() {
                                        return Err(DatalogError::Function(format!(
                                            "{name} returned a row of {} values for {} bindings",
                                            vs.len(),
                                            xs.len()
                                        )));
                                    }
                                    let mut e = env.clone();
                                    if xs.iter().zip(vs).all(|(x, v)| match x {
                                        Some(var) => bind_var(&mut e, var, v),
                                        None => true,
                                    }) {
                                        next_envs.push(e);
                                    }
                                }
                            }
                            (FnOut::Rel(_), None) => {}
                            (FnOut::Rel(_), Some(_)) => {
                                return Err(DatalogError::Function(format!("{name} must return a tuple of rows")));
                            }
                        }
                    }
                }
                Clause::Or(branches) => {
                    for branch in branches {
                        let branch_envs = self.eval_clauses(rtxn, branch, envs.clone(), rule_tables)?;
                        next_envs.extend(branch_envs);
                    }
                }
                Clause::OrJoin(join, branches) => {
                    // Only the outer variables and the join variables survive a branch; the rest are branch-local.
                    let outer: HashSet<String> = envs.iter().flat_map(|e| e.keys().cloned()).collect();
                    for branch in branches {
                        for mut be in self.eval_clauses(rtxn, branch, envs.clone(), rule_tables)? {
                            be.retain(|k, _| outer.contains(k) || join.contains(k));
                            next_envs.push(be);
                        }
                    }
                }
                Clause::NotJoin(join, inner) => {
                    for env in envs {
                        let scoped: HashMap<String, Value> =
                            env.iter().filter(|(k, _)| join.contains(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
                        if self.eval_clauses(rtxn, inner, vec![scoped], rule_tables)?.is_empty() {
                            next_envs.push(env);
                        }
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

#[cfg(test)]
thread_local! {
    /// Test-only switch: when set, the next logged transaction's store commit fails.
    static FAIL_NEXT_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Commits a transaction. In tests, a pending injected failure makes the commit fail instead.
fn commit_txn(wtxn: WriteTxn<'_>) -> Result<(), KvError> {
    #[cfg(test)]
    if FAIL_NEXT_COMMIT.with(|c| c.replace(false)) {
        drop(wtxn);
        return Err(KvError::Unsupported("injected commit failure"));
    }
    wtxn.commit()
}

/// Full-text document key for one datom: entity id, attribute id, then the encoded value. Each datom is
/// its own document, so an entity's other values and attributes stay indexed when one is removed.
fn fts_key(eid: EntityId, aid: AttrId, val: &Value) -> Vec<u8> {
    let mut key = eid.to_be_bytes().to_vec();
    key.extend_from_slice(&aid.to_be_bytes());
    encode_value(val, &mut key);
    key
}

/// Hash key for a value under `Value` equality. `-0.0` and `0.0` are equal, so they share a key; `NaN` equals
/// nothing, so it has none.
fn probe_key(v: &Value) -> Option<Vec<u8>> {
    let canonical = match v {
        Value::F64(x) if x.is_nan() => return None,
        Value::F64(x) if *x == 0.0 => Value::F64(0.0),
        other => other.clone(),
    };
    let mut key = Vec::new();
    encode_value(&canonical, &mut key);
    Some(key)
}

/// Extends `env` with one matching datom `(eid, val)` for the pattern `[e_term a v_term]`, or `None` if the
/// datom conflicts with a binding `env` already has.
fn bind_datom(env: &HashMap<String, Value>, e_term: &ExprTerm, v_term: &ExprTerm, eid: EntityId, val: &Value) -> Option<HashMap<String, Value>> {
    let mut e2 = env.clone();
    if let ExprTerm::Var(ename) = e_term {
        match e2.get(ename) {
            Some(existing) if existing != &Value::Ref(eid) => return None,
            Some(_) => {}
            None => {
                e2.insert(ename.clone(), Value::Ref(eid));
            }
        }
    }
    if let ExprTerm::Var(vname) = v_term {
        match e2.get(vname) {
            Some(existing) if existing != val => return None,
            Some(_) => {}
            None => {
                e2.insert(vname.clone(), val.clone());
            }
        }
    }
    Some(e2)
}

/// Binds `var` to `val`, or checks equality if it is already bound. Returns false on conflict.
fn bind_var(env: &mut HashMap<String, Value>, var: &str, val: Value) -> bool {
    match env.get(var) {
        Some(existing) => *existing == val,
        None => {
            env.insert(var.to_owned(), val);
            true
        }
    }
}

/// Expands `:in` inputs into starting environments. Collections and relations multiply out,
/// and a variable repeated across bindings must agree.
fn seed_envs(
    bindings: &[InBinding],
    inputs: &[QueryInput],
) -> Result<Vec<HashMap<String, Value>>, DatalogError> {
    let mismatch = || DatalogError::Parse(ParseError::InvalidQuery(":in input does not match its binding shape".into()));
    if bindings.len() != inputs.len() {
        return Err(DatalogError::Parse(ParseError::InvalidQuery(format!(
            "expected {} :in arguments, got {}",
            bindings.len(),
            inputs.len()
        ))));
    }
    let mut envs = vec![HashMap::new()];
    for (b, inp) in bindings.iter().zip(inputs) {
        let parts: Vec<Vec<(String, Value)>> = match (b, inp) {
            (InBinding::Scalar(v), QueryInput::Scalar(x)) => vec![vec![(v.clone(), x.clone())]],
            (InBinding::Coll(v), QueryInput::Coll(xs)) => {
                xs.iter().map(|x| vec![(v.clone(), x.clone())]).collect()
            }
            (InBinding::Tuple(vs), QueryInput::Tuple(xs)) if vs.len() == xs.len() => {
                vec![vs.iter().cloned().zip(xs.iter().cloned()).collect()]
            }
            (InBinding::Rel(vs), QueryInput::Rel(rows)) => {
                let mut parts = Vec::with_capacity(rows.len());
                for r in rows {
                    if r.len() != vs.len() {
                        return Err(mismatch());
                    }
                    parts.push(vs.iter().cloned().zip(r.iter().cloned()).collect());
                }
                parts
            }
            _ => return Err(mismatch()),
        };
        let mut next = Vec::with_capacity(envs.len() * parts.len());
        for env in &envs {
            'part: for part in &parts {
                let mut e2 = env.clone();
                for (k, v) in part {
                    if !bind_var(&mut e2, k, v.clone()) {
                        continue 'part;
                    }
                }
                next.push(e2);
            }
        }
        envs = next;
    }
    Ok(envs)
}

fn checked_num(r: Option<i64>, op: &str) -> Result<Value, DatalogError> {
    r.map(Value::I64)
        .ok_or_else(|| DatalogError::Function(format!("integer overflow in {op}")))
}

/// Arithmetic over `I64` and `F64` (mixed operands promote to `F64`). Integer `/` stays integral when exact.
fn num_op(op: &str, a: &Value, b: &Value) -> Result<Value, DatalogError> {
    let bad = || DatalogError::Function(format!("{op} expects numbers"));
    match (a, b) {
        (Value::I64(x), Value::I64(y)) => {
            let (x, y) = (*x, *y);
            if matches!(op, "quot" | "rem" | "mod" | "/") && y == 0 {
                return Err(DatalogError::Function("division by zero".into()));
            }
            match op {
                "+" => checked_num(x.checked_add(y), op),
                "-" => checked_num(x.checked_sub(y), op),
                "*" => checked_num(x.checked_mul(y), op),
                "/" if x.checked_rem(y) == Some(0) => checked_num(x.checked_div(y), op),
                "/" => Ok(Value::F64(x as f64 / y as f64)),
                "quot" => checked_num(x.checked_div(y), op),
                "rem" => checked_num(x.checked_rem(y), op),
                "mod" => checked_num(x.checked_rem(y).map(|r| if r != 0 && (r < 0) != (y < 0) { r + y } else { r }), op),
                _ => Err(bad()),
            }
        }
        _ => {
            let (x, y) = match (value_as_f64(a), value_as_f64(b)) {
                (Some(x), Some(y)) => (x, y),
                _ => return Err(bad()),
            };
            Ok(Value::F64(match op {
                "+" => x + y,
                "-" => x - y,
                "*" => x * y,
                "/" => x / y,
                "quot" => (x / y).trunc(),
                "rem" => x % y,
                "mod" => x - y * (x / y).floor(),
                _ => return Err(bad()),
            }))
        }
    }
}

fn check_arity(name: &str, args: &[Value], n: usize) -> Result<(), DatalogError> {
    if args.len() != n {
        return Err(DatalogError::Function(format!("{name} takes {n} arguments, got {}", args.len())));
    }
    Ok(())
}

impl Conn {
    /// Resolves an entity argument: an entity id, or a `[attr value]` lookup ref. `None` if absent.
    fn entity_arg(&self, rtxn: &ReadTxn<'_>, v: &Value) -> Result<Option<EntityId>, DatalogError> {
        match v {
            Value::Ref(e) => Ok(Some(*e)),
            Value::I64(e) if *e >= 0 => Ok(Some(*e as EntityId)),
            Value::Tuple(lr) if lr.len() == 2 => match &lr[0] {
                Value::Keyword(a) => self.lookup_entity_by_av(rtxn, a, &lr[1]),
                _ => Err(DatalogError::Function("lookup ref must start with an attribute keyword".into())),
            },
            _ => Err(DatalogError::Function(format!("not an entity: {v:?}"))),
        }
    }

    fn attr_arg(v: &Value) -> Result<&str, DatalogError> {
        match v {
            Value::Keyword(a) => Ok(a.as_str()),
            _ => Err(DatalogError::Function(format!("expected an attribute keyword, got {v:?}"))),
        }
    }

    /// Evaluates a built-in query function. `None` means nil (the binding or filter fails).
    fn call_fn(&self, rtxn: &ReadTxn<'_>, name: &str, args: &[Value]) -> Result<Option<Value>, DatalogError> {
        Ok(match name {
            "get-else" => {
                check_arity(name, args, 3)?;
                let v = match self.entity_arg(rtxn, &args[0])? {
                    Some(e) => self.get_attr(rtxn, e, Self::attr_arg(&args[1])?)?,
                    None => None,
                };
                Some(v.unwrap_or_else(|| args[2].clone()))
            }
            "get-some" => {
                if args.len() < 2 {
                    return Err(DatalogError::Function("get-some takes an entity and attributes".into()));
                }
                let mut found = None;
                if let Some(e) = self.entity_arg(rtxn, &args[0])? {
                    for a in &args[1..] {
                        let attr = Self::attr_arg(a)?;
                        if let Some(v) = self.get_attr(rtxn, e, attr)? {
                            found = Some(Value::Tuple(vec![Value::Keyword(attr.to_owned()), v]));
                            break;
                        }
                    }
                }
                found
            }
            // Total variant of get-some: `[nil else]` when no attribute is present. Datalevin's nil is
            // represented by the keyword `:nil`, since a `Value` cannot be nil.
            "get-some-else" => {
                if args.len() < 3 {
                    return Err(DatalogError::Function("get-some-else takes an entity, a default, and attributes".into()));
                }
                let mut found = None;
                if let Some(e) = self.entity_arg(rtxn, &args[0])? {
                    for a in &args[2..] {
                        let attr = Self::attr_arg(a)?;
                        if let Some(v) = self.get_attr(rtxn, e, attr)? {
                            found = Some(Value::Tuple(vec![Value::Keyword(attr.to_owned()), v]));
                            break;
                        }
                    }
                }
                Some(found.unwrap_or_else(|| Value::Tuple(vec![Value::Keyword(":nil".into()), args[1].clone()])))
            }
            "missing?" => {
                check_arity(name, args, 2)?;
                let has = match self.entity_arg(rtxn, &args[0])? {
                    Some(e) => self.get_attr(rtxn, e, Self::attr_arg(&args[1])?)?.is_some(),
                    None => false,
                };
                Some(Value::Bool(!has))
            }
            "ground" => {
                check_arity(name, args, 1)?;
                Some(args[0].clone())
            }
            "tuple" => Some(Value::Tuple(args.to_vec())),
            "vec-neighbors" => {
                check_arity(name, args, 2)?;
                let query = match &args[0] {
                    Value::Tuple(xs) => xs
                        .iter()
                        .map(|x| match x {
                            Value::I64(i) => Ok(*i as f32),
                            Value::F64(f) => Ok(*f as f32),
                            _ => Err(DatalogError::Function("vec-neighbors query must be a tuple of numbers".into())),
                        })
                        .collect::<Result<Vec<f32>, _>>()?,
                    _ => return Err(DatalogError::Function("vec-neighbors query must be a tuple of numbers".into())),
                };
                let k = match &args[1] {
                    Value::I64(k) if *k >= 0 => *k as usize,
                    _ => return Err(DatalogError::Function("vec-neighbors k must be a non-negative integer".into())),
                };
                let hits = self.search_vec(rtxn, &query, k)?;
                Some(Value::Tuple(
                    hits.into_iter()
                        .map(|(e, d)| Value::Tuple(vec![Value::Ref(e), Value::F64(d as f64)]))
                        .collect(),
                ))
            }
            "untuple" => {
                check_arity(name, args, 1)?;
                match &args[0] {
                    t @ Value::Tuple(_) => Some(t.clone()),
                    _ => return Err(DatalogError::Function("untuple expects a tuple".into())),
                }
            }
            "in" | "not-in" => {
                check_arity(name, args, 2)?;
                let coll = match &args[1] {
                    Value::Tuple(c) => c,
                    _ => return Err(DatalogError::Function(format!("{name} expects a collection"))),
                };
                let hit = coll.contains(&args[0]);
                Some(Value::Bool(if name == "in" { hit } else { !hit }))
            }
            "inc" | "dec" => {
                check_arity(name, args, 1)?;
                let op = if name == "inc" { "+" } else { "-" };
                Some(num_op(op, &args[0], &Value::I64(1))?)
            }
            "+" | "-" | "*" | "/" | "quot" | "rem" | "mod" => {
                check_arity(name, args, 2)?;
                Some(num_op(name, &args[0], &args[1])?)
            }
            other => match self.query_fns.get(other) {
                Some(f) => return f(args),
                None => return Err(DatalogError::Function(format!("unknown function: {other}"))),
            },
        })
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum LikeTok {
    Lit(char),
    AnyRun,
    AnyOne,
}

/// Compiles a LIKE pattern: `%` any run, `_` one char, and an escape character that makes the next
/// `%`, `_`, or escape literal. A trailing escape adds nothing. Other escapes are invalid (`None`).
fn like_compile(pattern: &str, escape: char) -> Option<Vec<LikeTok>> {
    let mut out = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        out.push(if c == escape {
            match chars.next() {
                None => continue,
                Some(n) if n == '%' || n == '_' || n == escape => LikeTok::Lit(n),
                Some(_) => return None,
            }
        } else {
            match c {
                '%' => LikeTok::AnyRun,
                '_' => LikeTok::AnyOne,
                other => LikeTok::Lit(other),
            }
        });
    }
    Some(out)
}

fn like_run(text: &str, p: &[LikeTok]) -> bool {
    let t: Vec<char> = text.chars().collect();
    let m = p.len();
    let mut dp = vec![false; m + 1];
    dp[0] = true;
    for j in 1..=m {
        if p[j - 1] == LikeTok::AnyRun {
            dp[j] = dp[j - 1];
        }
    }
    for &ch in &t {
        let mut next = vec![false; m + 1];
        for j in 1..=m {
            next[j] = match p[j - 1] {
                LikeTok::AnyRun => next[j - 1] || dp[j],
                LikeTok::AnyOne => dp[j - 1],
                LikeTok::Lit(c) => c == ch && dp[j - 1],
            };
        }
        dp = next;
    }
    dp[m]
}

/// SQL-style `LIKE` with the default `!` escape (Datalevin's default).
pub fn like_match(text: &str, pattern: &str) -> bool {
    like_compile(pattern, parser::DEFAULT_ESCAPE).is_some_and(|t| like_run(text, &t))
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
        let typed: Vec<QueryInput> = inputs.iter().cloned().map(QueryInput::Scalar).collect();
        self.execute_inputs(rtxn, &typed)
    }

    pub fn execute_inputs(&self, rtxn: &ReadTxn<'_>, inputs: &[QueryInput]) -> Result<QueryResult, DatalogError> {
        let seed = seed_envs(&self.parsed.in_bindings, inputs)?;
        // shortcut: rewrites per execution; cache per adornment only if rewriting shows up in profiles.
        let mut clauses = self.parsed.clauses.clone();
        if seed.len() == 1 {
            // Substitute the single input's values into top-level rule calls so magic-set rewriting can fire.
            for c in clauses.iter_mut() {
                if let Clause::RuleCall(_, args) = c {
                    for a in args.iter_mut() {
                        if let ExprTerm::Var(v) = a {
                            if let Some(val) = seed[0].get(v) {
                                *a = ExprTerm::Val(val.clone());
                            }
                        }
                    }
                }
            }
        }
        let (eval_rules, clauses) = self.conn.demand_rules(&self.rule_map, clauses);
        let rule_tables = if eval_rules.is_empty() {
            HashMap::new()
        } else {
            self.conn.eval_all_rules(rtxn, &eval_rules)?
        };
        let envs = self
            .conn
            .eval_clauses(rtxn, &clauses, seed, &rule_tables)?;
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

impl Conn {
    /// Largest entity id allocated so far (0 when none).
    pub fn max_eid(&self, rtxn: &ReadTxn<'_>) -> Result<EntityId, DatalogError> {
        Ok(match self.meta.get(rtxn, NEXT_EID_KEY)? {
            Some(b) if b.len() == 8 => u64::from_be_bytes(b.as_ref().try_into().unwrap()).saturating_sub(1),
            _ => 0,
        })
    }

    /// Number of distinct values stored for `attr`.
    pub fn cardinality(&self, rtxn: &ReadTxn<'_>, attr: &str) -> Result<u64, DatalogError> {
        let aid = self.attr_id(attr)?;
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        for item in self.eav.iter(rtxn)? {
            let (k, _) = item?;
            if k.len() >= 12 && u32::from_be_bytes(k[8..12].try_into().unwrap()) == aid {
                seen.insert(k[12..].to_vec());
            }
        }
        Ok(seen.len() as u64)
    }

    /// Datoms for an indexed `attr` with value `<= start`, in descending value order.
    pub fn rseek_datoms(
        &self,
        rtxn: &ReadTxn<'_>,
        attr: &str,
        start: &Value,
    ) -> Result<Vec<(EntityId, String, Value)>, DatalogError> {
        let aid = self.attr_id(attr)?;
        let mut hi = Vec::with_capacity(32);
        encode_av_prefix(aid, start, &mut hi);
        hi.extend_from_slice(&[0xff; 8]);
        let lo = aid.to_be_bytes();
        let mut out = Vec::new();
        for item in self.ave.range(
            rtxn,
            &(std::ops::Bound::Included(&lo[..]), std::ops::Bound::Included(hi.as_slice())),
        )? {
            let (k, _) = item?;
            let eid = decode_ave_entity(&k)?;
            let (raw_val, _) = dtlv_codec::decode_escaped_bytes(&k[4..k.len() - 8])?;
            out.push((eid, attr.to_owned(), decode_value(&raw_val)?));
        }
        out.reverse();
        Ok(out)
    }

    /// Datoms matching the given components; `None` is a wildcard. Uses EAV for a bound entity,
    /// AVE for an indexed `(attr, value)`, and a full EAV scan otherwise.
    pub fn search_datoms(
        &self,
        rtxn: &ReadTxn<'_>,
        e: Option<EntityId>,
        a: Option<&str>,
        v: Option<&Value>,
    ) -> Result<Vec<(EntityId, String, Value)>, DatalogError> {
        let aid = match a {
            Some(name) => Some(self.attr_id(name)?),
            None => None,
        };
        let mut out = Vec::new();
        if let Some(eid) = e {
            let mut pref = eid.to_be_bytes().to_vec();
            if let Some(aid) = aid {
                pref.extend_from_slice(&aid.to_be_bytes());
            }
            for item in self.eav.prefix_iter(rtxn, &pref)? {
                let (k, _) = item?;
                let kaid = u32::from_be_bytes(k[8..12].try_into().unwrap());
                let val = decode_value(&k[12..])?;
                if v.is_none_or(|x| *x == val) {
                    if let Some(name) = self.aid_names.get(&kaid) {
                        out.push((eid, name.clone(), val));
                    }
                }
            }
            return Ok(out);
        }
        let indexed_av = match (aid, v) {
            (Some(aid), Some(val)) if self.attrs.get(&aid).is_some_and(|s| s.index) => Some((aid, val)),
            _ => None,
        };
        if let Some((aid, val)) = indexed_av {
            let mut pref = Vec::with_capacity(32);
            encode_av_prefix(aid, val, &mut pref);
            for item in self.ave.prefix_iter(rtxn, &pref)? {
                let (k, _) = item?;
                out.push((decode_ave_entity(&k)?, a.unwrap_or_default().to_owned(), val.clone()));
            }
            return Ok(out);
        }
        for (eid, name, val) in self.datoms(rtxn, IndexKind::Eavt)? {
            if a.is_none_or(|x| x == name) && v.is_none_or(|x| *x == val) {
                out.push((eid, name, val));
            }
        }
        Ok(out)
    }

    /// Datoms of fulltext-indexed attributes for the entities that match `query`, best match first.
    pub fn fulltext_datoms(
        &self,
        rtxn: &ReadTxn<'_>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(EntityId, String, Value)>, DatalogError> {
        let mut out = Vec::new();
        for (doc, _) in self.fts.search_docs(rtxn, query, limit)? {
            let eid = dtlv_search::fulltext::doc_entity(&doc);
            let aid = u32::from_be_bytes(doc[8..12].try_into().unwrap());
            let val = decode_value(&doc[12..])?;
            if let Some(name) = self.aid_names.get(&aid) {
                out.push((eid, name.clone(), val));
            }
        }
        Ok(out)
    }

    /// Every `(eid, value, raw EAV key)` stored under `aid`, read inside a write transaction.
    fn scan_attr_rw(
        &self,
        wtxn: &WriteTxn<'_>,
        aid: AttrId,
    ) -> Result<Vec<(EntityId, Value, Vec<u8>)>, DatalogError> {
        let mut out = Vec::new();
        for (k, _) in self.eav.iter_rw(wtxn)? {
            if k.len() >= 12 && u32::from_be_bytes(k[8..12].try_into().unwrap()) == aid {
                let eid = u64::from_be_bytes(k[0..8].try_into().unwrap());
                out.push((eid, decode_value(&k[12..])?, k));
            }
        }
        Ok(out)
    }

    /// Adds attributes, deletes attributes (with their datoms), and renames attributes.
    ///
    /// Adding a name that already exists is a no-op when the spec is identical. For an existing attribute
    /// these changes are allowed, in place:
    /// - `cardinality` one to many (always).
    /// - `cardinality` many to one, only if no entity holds more than one value.
    /// - `unique` none to value or identity, only if no two entities share a value. Backfills AVE entries.
    /// - `unique` value or identity to another kind, or to none (always). AVE entries are kept, not deleted.
    /// - gaining `index` or `fulltext`, which backfills existing datoms. Dropping `index` or `fulltext`
    ///   is rejected, except for an index that only came from `unique`.
    ///
    /// Changes to `is_component` or `tuple_sources` are rejected. Renames keep the internal attribute id,
    /// so no datoms are rewritten. Validation runs before any write, and in-memory state changes only
    /// after the commit succeeds. Attribute specs are not persisted; the caller passes them to `open`.
    pub fn update_schema(
        &mut self,
        add: Vec<AttrSpec>,
        del: &[&str],
        rename: &[(&str, &str)],
    ) -> Result<(), DatalogError> {
        for name in del {
            let aid = self.attr_id(name)?;
            if self.tuple_specs.iter().any(|(_, _, srcs)| srcs.contains(&aid)) {
                return Err(DatalogError::Schema(format!("{name} is a source of a tuple attribute")));
            }
        }
        for (old, new) in rename {
            self.attr_id(old)?;
            let clashes = self.attr_ids.contains_key(*new)
                || add.iter().any(|s| s.name == *new)
                || rename.iter().filter(|(_, n)| n == new).count() > 1;
            if clashes || del.contains(old) {
                return Err(DatalogError::Schema(format!("cannot rename {old} to {new}")));
            }
        }
        // Each add: (stored spec, existing aid if the attribute already exists and its schema changes)
        let mut plan: Vec<(AttrSpec, Option<AttrId>)> = Vec::new();
        // Existing attributes whose datoms must be scanned first: (aid, name, check one value per entity, check unique)
        let mut checks: Vec<(AttrId, String, bool, bool)> = Vec::new();
        for spec in add {
            if spec.tuple_sources.is_some() {
                return Err(DatalogError::Schema(format!(
                    "tuple attribute {} must be declared when the connection is opened",
                    spec.name
                )));
            }
            match self.attr_ids.get(&spec.name).copied() {
                None => plan.push((spec, None)),
                Some(aid) => {
                    let old = self.attrs.get(&aid).cloned().unwrap_or_else(|| AttrSpec::new(spec.name.clone()));
                    // Unique attributes always keep their AVE entries, so the index flag is derived from both sides.
                    let ave_old = old.index || old.unique.is_some();
                    let ave_new = spec.index || spec.unique.is_some();
                    if old.is_component != spec.is_component
                        || old.tuple_sources != spec.tuple_sources
                        || (ave_old && !ave_new && old.unique.is_none())
                        || (old.fulltext && !spec.fulltext)
                    {
                        return Err(DatalogError::Schema(format!(
                            "cannot change the properties of existing attribute {}",
                            spec.name
                        )));
                    }
                    let single = old.cardinality == Cardinality::Many && spec.cardinality == Cardinality::One;
                    let unique = old.unique.is_none() && spec.unique.is_some();
                    if single || unique {
                        checks.push((aid, spec.name.clone(), single, unique));
                    }
                    let mut stored = spec;
                    // AVE stays maintained once it exists: dropping unique leaves its entries in place and keeps writing them.
                    stored.index = ave_old || ave_new;
                    if old.cardinality != stored.cardinality
                        || old.unique != stored.unique
                        || old.index != stored.index
                        || old.fulltext != stored.fulltext
                    {
                        plan.push((stored, Some(aid)));
                    }
                }
            }
        }

        // Validate before any write: a failed check leaves the store and in-memory schema unchanged.
        if !checks.is_empty() {
            let rtxn = self.read_txn()?;
            let mut buf = Vec::new();
            for (aid, name, single, unique) in &checks {
                let mut eids = HashSet::new();
                let mut vals = HashSet::new();
                for item in self.eav.iter(&rtxn)? {
                    let (k, _) = item?;
                    if k.len() < 12 || u32::from_be_bytes(k[8..12].try_into().unwrap()) != *aid {
                        continue;
                    }
                    let eid = u64::from_be_bytes(k[0..8].try_into().unwrap());
                    if *single && !eids.insert(eid) {
                        return Err(DatalogError::Schema(format!(
                            "cannot change {name} to cardinality one: entity {eid} has more than one value"
                        )));
                    }
                    if *unique {
                        encode_av_prefix(*aid, &decode_value(&k[12..])?, &mut buf);
                        if !vals.insert(buf.clone()) {
                            return Err(DatalogError::Schema(format!(
                                "cannot make {name} unique: duplicate value"
                            )));
                        }
                    }
                }
            }
        }

        let mut wtxn = self.store.write_txn()?;
        let mut next_aid = match self.meta.get_rw(&wtxn, NEXT_AID_KEY)? {
            Some(b) if b.len() == 4 => u32::from_be_bytes(b.as_ref().try_into().unwrap()),
            _ => 1,
        };
        let mut ave_buf = Vec::new();

        // Deletes: drop every datom of the attribute, then its schema entry.
        for name in del {
            let aid = self.attr_ids[*name];
            let spec = self.attrs.get(&aid).cloned();
            for (eid, val, key) in self.scan_attr_rw(&wtxn, aid)? {
                self.eav.delete(&mut wtxn, &key)?;
                if spec.as_ref().is_some_and(|s| s.index) {
                    encode_ave_key(aid, &val, eid, &mut ave_buf);
                    self.ave.delete(&mut wtxn, &ave_buf)?;
                }
                if let Value::Ref(target) = val {
                    encode_vae_key(target, aid, eid, &mut ave_buf);
                    self.vae.delete(&mut wtxn, &ave_buf)?;
                }
                if spec.as_ref().is_some_and(|s| s.fulltext) {
                    self.fts.remove_entry(&mut wtxn, &fts_key(eid, aid, &val))?;
                }
            }
            let mut key = b"attr:".to_vec();
            key.extend_from_slice(name.as_bytes());
            self.meta.delete(&mut wtxn, &key)?;
        }

        // Renames: move the name mapping; the id and datoms stay put.
        for (old, new) in rename {
            let aid = self.attr_ids[*old];
            let mut old_key = b"attr:".to_vec();
            old_key.extend_from_slice(old.as_bytes());
            self.meta.delete(&mut wtxn, &old_key)?;
            let mut new_key = b"attr:".to_vec();
            new_key.extend_from_slice(new.as_bytes());
            self.meta.put(&mut wtxn, &new_key, &aid.to_be_bytes())?;
        }

        // Adds: new attributes get fresh ids; upgrades backfill their index or fulltext entries.
        let mut new_entries: Vec<(AttrId, AttrSpec)> = Vec::new();
        for (spec, existing) in &plan {
            let aid = match existing {
                Some(aid) => *aid,
                None => {
                    let aid = next_aid;
                    next_aid += 1;
                    let mut key = b"attr:".to_vec();
                    key.extend_from_slice(spec.name.as_bytes());
                    self.meta.put(&mut wtxn, &key, &aid.to_be_bytes())?;
                    aid
                }
            };
            if let Some(old) = self.attrs.get(&aid).filter(|_| existing.is_some()) {
                let backfill_ave = spec.index && !(old.index || old.unique.is_some());
                let backfill_ft = spec.fulltext && !old.fulltext;
                if backfill_ave || backfill_ft {
                    for (eid, val, _) in self.scan_attr_rw(&wtxn, aid)? {
                        if backfill_ave {
                            encode_ave_key(aid, &val, eid, &mut ave_buf);
                            self.ave.put(&mut wtxn, &ave_buf, &[])?;
                        }
                        if backfill_ft {
                            if let Some(text) = val.as_str() {
                                self.fts.index_entry(&mut wtxn, &fts_key(eid, aid, &val), text)?;
                            }
                        }
                    }
                }
            }
            new_entries.push((aid, spec.clone()));
        }
        self.meta.put(&mut wtxn, NEXT_AID_KEY, &next_aid.to_be_bytes())?;
        wtxn.commit()?;

        // Commit succeeded: mirror the change in memory.
        for name in del {
            let aid = self.attr_ids.remove(*name).expect("validated above");
            self.aid_names.remove(&aid);
            self.attrs.remove(&aid);
            self.tuple_specs.retain(|(_, t, _)| *t != aid);
            self.tuple_targets.remove(&aid);
        }
        for (old, new) in rename {
            let aid = self.attr_ids.remove(*old).expect("validated above");
            self.attr_ids.insert((*new).to_owned(), aid);
            self.aid_names.insert(aid, (*new).to_owned());
            if let Some(spec) = self.attrs.get_mut(&aid) {
                spec.name = (*new).to_owned();
            }
            for entry in &mut self.tuple_specs {
                if entry.1 == aid {
                    entry.0 = (*new).to_owned();
                }
            }
        }
        for (aid, spec) in new_entries {
            self.attr_ids.insert(spec.name.clone(), aid);
            self.aid_names.insert(aid, spec.name.clone());
            self.attrs.insert(aid, spec);
        }
        Ok(())
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
                            if vals.iter().all(|v| matches!(v, Value::I64(_))) {
                                let mut s: i64 = 0;
                                for v in &vals {
                                    if let Some(x) = v.as_i64() {
                                        s = s.checked_add(x).ok_or(DatalogError::SumOverflow)?;
                                    }
                                }
                                Value::I64(s)
                            } else {
                                let s: f64 = vals.iter().filter_map(|v| value_as_f64(v)).sum();
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

#[cfg(test)]
mod wal_abort_tests {
    use super::*;
    use tempfile::tempdir;

    fn schema() -> Vec<AttrSpec> {
        vec![AttrSpec::new(":w/name").unique_identity()]
    }

    fn name(n: &str) -> TxItem {
        TxItem::Assert(None, vec![(":w/name".into(), Value::Str(n.into()))])
    }

    #[test]
    fn a_failed_store_commit_after_logging_is_marked_aborted_and_never_replayed() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db");
        let conn = Conn::open_wal(&db, dir.path(), schema(), Durability::Strict).unwrap();
        conn.transact(&[name("kept")]).unwrap();

        // The next store commit fails after its record is appended.
        FAIL_NEXT_COMMIT.with(|c| c.set(true));
        let err = conn.transact(&[name("lost")]);
        assert!(matches!(err, Err(DatalogError::Kv(_))), "got {err:?}");

        // The failed transaction is not visible, and the log now holds an abort marker for it.
        let rtxn = conn.read_txn().unwrap();
        assert_eq!(conn.lookup_entity_by_av(&rtxn, ":w/name", &Value::Str("lost".into())).unwrap(), None);
        drop(rtxn);
        let records = read_wal_records(&dir.path().join("wal.log")).unwrap();
        assert_eq!(records.len(), 1, "only the committed transaction is replayable");

        // The connection keeps working after the failure.
        conn.transact(&[name("after")]).unwrap();
        drop(conn);

        // After a restart, replay applies the committed records and skips the aborted one.
        let conn = Conn::open_wal(&db, dir.path(), schema(), Durability::Strict).unwrap();
        let rtxn = conn.read_txn().unwrap();
        let has = |n: &str| conn.lookup_entity_by_av(&rtxn, ":w/name", &Value::Str(n.into())).unwrap().is_some();
        assert!(has("kept"));
        assert!(has("after"));
        assert!(!has("lost"), "aborted transaction was replayed after restart");
    }
}
