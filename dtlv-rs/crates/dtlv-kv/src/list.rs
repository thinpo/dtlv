//! Dupsort-style list DBIs and ordered-DBI utilities, built on the plain `RawDb` operations so they
//! behave the same on every backend.
//!
//! A list item `(key, val)` is stored as the key `escaped(key) ++ val` with an empty value. The escape
//! is prefix-free, so all items of one key share exactly the prefix `escaped(key)`, and their values
//! come back in byte order, as in LMDB dupsort.

use std::ops::Bound;

use dtlv_codec::{decode_escaped_bytes, encode_escaped_bytes};

use crate::{KvError, KvStore, RawDb, ReadTxn, WriteTxn};

#[derive(Clone)]
pub struct ListDb {
    db: RawDb,
}

fn key_prefix(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len() + 2);
    encode_escaped_bytes(key, &mut out);
    out
}

fn item_key(key: &[u8], val: &[u8]) -> Vec<u8> {
    let mut out = key_prefix(key);
    out.extend_from_slice(val);
    out
}

impl ListDb {
    pub fn open(store: &KvStore, wtxn: &mut WriteTxn<'_>, name: &str) -> Result<Self, KvError> {
        Ok(Self { db: store.open_db(wtxn, name)? })
    }

    pub fn put_item(&self, wtxn: &mut WriteTxn<'_>, key: &[u8], val: &[u8]) -> Result<(), KvError> {
        self.db.put(wtxn, &item_key(key, val), &[])
    }

    pub fn del_item(&self, wtxn: &mut WriteTxn<'_>, key: &[u8], val: &[u8]) -> Result<bool, KvError> {
        self.db.delete(wtxn, &item_key(key, val))
    }

    /// All values of `key`, in byte order.
    pub fn values(&self, rtxn: &ReadTxn<'_>, key: &[u8]) -> Result<Vec<Vec<u8>>, KvError> {
        let prefix = key_prefix(key);
        let mut out = Vec::new();
        for item in self.db.prefix_iter(rtxn, &prefix)? {
            let (k, _) = item?;
            out.push(k[prefix.len()..].to_vec());
        }
        Ok(out)
    }

    pub fn count(&self, rtxn: &ReadTxn<'_>, key: &[u8]) -> Result<u64, KvError> {
        let prefix = key_prefix(key);
        let mut n = 0;
        for item in self.db.prefix_iter(rtxn, &prefix)? {
            item?;
            n += 1;
        }
        Ok(n)
    }

    /// Removes every value of `key`, returning how many were removed.
    pub fn clear_key(&self, wtxn: &mut WriteTxn<'_>, key: &[u8]) -> Result<u64, KvError> {
        let prefix = key_prefix(key);
        let keys = collect_keys_rw(&self.db, wtxn, &prefix)?;
        for k in &keys {
            self.db.delete(wtxn, k)?;
        }
        Ok(keys.len() as u64)
    }

    /// Every distinct key with its sorted values, in key order.
    pub fn all(&self, rtxn: &ReadTxn<'_>) -> Result<Vec<(Vec<u8>, Vec<Vec<u8>>)>, KvError> {
        let mut out: Vec<(Vec<u8>, Vec<Vec<u8>>)> = Vec::new();
        for item in self.db.iter(rtxn)? {
            let (k, _) = item?;
            let (raw, val) = decode_escaped_bytes(&k).map_err(|_| KvError::InvalidListItem)?;
            match out.last_mut() {
                Some((last, vals)) if *last == raw => vals.push(val.to_vec()),
                _ => out.push((raw, vec![val.to_vec()])),
            }
        }
        Ok(out)
    }
}

fn collect_keys_rw(db: &RawDb, wtxn: &WriteTxn<'_>, prefix: &[u8]) -> Result<Vec<Vec<u8>>, KvError> {
    if prefix.is_empty() {
        return Ok(db.iter_rw(wtxn)?.into_iter().map(|(k, _)| k).collect());
    }
    let mut keys = Vec::new();
    for item in db.prefix_iter_rw(wtxn, prefix)? {
        let (k, _) = item?;
        keys.push(k.into_owned());
    }
    Ok(keys)
}

/// Ordered-DBI utilities. Rank and count walk the index, so they are O(n); a counted B-tree would make
/// them O(log n).
impl RawDb {
    /// Number of entries whose key sorts before `key`.
    pub fn rank(&self, rtxn: &ReadTxn<'_>, key: &[u8]) -> Result<u64, KvError> {
        let mut n = 0;
        for item in self.range(rtxn, &(Bound::Unbounded, Bound::Excluded(key)))? {
            item?;
            n += 1;
        }
        Ok(n)
    }

    /// The entry at zero-based position `n` in key order.
    pub fn by_rank(&self, rtxn: &ReadTxn<'_>, n: u64) -> Result<Option<(Vec<u8>, Vec<u8>)>, KvError> {
        match self.iter(rtxn)?.nth(n as usize) {
            Some(item) => {
                let (k, v) = item?;
                Ok(Some((k.into_owned(), v.into_owned())))
            }
            None => Ok(None),
        }
    }

    /// Number of entries whose key lies in `[start, end]` bounds.
    pub fn key_range_count(
        &self,
        rtxn: &ReadTxn<'_>,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<u64, KvError> {
        let mut n = 0;
        for item in self.range(rtxn, &(start, end))? {
            item?;
            n += 1;
        }
        Ok(n)
    }

    /// The first `n` entries in key order.
    pub fn first_n(&self, rtxn: &ReadTxn<'_>, n: usize) -> Result<Vec<(Vec<u8>, Vec<u8>)>, KvError> {
        let mut out = Vec::with_capacity(n);
        for item in self.iter(rtxn)?.take(n) {
            let (k, v) = item?;
            out.push((k.into_owned(), v.into_owned()));
        }
        Ok(out)
    }

    /// Removes every entry, returning how many were removed.
    pub fn clear(&self, wtxn: &mut WriteTxn<'_>) -> Result<u64, KvError> {
        let keys = collect_keys_rw(self, wtxn, &[])?;
        for k in &keys {
            self.delete(wtxn, k)?;
        }
        Ok(keys.len() as u64)
    }
}
