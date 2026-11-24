use std::ops::Bound;
use std::path::Path;
use heed::types::Bytes;
use heed::{Database, Env, EnvOpenOptions, RoTxn, RwTxn};
use thiserror::Error;

pub type RawDb = Database<Bytes, Bytes>;

#[derive(Debug, Error)]
pub enum KvError {
    #[error("lmdb error: {0}")]
    Heed(#[from] heed::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub enum KvOp<'a> {
    Put(&'a [u8], &'a [u8]),
    Del(&'a [u8]),
}

pub struct KvStore {
    env: Env,
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
        Ok(Self { env })
    }

    pub fn open_db(&self, wtxn: &mut RwTxn, name: &str) -> Result<RawDb, KvError> {
        Ok(self.env.create_database(wtxn, Some(name))?)
    }

    #[inline]
    pub fn read_txn(&self) -> Result<RoTxn<'_>, KvError> {
        Ok(self.env.read_txn()?)
    }

    #[inline]
    pub fn write_txn(&self) -> Result<RwTxn<'_>, KvError> {
        Ok(self.env.write_txn()?)
    }

    pub fn sync(&self) -> Result<(), KvError> {
        self.env.force_sync()?;
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
        rtxn: &'txn RoTxn<'_>,
        db: &RawDb,
        key: &[u8],
    ) -> Result<Option<&'txn [u8]>, KvError> {
        Ok(db.get(rtxn, key)?)
    }

    /// Range scan `[start..end]` returning owned `(key, value)` pairs.
    pub fn get_range(
        &self,
        rtxn: &RoTxn<'_>,
        db: &RawDb,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, KvError> {
        let mut out = Vec::new();
        for item in db.range(rtxn, &(start, end))? {
            let (k, v) = item?;
            out.push((k.to_vec(), v.to_vec()));
        }
        Ok(out)
    }

    pub fn entries(&self, rtxn: &RoTxn<'_>, db: &RawDb) -> Result<u64, KvError> {
        Ok(db.len(rtxn)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn kv_roundtrip_range_and_batch() {
        let dir = tempdir().unwrap();
        let store = KvStore::open(dir.path(), 16 * 1024 * 1024, 8).unwrap();
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
        assert_eq!(store.get_value(&rtxn, &db, b"sec:AAPL").unwrap(), Some(b"2".as_slice()));

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
}
