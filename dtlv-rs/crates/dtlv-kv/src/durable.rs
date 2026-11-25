//! Durable key-value writes: a log of applied batches in front of a `KvStore`.
//!
//! A batch is appended to `wal.log` before its store commit. Its applied position is written in the same store
//! transaction as the data, so a crash between append and commit is recovered by replaying the record on open.
//! Records are `len (u32 LE) | crc32 (u32 LE) | payload`; payload is `lsn (u64 BE) | count (u32 BE) | ops`, where each
//! op is `table (len-prefixed) | tag (0 put, 1 delete) | key (len-prefixed) | [value (len-prefixed) if put]`.
//! Recovery stops at the first short or corrupt record and truncates the file there.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::{KvError, KvStore, RawDb};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    Strict,
    Relaxed,
}

/// One change inside a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Put { table: String, key: Vec<u8>, val: Vec<u8> },
    Delete { table: String, key: Vec<u8> },
}

const APPLIED_KEY: &[u8] = b"__applied_lsn";
const META_TABLE: &str = "__wal_meta";

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

fn put(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_be_bytes());
    out.extend_from_slice(b);
}

fn encode(lsn: u64, ops: &[Op]) -> Vec<u8> {
    let mut payload = lsn.to_be_bytes().to_vec();
    payload.extend_from_slice(&(ops.len() as u32).to_be_bytes());
    for op in ops {
        match op {
            Op::Put { table, key, val } => {
                put(&mut payload, table.as_bytes());
                payload.push(0);
                put(&mut payload, key);
                put(&mut payload, val);
            }
            Op::Delete { table, key } => {
                put(&mut payload, table.as_bytes());
                payload.push(1);
                put(&mut payload, key);
            }
        }
    }
    let mut rec = Vec::with_capacity(payload.len() + 8);
    rec.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    rec.extend_from_slice(&crc32(&payload).to_le_bytes());
    rec.extend_from_slice(&payload);
    rec
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.buf.len() - self.pos < n {
            return None;
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

fn decode_payload(p: &[u8]) -> Option<(u64, Vec<Op>)> {
    let mut c = Cursor { buf: p, pos: 0 };
    let lsn = u64::from_be_bytes(c.take(8)?.try_into().ok()?);
    let n = c.u32()?;
    let mut ops = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let table = String::from_utf8(c.bytes()?.to_vec()).ok()?;
        let op = match c.u8()? {
            0 => Op::Put { table, key: c.bytes()?.to_vec(), val: c.bytes()?.to_vec() },
            1 => Op::Delete { table, key: c.bytes()?.to_vec() },
            _ => return None,
        };
        ops.push(op);
    }
    Some((lsn, ops))
}

/// Parses the valid prefix of a log: `(lsn, ops)` per committed-or-not record, and the prefix length.
fn parse(bytes: &[u8]) -> (Vec<(u64, Vec<Op>)>, usize) {
    let mut out = Vec::new();
    let mut pos = 0;
    while bytes.len() - pos >= 8 {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
        let start = pos + 8;
        if bytes.len() - start < len || crc32(&bytes[start..start + len]) != crc {
            break;
        }
        match decode_payload(&bytes[start..start + len]) {
            Some(rec) => out.push(rec),
            None => break,
        }
        pos = start + len;
    }
    (out, pos)
}

fn io(e: std::io::Error) -> KvError {
    KvError::Io(e)
}

/// A `KvStore` whose batched writes are logged and replayed after a crash.
pub struct DurableKv {
    store: KvStore,
    meta: RawDb,
    log_path: PathBuf,
    durability: Durability,
    state: Mutex<LogState>,
}

struct LogState {
    file: File,
    next_lsn: u64,
}

impl DurableKv {
    /// Opens `store` with its log in `dir`, replaying records the store has not applied.
    pub fn open(store: KvStore, dir: &Path, durability: Durability) -> Result<Self, KvError> {
        std::fs::create_dir_all(dir)?;
        let log_path = dir.join("wal.log");
        let mut w = store.write_txn()?;
        let meta = store.open_db(&mut w, META_TABLE)?;
        w.commit()?;
        let applied = {
            let r = store.read_txn()?;
            match meta.get(&r, APPLIED_KEY)? {
                Some(b) if b.len() == 8 => u64::from_be_bytes(b.as_ref().try_into().unwrap()),
                _ => 0,
            }
        };
        let mut bytes = Vec::new();
        if log_path.exists() {
            File::open(&log_path).and_then(|mut f| f.read_to_end(&mut bytes)).map_err(io)?;
        }
        let (records, valid) = parse(&bytes);
        let file = OpenOptions::new().create(true).append(true).open(&log_path).map_err(io)?;
        if valid < bytes.len() {
            file.set_len(valid as u64).and_then(|_| file.sync_all()).map_err(io)?;
        }
        let last = records.iter().map(|r| r.0).max().unwrap_or(0);
        let kv = DurableKv {
            store,
            meta,
            log_path,
            durability,
            state: Mutex::new(LogState { file, next_lsn: last.max(applied) + 1 }),
        };
        for (lsn, ops) in records {
            if lsn > applied {
                kv.apply(&ops, Some(lsn))?;
            }
        }
        Ok(kv)
    }

    pub fn store(&self) -> &KvStore {
        &self.store
    }

    /// Applies a batch atomically and durably. The batch is logged before the store commit.
    pub fn write(&self, ops: &[Op]) -> Result<u64, KvError> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let lsn = st.next_lsn;
        let rec = encode(lsn, ops);
        st.file.write_all(&rec).map_err(io)?;
        if self.durability == Durability::Strict {
            st.file.sync_data().map_err(io)?;
        }
        self.apply(ops, Some(lsn))?;
        st.next_lsn += 1;
        Ok(lsn)
    }

    /// Writes `ops` and records `lsn` as applied in one store transaction.
    fn apply(&self, ops: &[Op], lsn: Option<u64>) -> Result<(), KvError> {
        let mut w = self.store.write_txn()?;
        for op in ops {
            match op {
                Op::Put { table, key, val } => {
                    let db = self.store.open_db(&mut w, table)?;
                    db.put(&mut w, key, val)?;
                }
                Op::Delete { table, key } => {
                    let db = self.store.open_db(&mut w, table)?;
                    db.delete(&mut w, key)?;
                }
            }
        }
        if let Some(lsn) = lsn {
            self.meta.put(&mut w, APPLIED_KEY, &lsn.to_be_bytes())?;
        }
        w.commit()
    }

    /// Last log record applied to the store.
    pub fn applied_lsn(&self) -> Result<u64, KvError> {
        let r = self.store.read_txn()?;
        Ok(match self.meta.get(&r, APPLIED_KEY)? {
            Some(v) if v.len() == 8 => u64::from_be_bytes(v.as_ref().try_into().unwrap()),
            _ => 0,
        })
    }

    /// Makes the store durable, then drops the log, which recovery no longer needs.
    pub fn checkpoint(&self) -> Result<(), KvError> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.store.sync()?;
        st.file.set_len(0).and_then(|_| st.file.sync_all()).map_err(io)
    }

    /// Writes a copy of the store to `path`. The log is untouched.
    pub fn snapshot(&self, path: &Path) -> Result<(), KvError> {
        self.store.copy_to(path)
    }

    /// Records in the log that the store has not yet checkpointed (for inspection and tests).
    pub fn log_records(&self) -> Result<Vec<(u64, Vec<Op>)>, KvError> {
        let mut bytes = Vec::new();
        File::open(&self.log_path).and_then(|mut f| f.read_to_end(&mut bytes)).map_err(io)?;
        Ok(parse(&bytes).0)
    }
}

/// Appends a raw record to a log file without applying it (simulates a crash after append).
pub fn append_unapplied(dir: &Path, lsn: u64, ops: &[Op]) -> Result<(), KvError> {
    let mut f = OpenOptions::new().create(true).append(true).open(dir.join("wal.log")).map_err(io)?;
    f.write_all(&encode(lsn, ops)).map_err(io)
}
