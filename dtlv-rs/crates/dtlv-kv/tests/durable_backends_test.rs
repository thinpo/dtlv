//! DurableKv on the backends the main durability tests skip: redb (file-backed) and the in-memory store.

use dtlv_kv::durable::{append_unapplied, Durability, DurableKv, Op};
use dtlv_kv::{KvError, KvStore};
use tempfile::tempdir;

fn put(k: &str, v: &str) -> Op {
    Op::Put { table: "t".into(), key: k.into(), val: v.into() }
}

fn get(kv: &DurableKv, key: &str) -> Option<Vec<u8>> {
    let mut w = kv.store().write_txn().unwrap();
    let db = kv.store().open_db(&mut w, "t").unwrap();
    let v = db.get_rw(&w, key.as_bytes()).unwrap().map(|v| v.to_vec());
    w.commit().unwrap();
    v
}

#[test]
fn redb_batches_survive_reopen_and_replay() {
    let dir = tempdir().unwrap();
    {
        let kv = DurableKv::open(KvStore::open_redb(dir.path().join("r.redb")).unwrap(), dir.path(), Durability::Strict).unwrap();
        kv.write(&[put("a", "1")]).unwrap();
    }
    // A logged-but-unapplied batch must be replayed on redb as well.
    append_unapplied(dir.path(), 2, &[put("crash", "replayed")]).unwrap();
    let kv = DurableKv::open(KvStore::open_redb(dir.path().join("r.redb")).unwrap(), dir.path(), Durability::Strict).unwrap();
    assert_eq!(get(&kv, "a"), Some(b"1".to_vec()));
    assert_eq!(get(&kv, "crash"), Some(b"replayed".to_vec()));
    assert_eq!(kv.applied_lsn().unwrap(), 2);
}

#[test]
fn redb_checkpoint_truncates_the_log() {
    let dir = tempdir().unwrap();
    let kv = DurableKv::open(KvStore::open_redb(dir.path().join("r.redb")).unwrap(), dir.path(), Durability::Relaxed).unwrap();
    kv.write(&[put("a", "1")]).unwrap();
    kv.checkpoint().unwrap();
    assert!(kv.log_records().unwrap().is_empty());
}

#[test]
fn redb_snapshot_is_reported_unsupported() {
    let dir = tempdir().unwrap();
    let kv = DurableKv::open(KvStore::open_redb(dir.path().join("r.redb")).unwrap(), dir.path(), Durability::Strict).unwrap();
    assert!(matches!(kv.snapshot(&dir.path().join("copy")), Err(KvError::Unsupported(_))));
}

#[test]
fn in_memory_store_applies_logs_and_checkpoints_within_a_session() {
    // The in-memory store has no persistence, so only the write path and log bookkeeping are checked.
    let dir = tempdir().unwrap();
    let kv = DurableKv::open(KvStore::open_mem(), dir.path(), Durability::Strict).unwrap();
    kv.write(&[put("a", "1")]).unwrap();
    kv.write(&[Op::Delete { table: "t".into(), key: b"a".to_vec() }]).unwrap();
    assert_eq!(get(&kv, "a"), None);
    assert_eq!(kv.log_records().unwrap().len(), 2);
    kv.checkpoint().unwrap();
    assert!(kv.log_records().unwrap().is_empty());
    assert!(matches!(kv.snapshot(&dir.path().join("x")), Err(KvError::Unsupported(_))));
}
