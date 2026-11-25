//! Write-ahead log of applied transactions.
//!
//! Each record is `len (u32 LE) | crc32 (u32 LE) | payload`, where the payload is `lsn (u64 BE) | count (u32 BE)`
//! followed by encoded `TxItem`s. Items are the ones actually applied: transaction-function calls are logged
//! as the items they produced, so replay never re-runs user code.
//!
//! Durability: `Strict` fsyncs every append. `Relaxed` leaves the fsync to `Wal::sync`. Recovery stops at the
//! first record that is short or fails its checksum, and truncates the file there (a torn tail from a crash).

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use dtlv_codec::{decode_value, encode_value, Value};

use crate::{EntityRef, TxItem, TxVal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    Strict,
    Relaxed,
}

#[derive(Debug)]
pub struct Wal {
    file: File,
    durability: Durability,
    next_lsn: u64,
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    !c
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_be_bytes());
    out.extend_from_slice(b);
}

fn put_value(out: &mut Vec<u8>, v: &Value) {
    let mut enc = Vec::new();
    encode_value(v, &mut enc);
    put_bytes(out, &enc);
}

fn put_entity_ref(out: &mut Vec<u8>, r: &EntityRef) {
    match r {
        EntityRef::New => out.push(0),
        EntityRef::Id(id) => {
            out.push(1);
            out.extend_from_slice(&id.to_be_bytes());
        }
        EntityRef::TempId(t) => {
            out.push(2);
            put_bytes(out, t.as_bytes());
        }
        EntityRef::LookupRef(a, v) => {
            out.push(3);
            put_bytes(out, a.as_bytes());
            put_value(out, v);
        }
    }
}

fn put_tx_val(out: &mut Vec<u8>, v: &TxVal) {
    match v {
        TxVal::Val(x) => {
            out.push(0);
            put_value(out, x);
        }
        TxVal::TempRef(t) => {
            out.push(1);
            put_bytes(out, t.as_bytes());
        }
        TxVal::LookupRef(a, x) => {
            out.push(2);
            put_bytes(out, a.as_bytes());
            put_value(out, x);
        }
    }
}

fn encode_item(out: &mut Vec<u8>, item: &TxItem) {
    match item {
        TxItem::Assert(eid, pairs) => {
            out.push(0);
            match eid {
                Some(e) => {
                    out.push(1);
                    out.extend_from_slice(&e.to_be_bytes());
                }
                None => out.push(0),
            }
            out.extend_from_slice(&(pairs.len() as u32).to_be_bytes());
            for (a, v) in pairs {
                put_bytes(out, a.as_bytes());
                put_value(out, v);
            }
        }
        TxItem::AssertRef(r, pairs) => {
            out.push(1);
            put_entity_ref(out, r);
            out.extend_from_slice(&(pairs.len() as u32).to_be_bytes());
            for (a, v) in pairs {
                put_bytes(out, a.as_bytes());
                put_tx_val(out, v);
            }
        }
        TxItem::Retract(e, a, v) => {
            out.push(2);
            out.extend_from_slice(&e.to_be_bytes());
            put_bytes(out, a.as_bytes());
            put_value(out, v);
        }
        TxItem::RetractAttribute(e, a) => {
            out.push(3);
            out.extend_from_slice(&e.to_be_bytes());
            put_bytes(out, a.as_bytes());
        }
        TxItem::RetractEntity(e) => {
            out.push(4);
            out.extend_from_slice(&e.to_be_bytes());
        }
        TxItem::Cas(e, a, old, new) => {
            out.push(5);
            out.extend_from_slice(&e.to_be_bytes());
            put_bytes(out, a.as_bytes());
            put_value(out, old);
            put_value(out, new);
        }
        // Calls are expanded before logging; a logged `Call` would have no replayable meaning.
        TxItem::Call(..) => unreachable!("transaction calls are logged as the items they produced"),
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

type DecodeResult<T> = Result<T, String>;

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> DecodeResult<&'a [u8]> {
        if self.buf.len() - self.pos < n {
            return Err("truncated record".into());
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> DecodeResult<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> DecodeResult<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> DecodeResult<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn bytes(&mut self) -> DecodeResult<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    fn string(&mut self) -> DecodeResult<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|e| e.to_string())
    }

    fn value(&mut self) -> DecodeResult<Value> {
        decode_value(self.bytes()?).map_err(|e| e.to_string())
    }

    fn entity_ref(&mut self) -> DecodeResult<EntityRef> {
        Ok(match self.u8()? {
            0 => EntityRef::New,
            1 => EntityRef::Id(self.u64()?),
            2 => EntityRef::TempId(self.string()?),
            3 => EntityRef::LookupRef(self.string()?, self.value()?),
            t => return Err(format!("bad entity ref tag {t}")),
        })
    }

    fn tx_val(&mut self) -> DecodeResult<TxVal> {
        Ok(match self.u8()? {
            0 => TxVal::Val(self.value()?),
            1 => TxVal::TempRef(self.string()?),
            2 => TxVal::LookupRef(self.string()?, self.value()?),
            t => return Err(format!("bad tx value tag {t}")),
        })
    }

    fn item(&mut self) -> DecodeResult<TxItem> {
        Ok(match self.u8()? {
            0 => {
                let eid = match self.u8()? {
                    0 => None,
                    _ => Some(self.u64()?),
                };
                let n = self.u32()?;
                let mut pairs = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    pairs.push((self.string()?, self.value()?));
                }
                TxItem::Assert(eid, pairs)
            }
            1 => {
                let r = self.entity_ref()?;
                let n = self.u32()?;
                let mut pairs = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    pairs.push((self.string()?, self.tx_val()?));
                }
                TxItem::AssertRef(r, pairs)
            }
            2 => TxItem::Retract(self.u64()?, self.string()?, self.value()?),
            3 => TxItem::RetractAttribute(self.u64()?, self.string()?),
            4 => TxItem::RetractEntity(self.u64()?),
            5 => TxItem::Cas(self.u64()?, self.string()?, self.value()?, self.value()?),
            t => return Err(format!("bad item tag {t}")),
        })
    }
}

/// `None` marks an abort record: the transaction with that LSN did not commit to the store.
type Record = (u64, Option<Vec<TxItem>>);

const ABORT_COUNT: u32 = u32::MAX;

/// Drops aborted transactions. Their records stay in the log but are never applied.
fn committed(records: &[Record]) -> Vec<(u64, Vec<TxItem>)> {
    let aborted: std::collections::HashSet<u64> = records.iter().filter(|r| r.1.is_none()).map(|r| r.0).collect();
    records
        .iter()
        .filter(|r| !aborted.contains(&r.0))
        .filter_map(|(lsn, items)| items.clone().map(|i| (*lsn, i)))
        .collect()
}

/// Parses the valid prefix of the log. Returns the records and the byte length of that prefix.
fn parse(bytes: &[u8]) -> (Vec<Record>, usize) {
    let mut records = Vec::new();
    let mut pos = 0;
    while bytes.len() - pos >= 8 {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
        let start = pos + 8;
        if bytes.len() - start < len || crc32(&bytes[start..start + len]) != crc {
            break;
        }
        let mut r = Reader { buf: &bytes[start..start + len], pos: 0 };
        let parsed = (|| -> DecodeResult<Record> {
            let lsn = r.u64()?;
            let n = r.u32()?;
            if n == ABORT_COUNT {
                return Ok((lsn, None));
            }
            let mut items = Vec::with_capacity(n as usize);
            for _ in 0..n {
                items.push(r.item()?);
            }
            Ok((lsn, Some(items)))
        })();
        match parsed {
            Ok(rec) => records.push(rec),
            Err(_) => break,
        }
        pos = start + len;
    }
    (records, pos)
}

impl Wal {
    /// Opens (or creates) the log at `path`, truncating any torn tail. Returns the valid records and the
    /// log, with the next LSN set after both the records and `min_lsn` (the last LSN known to be applied).
    pub fn open(path: &Path, durability: Durability, min_lsn: u64) -> Result<(Wal, Vec<(u64, Vec<TxItem>)>), crate::DatalogError> {
        let mut bytes = Vec::new();
        if path.exists() {
            File::open(path)
                .and_then(|mut f| f.read_to_end(&mut bytes))
                .map_err(|e| crate::DatalogError::Wal(e.to_string()))?;
        }
        let (records, valid) = parse(&bytes);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| crate::DatalogError::Wal(e.to_string()))?;
        if valid < bytes.len() {
            file.set_len(valid as u64)
                .and_then(|_| file.sync_all())
                .map_err(|e| crate::DatalogError::Wal(e.to_string()))?;
        }
        let last = records.iter().map(|r| r.0).max().unwrap_or(0);
        let next_lsn = last.max(min_lsn) + 1;
        Ok((Wal { file, durability, next_lsn }, committed(&records)))
    }

    /// Appends one transaction and returns its LSN.
    pub fn append(&mut self, items: &[TxItem]) -> Result<u64, crate::DatalogError> {
        let lsn = self.next_lsn;
        let mut payload = Vec::new();
        payload.extend_from_slice(&lsn.to_be_bytes());
        payload.extend_from_slice(&(items.len() as u32).to_be_bytes());
        for item in items {
            encode_item(&mut payload, item);
        }
        let mut record = Vec::with_capacity(payload.len() + 8);
        record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        record.extend_from_slice(&crc32(&payload).to_le_bytes());
        record.extend_from_slice(&payload);
        let io = |e: std::io::Error| crate::DatalogError::Wal(e.to_string());
        self.file.write_all(&record).map_err(io)?;
        if self.durability == Durability::Strict {
            self.file.sync_data().map_err(io)?;
        }
        self.next_lsn += 1;
        Ok(lsn)
    }

    /// Marks transaction `lsn` as not committed to the store, so replay skips it.
    pub fn append_abort(&mut self, lsn: u64) -> Result<(), crate::DatalogError> {
        let mut payload = Vec::with_capacity(12);
        payload.extend_from_slice(&lsn.to_be_bytes());
        payload.extend_from_slice(&ABORT_COUNT.to_be_bytes());
        let mut record = Vec::with_capacity(payload.len() + 8);
        record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        record.extend_from_slice(&crc32(&payload).to_le_bytes());
        record.extend_from_slice(&payload);
        let io = |e: std::io::Error| crate::DatalogError::Wal(e.to_string());
        self.file.write_all(&record).map_err(io)?;
        if self.durability == Durability::Strict {
            self.file.sync_data().map_err(io)?;
        }
        Ok(())
    }

    /// Forces appended records to disk (the relaxed profile's durability point).
    pub fn sync(&mut self) -> Result<(), crate::DatalogError> {
        self.file.sync_data().map_err(|e| crate::DatalogError::Wal(e.to_string()))
    }

    /// Drops all records. Call only after the data they describe is durable in the store.
    pub fn truncate(&mut self) -> Result<(), crate::DatalogError> {
        self.file
            .set_len(0)
            .and_then(|_| self.file.sync_all())
            .map_err(|e| crate::DatalogError::Wal(e.to_string()))
    }
}

/// Reads the valid records of a log file without changing it.
pub fn read_records(path: &Path) -> Result<Vec<(u64, Vec<TxItem>)>, crate::DatalogError> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .map_err(|e| crate::DatalogError::Wal(e.to_string()))?;
    Ok(committed(&parse(&bytes).0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttrSpec, Conn};
    use dtlv_codec::Value;
    use tempfile::tempdir;

    fn schema() -> Vec<AttrSpec> {
        vec![AttrSpec::new(":w/name").unique_identity(), AttrSpec::new(":w/n"), AttrSpec::new(":w/tags").many()]
    }

    fn name(s: &str) -> (String, Value) {
        (":w/name".into(), Value::Str(s.into()))
    }

    fn lookup(conn: &Conn, s: &str) -> Option<u64> {
        let rtxn = conn.read_txn().unwrap();
        conn.lookup_entity_by_av(&rtxn, ":w/name", &Value::Str(s.into())).unwrap()
    }

    fn count_entities(conn: &Conn) -> usize {
        let rtxn = conn.read_txn().unwrap();
        conn.search_datoms(&rtxn, None, Some(":w/name"), None).unwrap().len()
    }

    #[test]
    fn a_record_appended_but_not_committed_is_replayed_on_open() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db");
        let conn = Conn::open_wal(&db, dir.path(), schema(), Durability::Strict).unwrap();
        conn.transact(&[TxItem::Assert(None, vec![name("A")])]).unwrap();
        drop(conn);

        // Simulate a crash after the log append but before the store commit: the record is only in the log.
        let (mut log, _) = Wal::open(&dir.path().join("wal.log"), Durability::Strict, 1).unwrap();
        log.append(&[TxItem::Assert(None, vec![name("B")])]).unwrap();
        drop(log);

        let conn = Conn::open_wal(&db, dir.path(), schema(), Durability::Strict).unwrap();
        assert!(lookup(&conn, "A").is_some());
        assert!(lookup(&conn, "B").is_some(), "logged-but-uncommitted write was not replayed");
        assert_eq!(conn.applied_lsn().unwrap(), 2);
    }

    #[test]
    fn aborted_records_are_skipped_on_replay() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db");
        let conn = Conn::open_wal(&db, dir.path(), schema(), Durability::Strict).unwrap();
        conn.transact(&[TxItem::Assert(None, vec![name("A")])]).unwrap();
        drop(conn);

        // A logged transaction whose store commit failed: appended, then marked aborted.
        let (mut log, _) = Wal::open(&dir.path().join("wal.log"), Durability::Strict, 1).unwrap();
        let lsn = log.append(&[TxItem::Assert(None, vec![name("Ghost")])]).unwrap();
        log.append_abort(lsn).unwrap();
        drop(log);

        let conn = Conn::open_wal(&db, dir.path(), schema(), Durability::Strict).unwrap();
        assert!(lookup(&conn, "Ghost").is_none(), "aborted transaction was replayed");
        assert!(lookup(&conn, "A").is_some());
        assert_eq!(read_records(&dir.path().join("wal.log")).unwrap().len(), 1);
    }

    #[test]
    fn replay_does_not_reapply_committed_records() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db");
        {
            let conn = Conn::open_wal(&db, dir.path(), schema(), Durability::Strict).unwrap();
            conn.transact(&[TxItem::Assert(None, vec![name("A")])]).unwrap();
            conn.transact(&[TxItem::Assert(None, vec![name("B")])]).unwrap();
        }
        for _ in 0..2 {
            let conn = Conn::open_wal(&db, dir.path(), schema(), Durability::Strict).unwrap();
            assert_eq!(count_entities(&conn), 2);
        }
    }

    #[test]
    fn torn_tail_is_discarded_and_the_log_keeps_working() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db");
        {
            let conn = Conn::open_wal(&db, dir.path(), schema(), Durability::Strict).unwrap();
            conn.transact(&[TxItem::Assert(None, vec![name("A")])]).unwrap();
        }
        let log_path = dir.path().join("wal.log");
        let valid_len = std::fs::metadata(&log_path).unwrap().len();
        std::fs::OpenOptions::new().append(true).open(&log_path).unwrap().write_all(&[9, 0, 0, 0, 1, 2]).unwrap();

        let conn = Conn::open_wal(&db, dir.path(), schema(), Durability::Strict).unwrap();
        assert_eq!(std::fs::metadata(&log_path).unwrap().len(), valid_len, "garbage tail not truncated");
        conn.transact(&[TxItem::Assert(None, vec![name("C")])]).unwrap();
        assert_eq!(read_records(&log_path).unwrap().len(), 2);
    }

    #[test]
    fn checkpoint_truncates_the_log_and_lsns_keep_increasing() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db");
        let log_path = dir.path().join("wal.log");
        let conn = Conn::open_wal(&db, dir.path(), schema(), Durability::Relaxed).unwrap();
        conn.transact(&[TxItem::Assert(None, vec![name("A")])]).unwrap();
        conn.sync_wal().unwrap();
        conn.checkpoint().unwrap();
        assert_eq!(std::fs::metadata(&log_path).unwrap().len(), 0);
        drop(conn);

        let conn = Conn::open_wal(&db, dir.path(), schema(), Durability::Relaxed).unwrap();
        conn.transact(&[TxItem::Assert(None, vec![name("B")])]).unwrap();
        let recs = read_records(&log_path).unwrap();
        assert_eq!(recs.len(), 1);
        assert!(recs[0].0 > 1, "lsn reused after checkpoint: {}", recs[0].0);
    }

    #[test]
    fn transaction_calls_are_logged_as_the_items_they_produced() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db");
        let mut conn = Conn::open_wal(&db, dir.path(), schema(), Durability::Strict).unwrap();
        conn.register_tx_fn(":w/add", |_view, args| {
            Ok(vec![TxItem::Assert(None, vec![(":w/name".into(), args[0].clone())])])
        });
        conn.transact(&[TxItem::Call(":w/add".into(), vec![Value::Str("Via".into())])]).unwrap();
        drop(conn);

        let recs = read_records(&dir.path().join("wal.log")).unwrap();
        let items = &recs[0].1;
        assert!(items.iter().all(|i| !matches!(i, TxItem::Call(..))));
        assert!(matches!(items[0], TxItem::Assert(None, _)));

        // Reopening needs no registration: replay never calls the function.
        let conn = Conn::open_wal(&db, dir.path(), schema(), Durability::Strict).unwrap();
        assert!(lookup(&conn, "Via").is_some());
    }

    #[test]
    fn every_item_variant_round_trips_through_the_log() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("wal.log");
        let items = vec![
            TxItem::Assert(Some(7), vec![(":w/n".into(), Value::I64(-3)), (":w/tags".into(), Value::Bool(true))]),
            TxItem::AssertRef(
                EntityRef::TempId("t".into()),
                vec![(":w/n".into(), TxVal::Val(Value::F64(2.5))), (":w/n".into(), TxVal::TempRef("u".into()))],
            ),
            TxItem::AssertRef(
                EntityRef::LookupRef(":w/name".into(), Value::Str("A".into())),
                vec![(":w/n".into(), TxVal::LookupRef(":w/name".into(), Value::Str("B".into())))],
            ),
            TxItem::AssertRef(EntityRef::Id(4), vec![]),
            TxItem::AssertRef(EntityRef::New, vec![]),
            TxItem::Retract(5, ":w/n".into(), Value::Keyword(":k".into())),
            TxItem::RetractAttribute(6, ":w/tags".into()),
            TxItem::RetractEntity(8),
            TxItem::Cas(9, ":w/n".into(), Value::I64(1), Value::I64(2)),
        ];
        let (mut log, _) = Wal::open(&log_path, Durability::Relaxed, 0).unwrap();
        let lsn = log.append(&items).unwrap();
        log.sync().unwrap();
        drop(log);
        let recs = read_records(&log_path).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].0, lsn);
        assert_eq!(format!("{:?}", recs[0].1), format!("{:?}", items));
    }
}
