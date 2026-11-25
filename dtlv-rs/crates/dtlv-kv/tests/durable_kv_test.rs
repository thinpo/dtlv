use std::io::Write;

use dtlv_kv::durable::{append_unapplied, Durability, DurableKv, Op};
use dtlv_kv::KvStore;
use tempfile::tempdir;

fn put(t: &str, k: &str, v: &str) -> Op {
    Op::Put { table: t.into(), key: k.into(), val: v.into() }
}

fn get(kv: &DurableKv, table: &str, key: &str) -> Option<Vec<u8>> {
    // One write transaction only: SQLite serializes connections, so a read held alongside a write deadlocks.
    let mut w = kv.store().write_txn().unwrap();
    let db = kv.store().open_db(&mut w, table).unwrap();
    let v = db.get_rw(&w, key.as_bytes()).unwrap().map(|v| v.to_vec());
    w.commit().unwrap();
    v
}

fn open_lmdb(dir: &std::path::Path, dur: Durability) -> DurableKv {
    DurableKv::open(KvStore::open(dir.join("db"), 64 << 20, 16).unwrap(), dir, dur).unwrap()
}

#[test]
fn committed_batches_survive_reopen_on_lmdb() {
    let dir = tempdir().unwrap();
    {
        let kv = open_lmdb(dir.path(), Durability::Strict);
        kv.write(&[put("t", "a", "1"), put("t", "b", "2")]).unwrap();
        kv.write(&[Op::Delete { table: "t".into(), key: b"a".to_vec() }]).unwrap();
    }
    let kv = open_lmdb(dir.path(), Durability::Strict);
    assert_eq!(get(&kv, "t", "b"), Some(b"2".to_vec()));
    assert_eq!(get(&kv, "t", "a"), None);
    assert_eq!(kv.applied_lsn().unwrap(), 2);
}

#[test]
fn a_record_logged_but_not_applied_is_replayed_on_open() {
    let dir = tempdir().unwrap();
    {
        let kv = open_lmdb(dir.path(), Durability::Strict);
        kv.write(&[put("t", "a", "1")]).unwrap();
    }
    // Crash after the append, before the store commit.
    append_unapplied(dir.path(), 2, &[put("t", "crash", "survived")]).unwrap();
    let kv = open_lmdb(dir.path(), Durability::Strict);
    assert_eq!(get(&kv, "t", "crash"), Some(b"survived".to_vec()));
    assert_eq!(kv.applied_lsn().unwrap(), 2);
}

#[test]
fn applied_records_are_not_replayed_again() {
    let dir = tempdir().unwrap();
    {
        let kv = open_lmdb(dir.path(), Durability::Strict);
        kv.write(&[put("t", "k", "v1")]).unwrap();
        kv.write(&[put("t", "k", "v2")]).unwrap();
    }
    // Reopen twice; a replay of record 1 would overwrite v2 with v1.
    let kv = open_lmdb(dir.path(), Durability::Strict);
    drop(kv);
    let kv = open_lmdb(dir.path(), Durability::Strict);
    assert_eq!(get(&kv, "t", "k"), Some(b"v2".to_vec()));
}

#[test]
fn a_torn_tail_is_truncated_and_the_log_keeps_working() {
    let dir = tempdir().unwrap();
    {
        let kv = open_lmdb(dir.path(), Durability::Strict);
        kv.write(&[put("t", "a", "1")]).unwrap();
    }
    let log = dir.path().join("wal.log");
    let valid = std::fs::metadata(&log).unwrap().len();
    std::fs::OpenOptions::new().append(true).open(&log).unwrap().write_all(&[7, 0, 0, 0, 1]).unwrap();
    let kv = open_lmdb(dir.path(), Durability::Strict);
    assert_eq!(std::fs::metadata(&log).unwrap().len(), valid);
    kv.write(&[put("t", "b", "2")]).unwrap();
    assert_eq!(kv.log_records().unwrap().len(), 2);
}

#[test]
fn checkpoint_drops_the_log_and_lsns_keep_increasing() {
    let dir = tempdir().unwrap();
    let kv = open_lmdb(dir.path(), Durability::Relaxed);
    kv.write(&[put("t", "a", "1")]).unwrap();
    kv.checkpoint().unwrap();
    assert!(kv.log_records().unwrap().is_empty());
    drop(kv);
    let kv = open_lmdb(dir.path(), Durability::Relaxed);
    kv.write(&[put("t", "b", "2")]).unwrap();
    let recs = kv.log_records().unwrap();
    assert_eq!(recs.len(), 1);
    assert!(recs[0].0 > 1, "lsn reused after checkpoint");
}

#[test]
fn a_snapshot_opens_independently_of_the_live_store() {
    let dir = tempdir().unwrap();
    let kv = open_lmdb(dir.path(), Durability::Strict);
    kv.write(&[put("t", "a", "snap")]).unwrap();
    let copy = dir.path().join("copy");
    std::fs::create_dir_all(&copy).unwrap();
    kv.snapshot(&copy.join("data.mdb")).unwrap();
    kv.write(&[put("t", "a", "later")]).unwrap();
    drop(kv);
    let snap = DurableKv::open(KvStore::open(&copy, 64 << 20, 16).unwrap(), &copy.join("log"), Durability::Strict).unwrap();
    assert_eq!(get(&snap, "t", "a"), Some(b"snap".to_vec()));
}

#[test]
fn sqlite_backed_batches_survive_reopen() {
    let dir = tempdir().unwrap();
    {
        let kv = DurableKv::open(KvStore::open_sqlite(dir.path().join("s.db")).unwrap(), dir.path(), Durability::Strict).unwrap();
        kv.write(&[put("t", "k", "sqlite")]).unwrap();
    }
    let kv = DurableKv::open(KvStore::open_sqlite(dir.path().join("s.db")).unwrap(), dir.path(), Durability::Strict).unwrap();
    assert_eq!(get(&kv, "t", "k"), Some(b"sqlite".to_vec()));
    assert_eq!(kv.applied_lsn().unwrap(), 1);
}
