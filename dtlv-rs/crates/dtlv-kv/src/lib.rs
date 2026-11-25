use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard};
use heed::types::Bytes;
use heed::{Database, Env, EnvOpenOptions, RoTxn, RwTxn};
use redb::{ReadableTable, ReadableTableMetadata, TableDefinition, TableHandle};
use rusqlite::{params, Connection};
use thiserror::Error;

pub mod durable;
pub mod list;
pub use list::ListDb;

#[derive(Debug, Error)]
pub enum KvError {
    #[error("lmdb error: {0}")]
    Heed(#[from] heed::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("redb error: {0}")]
    Redb(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid table name: {0}")]
    InvalidTableName(String),
    #[error("malformed list item key")]
    InvalidListItem,
    #[error("operation not supported by this backend: {0}")]
    Unsupported(&'static str),
    #[error("{0}")]
    Config(String),
}

impl From<redb::Error> for KvError {
    fn from(e: redb::Error) -> Self {
        KvError::Redb(e.to_string())
    }
}
impl From<redb::DatabaseError> for KvError {
    fn from(e: redb::DatabaseError) -> Self {
        KvError::Redb(e.to_string())
    }
}
impl From<redb::TransactionError> for KvError {
    fn from(e: redb::TransactionError) -> Self {
        KvError::Redb(e.to_string())
    }
}
impl From<redb::TableError> for KvError {
    fn from(e: redb::TableError) -> Self {
        KvError::Redb(e.to_string())
    }
}
impl From<redb::StorageError> for KvError {
    fn from(e: redb::StorageError) -> Self {
        KvError::Redb(e.to_string())
    }
}
impl From<redb::CommitError> for KvError {
    fn from(e: redb::CommitError) -> Self {
        KvError::Redb(e.to_string())
    }
}

pub enum KvOp<'a> {
    Put(&'a [u8], &'a [u8]),
    Del(&'a [u8]),
}

#[derive(Clone)]
pub enum RawDb {
    Lmdb(Database<Bytes, Bytes>),
    Sqlite { table: String },
    Redb { name: String },
    Mem { name: String },
}

pub enum ReadTxn<'a> {
    Lmdb(RoTxn<'a>),
    Sqlite(MutexGuard<'a, Connection>),
    Redb(redb::ReadTransaction),
    Mem(RwLockReadGuard<'a, HashMap<String, BTreeMap<Vec<u8>, Vec<u8>>>>),
}

pub struct SqliteWriteTxn<'a> {
    conn: MutexGuard<'a, Connection>,
    committed: bool,
}

impl<'a> Drop for SqliteWriteTxn<'a> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

pub struct MemWriteTxn<'a> {
    root: &'a RwLock<HashMap<String, BTreeMap<Vec<u8>, Vec<u8>>>>,
    // Held for the txn's lifetime to serialize writers; readers never take it.
    _writer: MutexGuard<'a, ()>,
    delta: HashMap<String, BTreeMap<Vec<u8>, Option<Vec<u8>>>>,
}

pub enum WriteTxn<'a> {
    Lmdb(RwTxn<'a>),
    Sqlite(SqliteWriteTxn<'a>),
    Redb(redb::WriteTransaction),
    Mem(MemWriteTxn<'a>),
}

impl<'a> WriteTxn<'a> {
    pub fn commit(self) -> Result<(), KvError> {
        match self {
            WriteTxn::Lmdb(t) => {
                t.commit()?;
                Ok(())
            }
            WriteTxn::Sqlite(mut t) => {
                t.conn.execute_batch("COMMIT")?;
                t.committed = true;
                Ok(())
            }
            WriteTxn::Redb(t) => {
                t.commit()?;
                Ok(())
            }
            WriteTxn::Mem(t) => {
                let mut guard = t.root.write().unwrap();
                for (table, ops) in t.delta {
                    let map = guard.entry(table).or_default();
                    for (k, v_opt) in ops {
                        match v_opt {
                            Some(v) => {
                                map.insert(k, v);
                            }
                            None => {
                                map.remove(&k);
                            }
                        }
                    }
                }
                Ok(())
            }
        }
    }
}

pub enum KvIter<'txn> {
    Lmdb(heed::RoIter<'txn, Bytes, Bytes>),
    LmdbPrefix(heed::RoPrefix<'txn, Bytes, Bytes>),
    LmdbRange(heed::RoRange<'txn, Bytes, Bytes>),
    Owned(std::vec::IntoIter<(Vec<u8>, Vec<u8>)>),
}

impl<'txn> Iterator for KvIter<'txn> {
    type Item = Result<(Cow<'txn, [u8]>, Cow<'txn, [u8]>), KvError>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            KvIter::Lmdb(it) => it
                .next()
                .map(|r| r.map(|(k, v)| (Cow::Borrowed(k), Cow::Borrowed(v))).map_err(KvError::from)),
            KvIter::LmdbPrefix(it) => it
                .next()
                .map(|r| r.map(|(k, v)| (Cow::Borrowed(k), Cow::Borrowed(v))).map_err(KvError::from)),
            KvIter::LmdbRange(it) => it
                .next()
                .map(|r| r.map(|(k, v)| (Cow::Borrowed(k), Cow::Borrowed(v))).map_err(KvError::from)),
            KvIter::Owned(it) => it
                .next()
                .map(|(k, v)| Ok((Cow::Owned(k), Cow::Owned(v)))),
        }
    }
}

fn validate_table_name(name: &str) -> Result<String, KvError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(KvError::InvalidTableName(name.to_owned()));
    }
    Ok(format!("kv_{}", name))
}

fn next_prefix(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    for i in (0..out.len()).rev() {
        if out[i] < 0xff {
            out[i] += 1;
            out.truncate(i + 1);
            return Some(out);
        }
    }
    None
}

fn prefix_bounds(prefix: &[u8]) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    let start = Bound::Included(prefix.to_vec());
    let end = match next_prefix(prefix) {
        Some(u) => Bound::Excluded(u),
        None => Bound::Unbounded,
    };
    (start, end)
}

impl RawDb {
    #[inline]
    pub fn get<'txn>(
        &self,
        txn: &'txn ReadTxn<'_>,
        key: &[u8],
    ) -> Result<Option<Cow<'txn, [u8]>>, KvError> {
        match (self, txn) {
            (RawDb::Lmdb(db), ReadTxn::Lmdb(rtxn)) => Ok(db.get(rtxn, key)?.map(Cow::Borrowed)),
            (RawDb::Sqlite { table }, ReadTxn::Sqlite(conn)) => {
                let sql = format!("SELECT v FROM \"{}\" WHERE k = ?1", table);
                let mut stmt = conn.prepare_cached(&sql)?;
                let mut rows = stmt.query(params![key])?;
                if let Some(row) = rows.next()? {
                    Ok(Some(Cow::Owned(row.get::<_, Vec<u8>>(0)?)))
                } else {
                    Ok(None)
                }
            }
            (RawDb::Redb { name }, ReadTxn::Redb(rtxn)) => {
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(name);
                let table = rtxn.open_table(def)?;
                Ok(table.get(key)?.map(|g| Cow::Owned(g.value().to_vec())))
            }
            (RawDb::Mem { name }, ReadTxn::Mem(guard)) => {
                Ok(guard.get(name).and_then(|m| m.get(key)).map(|v| Cow::Borrowed(v.as_slice())))
            }
            _ => unreachable!("mismatched RawDb and ReadTxn backend"),
        }
    }

    #[inline]
    pub fn get_rw<'txn>(
        &self,
        txn: &'txn WriteTxn<'_>,
        key: &[u8],
    ) -> Result<Option<Cow<'txn, [u8]>>, KvError> {
        match (self, txn) {
            (RawDb::Lmdb(db), WriteTxn::Lmdb(wtxn)) => Ok(db.get(wtxn, key)?.map(Cow::Borrowed)),
            (RawDb::Sqlite { table }, WriteTxn::Sqlite(wtxn)) => {
                let sql = format!("SELECT v FROM \"{}\" WHERE k = ?1", table);
                let mut stmt = wtxn.conn.prepare_cached(&sql)?;
                let mut rows = stmt.query(params![key])?;
                if let Some(row) = rows.next()? {
                    Ok(Some(Cow::Owned(row.get::<_, Vec<u8>>(0)?)))
                } else {
                    Ok(None)
                }
            }
            (RawDb::Redb { name }, WriteTxn::Redb(wtxn)) => {
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(name);
                let table = wtxn.open_table(def)?;
                Ok(table.get(key)?.map(|g| Cow::Owned(g.value().to_vec())))
            }
            (RawDb::Mem { name }, WriteTxn::Mem(wtxn)) => {
                if let Some(entry) = wtxn.delta.get(name).and_then(|m| m.get(key)) {
                    return Ok(entry.as_ref().map(|v| Cow::Borrowed(v.as_slice())));
                }
                let guard = wtxn.root.read().unwrap();
                Ok(guard
                    .get(name)
                    .and_then(|m| m.get(key))
                    .map(|v| Cow::Owned(v.clone())))
            }
            _ => unreachable!("mismatched RawDb and WriteTxn backend"),
        }
    }

    #[inline]
    pub fn put(&self, txn: &mut WriteTxn<'_>, key: &[u8], val: &[u8]) -> Result<(), KvError> {
        match (self, txn) {
            (RawDb::Lmdb(db), WriteTxn::Lmdb(wtxn)) => {
                db.put(wtxn, key, val)?;
                Ok(())
            }
            (RawDb::Sqlite { table }, WriteTxn::Sqlite(wtxn)) => {
                let sql = format!("INSERT OR REPLACE INTO \"{}\" (k, v) VALUES (?1, ?2)", table);
                let mut stmt = wtxn.conn.prepare_cached(&sql)?;
                stmt.execute(params![key, val])?;
                Ok(())
            }
            (RawDb::Redb { name }, WriteTxn::Redb(wtxn)) => {
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(name);
                let mut table = wtxn.open_table(def)?;
                table.insert(key, val)?;
                Ok(())
            }
            (RawDb::Mem { name }, WriteTxn::Mem(wtxn)) => {
                wtxn.delta
                    .entry(name.clone())
                    .or_default()
                    .insert(key.to_vec(), Some(val.to_vec()));
                Ok(())
            }
            _ => unreachable!("mismatched RawDb and WriteTxn backend"),
        }
    }

    #[inline]
    pub fn delete(&self, txn: &mut WriteTxn<'_>, key: &[u8]) -> Result<bool, KvError> {
        match (self, txn) {
            (RawDb::Lmdb(db), WriteTxn::Lmdb(wtxn)) => Ok(db.delete(wtxn, key)?),
            (RawDb::Sqlite { table }, WriteTxn::Sqlite(wtxn)) => {
                let sql = format!("DELETE FROM \"{}\" WHERE k = ?1", table);
                let mut stmt = wtxn.conn.prepare_cached(&sql)?;
                let changed = stmt.execute(params![key])?;
                Ok(changed > 0)
            }
            (RawDb::Redb { name }, WriteTxn::Redb(wtxn)) => {
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(name);
                let mut table = wtxn.open_table(def)?;
                let removed = table.remove(key)?.is_some();
                Ok(removed)
            }
            (RawDb::Mem { name }, WriteTxn::Mem(wtxn)) => {
                let existed = if let Some(entry) = wtxn.delta.get(name).and_then(|m| m.get(key)) {
                    entry.is_some()
                } else {
                    wtxn.root
                        .read()
                        .unwrap()
                        .get(name)
                        .is_some_and(|m| m.contains_key(key))
                };
                wtxn.delta
                    .entry(name.clone())
                    .or_default()
                    .insert(key.to_vec(), None);
                Ok(existed)
            }
            _ => unreachable!("mismatched RawDb and WriteTxn backend"),
        }
    }

    pub fn prefix_iter<'txn>(
        &self,
        txn: &'txn ReadTxn<'_>,
        prefix: &[u8],
    ) -> Result<KvIter<'txn>, KvError> {
        match (self, txn) {
            (RawDb::Lmdb(db), ReadTxn::Lmdb(rtxn)) => Ok(KvIter::LmdbPrefix(db.prefix_iter(rtxn, prefix)?)),
            (RawDb::Sqlite { table }, ReadTxn::Sqlite(conn)) => {
                let rows = sqlite_prefix_scan(conn, table, prefix)?;
                Ok(KvIter::Owned(rows.into_iter()))
            }
            (RawDb::Redb { name }, ReadTxn::Redb(rtxn)) => {
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(name);
                let table = rtxn.open_table(def)?;
                let (s, e) = prefix_bounds(prefix);
                let s_ref = match &s {
                    Bound::Included(v) => Bound::Included(v.as_slice()),
                    _ => Bound::Unbounded,
                };
                let e_ref = match &e {
                    Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
                    _ => Bound::Unbounded,
                };
                let mut rows = Vec::new();
                for item in table.range::<&[u8]>((s_ref, e_ref))? {
                    let (k, v) = item?;
                    rows.push((k.value().to_vec(), v.value().to_vec()));
                }
                Ok(KvIter::Owned(rows.into_iter()))
            }
            (RawDb::Mem { name }, ReadTxn::Mem(guard)) => {
                let rows = mem_prefix_scan(guard.get(name), prefix);
                Ok(KvIter::Owned(rows.into_iter()))
            }
            _ => unreachable!("mismatched RawDb and ReadTxn backend"),
        }
    }

    pub fn prefix_iter_rw<'txn>(
        &self,
        txn: &'txn WriteTxn<'_>,
        prefix: &[u8],
    ) -> Result<KvIter<'txn>, KvError> {
        match (self, txn) {
            (RawDb::Lmdb(db), WriteTxn::Lmdb(wtxn)) => Ok(KvIter::LmdbPrefix(db.prefix_iter(wtxn, prefix)?)),
            (RawDb::Sqlite { table }, WriteTxn::Sqlite(wtxn)) => {
                let rows = sqlite_prefix_scan(&wtxn.conn, table, prefix)?;
                Ok(KvIter::Owned(rows.into_iter()))
            }
            (RawDb::Redb { name }, WriteTxn::Redb(wtxn)) => {
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(name);
                let table = wtxn.open_table(def)?;
                let (s, e) = prefix_bounds(prefix);
                let s_ref = match &s {
                    Bound::Included(v) => Bound::Included(v.as_slice()),
                    _ => Bound::Unbounded,
                };
                let e_ref = match &e {
                    Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
                    _ => Bound::Unbounded,
                };
                let mut rows = Vec::new();
                for item in table.range::<&[u8]>((s_ref, e_ref))? {
                    let (k, v) = item?;
                    rows.push((k.value().to_vec(), v.value().to_vec()));
                }
                Ok(KvIter::Owned(rows.into_iter()))
            }
            (RawDb::Mem { name }, WriteTxn::Mem(wtxn)) => {
                let guard = wtxn.root.read().unwrap();
                let mut merged: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
                for (k, v) in mem_prefix_scan(guard.get(name), prefix) {
                    merged.insert(k, v);
                }
                if let Some(dmap) = wtxn.delta.get(name) {
                    let (s, e) = prefix_bounds(prefix);
                    for (k, v_opt) in dmap.range::<Vec<u8>, _>((s, e)) {
                        match v_opt {
                            Some(v) => {
                                merged.insert(k.clone(), v.clone());
                            }
                            None => {
                                merged.remove(k);
                            }
                        }
                    }
                }
                let rows: Vec<_> = merged.into_iter().collect();
                Ok(KvIter::Owned(rows.into_iter()))
            }
            _ => unreachable!("mismatched RawDb and WriteTxn backend"),
        }
    }

    /// Every entry inside a write transaction, in key order. LMDB rejects an empty prefix scan, so the
    /// whole-DBI read goes through the native iterator there.
    pub fn iter_rw(&self, txn: &WriteTxn<'_>) -> Result<Vec<(Vec<u8>, Vec<u8>)>, KvError> {
        let mut out = Vec::new();
        match (self, txn) {
            (RawDb::Lmdb(db), WriteTxn::Lmdb(w)) => {
                for item in db.iter(w)? {
                    let (k, v) = item?;
                    out.push((k.to_vec(), v.to_vec()));
                }
            }
            _ => {
                for item in self.prefix_iter_rw(txn, &[])? {
                    let (k, v) = item?;
                    out.push((k.into_owned(), v.into_owned()));
                }
            }
        }
        Ok(out)
    }

    pub fn range<'txn>(
        &self,
        txn: &'txn ReadTxn<'_>,
        bounds: &(Bound<&[u8]>, Bound<&[u8]>),
    ) -> Result<KvIter<'txn>, KvError> {
        match (self, txn) {
            (RawDb::Lmdb(db), ReadTxn::Lmdb(rtxn)) => Ok(KvIter::LmdbRange(db.range(rtxn, bounds)?)),
            (RawDb::Sqlite { table }, ReadTxn::Sqlite(conn)) => {
                let rows = sqlite_range_scan(conn, table, bounds.0, bounds.1)?;
                Ok(KvIter::Owned(rows.into_iter()))
            }
            (RawDb::Redb { name }, ReadTxn::Redb(rtxn)) => {
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(name);
                let table = rtxn.open_table(def)?;
                let mut rows = Vec::new();
                for item in table.range::<&[u8]>((bounds.0, bounds.1))? {
                    let (k, v) = item?;
                    rows.push((k.value().to_vec(), v.value().to_vec()));
                }
                Ok(KvIter::Owned(rows.into_iter()))
            }
            (RawDb::Mem { name }, ReadTxn::Mem(guard)) => {
                let mut rows = Vec::new();
                if let Some(map) = guard.get(name) {
                    let s = match bounds.0 {
                        Bound::Included(b) => Bound::Included(b.to_vec()),
                        Bound::Excluded(b) => Bound::Excluded(b.to_vec()),
                        Bound::Unbounded => Bound::Unbounded,
                    };
                    let e = match bounds.1 {
                        Bound::Included(b) => Bound::Included(b.to_vec()),
                        Bound::Excluded(b) => Bound::Excluded(b.to_vec()),
                        Bound::Unbounded => Bound::Unbounded,
                    };
                    for (k, v) in map.range::<Vec<u8>, _>((s, e)) {
                        rows.push((k.clone(), v.clone()));
                    }
                }
                Ok(KvIter::Owned(rows.into_iter()))
            }
            _ => unreachable!("mismatched RawDb and ReadTxn backend"),
        }
    }

    pub fn iter<'txn>(&self, txn: &'txn ReadTxn<'_>) -> Result<KvIter<'txn>, KvError> {
        match (self, txn) {
            (RawDb::Lmdb(db), ReadTxn::Lmdb(rtxn)) => Ok(KvIter::Lmdb(db.iter(rtxn)?)),
            (RawDb::Sqlite { table }, ReadTxn::Sqlite(conn)) => {
                let sql = format!("SELECT k, v FROM \"{}\" ORDER BY k ASC", table);
                let mut stmt = conn.prepare_cached(&sql)?;
                let mut rows = stmt.query([])?;
                let mut out = Vec::new();
                while let Some(r) = rows.next()? {
                    out.push((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?));
                }
                Ok(KvIter::Owned(out.into_iter()))
            }
            (RawDb::Redb { name }, ReadTxn::Redb(rtxn)) => {
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(name);
                let table = rtxn.open_table(def)?;
                let mut out = Vec::new();
                for item in table.iter()? {
                    let (k, v) = item?;
                    out.push((k.value().to_vec(), v.value().to_vec()));
                }
                Ok(KvIter::Owned(out.into_iter()))
            }
            (RawDb::Mem { name }, ReadTxn::Mem(guard)) => {
                let out: Vec<_> = guard
                    .get(name)
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                Ok(KvIter::Owned(out.into_iter()))
            }
            _ => unreachable!("mismatched RawDb and ReadTxn backend"),
        }
    }

    pub fn len(&self, txn: &ReadTxn<'_>) -> Result<u64, KvError> {
        match (self, txn) {
            (RawDb::Lmdb(db), ReadTxn::Lmdb(rtxn)) => Ok(db.len(rtxn)?),
            (RawDb::Sqlite { table }, ReadTxn::Sqlite(conn)) => {
                let sql = format!("SELECT COUNT(*) FROM \"{}\"", table);
                let cnt: i64 = conn.query_row(&sql, [], |r| r.get(0))?;
                Ok(cnt as u64)
            }
            (RawDb::Redb { name }, ReadTxn::Redb(rtxn)) => {
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(name);
                let table = rtxn.open_table(def)?;
                Ok(table.len()?)
            }
            (RawDb::Mem { name }, ReadTxn::Mem(guard)) => {
                Ok(guard.get(name).map(|m| m.len() as u64).unwrap_or(0))
            }
            _ => unreachable!("mismatched RawDb and ReadTxn backend"),
        }
    }
}

fn mem_prefix_scan(
    map_opt: Option<&BTreeMap<Vec<u8>, Vec<u8>>>,
    prefix: &[u8],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut rows = Vec::new();
    if let Some(map) = map_opt {
        let (s, e) = prefix_bounds(prefix);
        for (k, v) in map.range::<Vec<u8>, _>((s, e)) {
            rows.push((k.clone(), v.clone()));
        }
    }
    rows
}

fn sqlite_prefix_scan(
    conn: &Connection,
    table: &str,
    prefix: &[u8],
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, KvError> {
    let mut out = Vec::new();
    if let Some(upper) = next_prefix(prefix) {
        let sql = format!(
            "SELECT k, v FROM \"{}\" WHERE k >= ?1 AND k < ?2 ORDER BY k ASC",
            table
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let mut rows = stmt.query(params![prefix, upper])?;
        while let Some(r) = rows.next()? {
            out.push((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?));
        }
    } else {
        let sql = format!("SELECT k, v FROM \"{}\" WHERE k >= ?1 ORDER BY k ASC", table);
        let mut stmt = conn.prepare_cached(&sql)?;
        let mut rows = stmt.query(params![prefix])?;
        while let Some(r) = rows.next()? {
            out.push((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?));
        }
    }
    Ok(out)
}

fn sqlite_range_scan(
    conn: &Connection,
    table: &str,
    start: Bound<&[u8]>,
    end: Bound<&[u8]>,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, KvError> {
    let (lo_op, lo_val) = match start {
        Bound::Included(b) => (Some(">="), Some(b)),
        Bound::Excluded(b) => (Some(">"), Some(b)),
        Bound::Unbounded => (None, None),
    };
    let (hi_op, hi_val) = match end {
        Bound::Included(b) => (Some("<="), Some(b)),
        Bound::Excluded(b) => (Some("<"), Some(b)),
        Bound::Unbounded => (None, None),
    };

    let mut out = Vec::new();
    match (lo_op, lo_val, hi_op, hi_val) {
        (Some(lo), Some(lv), Some(hi), Some(hv)) => {
            let sql = format!(
                "SELECT k, v FROM \"{}\" WHERE k {} ?1 AND k {} ?2 ORDER BY k ASC",
                table, lo, hi
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut rows = stmt.query(params![lv, hv])?;
            while let Some(r) = rows.next()? {
                out.push((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?));
            }
        }
        (Some(lo), Some(lv), None, None) => {
            let sql = format!(
                "SELECT k, v FROM \"{}\" WHERE k {} ?1 ORDER BY k ASC",
                table, lo
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut rows = stmt.query(params![lv])?;
            while let Some(r) = rows.next()? {
                out.push((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?));
            }
        }
        (None, None, Some(hi), Some(hv)) => {
            let sql = format!(
                "SELECT k, v FROM \"{}\" WHERE k {} ?1 ORDER BY k ASC",
                table, hi
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut rows = stmt.query(params![hv])?;
            while let Some(r) = rows.next()? {
                out.push((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?));
            }
        }
        _ => {
            let sql = format!("SELECT k, v FROM \"{}\" ORDER BY k ASC", table);
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                out.push((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?));
            }
        }
    }
    Ok(out)
}

pub enum KvStore {
    Lmdb { env: Env },
    Sqlite { conn: Mutex<Connection> },
    Redb { db: redb::Database },
    Mem {
        tables: RwLock<HashMap<String, BTreeMap<Vec<u8>, Vec<u8>>>>,
        writer: Mutex<()>,
    },
}

impl KvStore {
    /// Open or create an LMDB environment at `path` with the given `map_size` in bytes.
    pub fn open<P: AsRef<Path>>(path: P, map_size: usize, max_dbs: u32) -> Result<Self, KvError> {
        std::fs::create_dir_all(path.as_ref())?;
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(map_size)
                .max_dbs(max_dbs)
                .open(path.as_ref())?
        };
        Ok(Self::Lmdb { env })
    }

    /// Open or create a single-file SQLite database at `path` with WAL mode enabled.
    pub fn open_sqlite<P: AsRef<Path>>(path: P) -> Result<Self, KvError> {
        if let Some(parent) = path.as_ref().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path.as_ref())?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        Ok(Self::Sqlite {
            conn: Mutex::new(conn),
        })
    }

    /// Open or create a pure-Rust single-file `redb` database at `path`.
    pub fn open_redb<P: AsRef<Path>>(path: P) -> Result<Self, KvError> {
        if let Some(parent) = path.as_ref().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let db = redb::Database::create(path.as_ref())?;
        Ok(Self::Redb { db })
    }

    /// Create an ephemeral pure-Rust in-memory `BTreeMap` KV store.
    pub fn open_mem() -> Self {
        Self::Mem {
            tables: RwLock::new(HashMap::new()),
            writer: Mutex::new(()),
        }
    }

    pub fn open_db(&self, wtxn: &mut WriteTxn<'_>, name: &str) -> Result<RawDb, KvError> {
        match (self, wtxn) {
            (KvStore::Lmdb { env }, WriteTxn::Lmdb(t)) => {
                Ok(RawDb::Lmdb(env.create_database(t, Some(name))?))
            }
            (KvStore::Sqlite { .. }, WriteTxn::Sqlite(t)) => {
                let table = validate_table_name(name)?;
                let sql = format!(
                    "CREATE TABLE IF NOT EXISTS \"{}\" (k BLOB PRIMARY KEY, v BLOB NOT NULL) WITHOUT ROWID",
                    table
                );
                t.conn.execute_batch(&sql)?;
                Ok(RawDb::Sqlite { table })
            }
            (KvStore::Redb { .. }, WriteTxn::Redb(t)) => {
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(name);
                let _ = t.open_table(def)?;
                Ok(RawDb::Redb {
                    name: name.to_owned(),
                })
            }
            (KvStore::Mem { .. }, WriteTxn::Mem(t)) => {
                t.delta.entry(name.to_owned()).or_default();
                Ok(RawDb::Mem {
                    name: name.to_owned(),
                })
            }
            _ => unreachable!("mismatched KvStore and WriteTxn backend"),
        }
    }

    #[inline]
    pub fn read_txn(&self) -> Result<ReadTxn<'_>, KvError> {
        match self {
            KvStore::Lmdb { env } => Ok(ReadTxn::Lmdb(env.read_txn()?)),
            KvStore::Sqlite { conn } => Ok(ReadTxn::Sqlite(conn.lock().unwrap())),
            KvStore::Redb { db } => Ok(ReadTxn::Redb(db.begin_read()?)),
            KvStore::Mem { tables, .. } => Ok(ReadTxn::Mem(tables.read().unwrap())),
        }
    }

    #[inline]
    pub fn write_txn(&self) -> Result<WriteTxn<'_>, KvError> {
        match self {
            KvStore::Lmdb { env } => Ok(WriteTxn::Lmdb(env.write_txn()?)),
            KvStore::Sqlite { conn } => {
                let guard = conn.lock().unwrap();
                guard.execute_batch("BEGIN IMMEDIATE")?;
                Ok(WriteTxn::Sqlite(SqliteWriteTxn {
                    conn: guard,
                    committed: false,
                }))
            }
            KvStore::Redb { db } => Ok(WriteTxn::Redb(db.begin_write()?)),
            KvStore::Mem { tables, writer } => {
                let _writer = writer.lock().unwrap();
                Ok(WriteTxn::Mem(MemWriteTxn {
                    root: tables,
                    _writer,
                    delta: HashMap::new(),
                }))
            }
        }
    }

    pub fn sync(&self) -> Result<(), KvError> {
        match self {
            KvStore::Lmdb { env } => env.force_sync()?,
            KvStore::Sqlite { conn } => {
                let c = conn.lock().unwrap();
                c.execute_batch("PRAGMA wal_checkpoint(FULL);")?;
            }
            KvStore::Redb { .. } | KvStore::Mem { .. } => {}
        }
        Ok(())
    }

    /// Execute an atomic batch of Put/Del operations on a named DBI.
    pub fn transact_kv(&self, db: &RawDb, ops: &[KvOp<'_>]) -> Result<(), KvError> {
        let mut wtxn = self.write_txn()?;
        for op in ops {
            match op {
                KvOp::Put(k, v) => db.put(&mut wtxn, k, v)?,
                KvOp::Del(k) => {
                    db.delete(&mut wtxn, k)?;
                }
            }
        }
        wtxn.commit()?;
        Ok(())
    }

    #[inline]
    pub fn get_value<'txn>(
        &self,
        rtxn: &'txn ReadTxn<'_>,
        db: &RawDb,
        key: &[u8],
    ) -> Result<Option<Cow<'txn, [u8]>>, KvError> {
        db.get(rtxn, key)
    }

    /// Range scan `[start..end]` returning owned `(key, value)` pairs.
    pub fn get_range(
        &self,
        rtxn: &ReadTxn<'_>,
        db: &RawDb,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, KvError> {
        let mut out = Vec::new();
        for item in db.range(rtxn, &(start, end))? {
            let (k, v) = item?;
            out.push((k.into_owned(), v.into_owned()));
        }
        Ok(out)
    }

    pub fn entries(&self, rtxn: &ReadTxn<'_>, db: &RawDb) -> Result<u64, KvError> {
        db.len(rtxn)
    }

    /// Names of the named DBIs/tables in this store.
    pub fn list_dbis(&self) -> Result<Vec<String>, KvError> {
        Ok(match self {
            KvStore::Lmdb { env } => {
                let rtxn = env.read_txn()?;
                let mut names = Vec::new();
                if let Some(root) = env.open_database::<Bytes, Bytes>(&rtxn, None)? {
                    for item in root.iter(&rtxn)? {
                        let (k, _) = item?;
                        names.push(String::from_utf8_lossy(k).into_owned());
                    }
                }
                names
            }
            KvStore::Sqlite { conn } => {
                let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
                let mut stmt = conn.prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )?;
                // Tables are stored as `kv_<name>`; report the caller's name.
                let names = stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                names.into_iter().filter_map(|n| n.strip_prefix("kv_").map(str::to_owned)).collect()
            }
            KvStore::Redb { db } => {
                let rtxn = db.begin_read()?;
                rtxn.list_tables()?.map(|h| h.name().to_string()).collect()
            }
            KvStore::Mem { tables, .. } => tables.read().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect(),
        })
    }

    /// Removes a named DBI/table, returning whether it existed. On LMDB the database is emptied but its
    /// name stays in the environment's directory, because the pinned `heed` cannot delete named databases.
    pub fn drop_dbi(&self, name: &str) -> Result<bool, KvError> {
        match self {
            KvStore::Lmdb { env } => {
                let rtxn = env.read_txn()?;
                let db = env.open_database::<Bytes, Bytes>(&rtxn, Some(name))?;
                drop(rtxn);
                let Some(db) = db else { return Ok(false) };
                let mut wtxn = env.write_txn()?;
                db.clear(&mut wtxn)?;
                wtxn.commit()?;
                Ok(true)
            }
            KvStore::Sqlite { conn } => {
                let table = validate_table_name(name)?;
                let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
                let exists: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [&table],
                    |r| r.get(0),
                )?;
                if exists == 0 {
                    return Ok(false);
                }
                conn.execute_batch(&format!("DROP TABLE \"{}\"", table))?;
                Ok(true)
            }
            KvStore::Redb { db } => {
                let wtxn = db.begin_write()?;
                let def: TableDefinition<&[u8], &[u8]> = TableDefinition::new(name);
                let existed = wtxn.delete_table(def)?;
                wtxn.commit()?;
                Ok(existed)
            }
            KvStore::Mem { tables, writer } => {
                let _w = writer.lock().unwrap_or_else(|e| e.into_inner());
                Ok(tables.write().unwrap_or_else(|e| e.into_inner()).remove(name).is_some())
            }
        }
    }

    /// Writes a consistent copy of the store to `path`. LMDB and SQLite only.
    pub fn copy_to(&self, path: &Path) -> Result<(), KvError> {
        match self {
            KvStore::Lmdb { env } => {
                env.copy_to_file(path, heed::CompactionOption::Disabled)?;
                Ok(())
            }
            KvStore::Sqlite { conn } => {
                let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
                conn.execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
                Ok(())
            }
            KvStore::Redb { .. } => Err(KvError::Unsupported("copy_to on redb")),
            KvStore::Mem { .. } => Err(KvError::Unsupported("copy_to on the in-memory store")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn run_kv_ops(store: &KvStore) {
        let mut wtxn = store.write_txn().unwrap();
        let db = store.open_db(&mut wtxn, "test_db").unwrap();
        wtxn.commit().unwrap();

        store
            .transact_kv(
                &db,
                &[
                    KvOp::Put(b"sec:AA", b"1"),
                    KvOp::Put(b"sec:AAPL", b"2"),
                    KvOp::Put(b"sec:MSFT", b"3"),
                    KvOp::Del(b"sec:AA"),
                ],
            )
            .unwrap();

        let rtxn = store.read_txn().unwrap();
        assert_eq!(store.entries(&rtxn, &db).unwrap(), 2);
        assert_eq!(
            store.get_value(&rtxn, &db, b"sec:AAPL").unwrap().as_deref(),
            Some(b"2".as_slice())
        );

        let range = store
            .get_range(
                &rtxn,
                &db,
                Bound::Included(b"sec:A".as_slice()),
                Bound::Excluded(b"sec:M".as_slice()),
            )
            .unwrap();
        assert_eq!(range, vec![(b"sec:AAPL".to_vec(), b"2".to_vec())]);
    }

    #[test]
    fn kv_roundtrip_range_and_batch_all_backends() {
        let dir = tempdir().unwrap();
        run_kv_ops(&KvStore::open(dir.path().join("lmdb"), 16 * 1024 * 1024, 8).unwrap());
        run_kv_ops(&KvStore::open_sqlite(dir.path().join("test.sqlite")).unwrap());
        run_kv_ops(&KvStore::open_redb(dir.path().join("test.redb")).unwrap());
        run_kv_ops(&KvStore::open_mem());
    }
}
