use dtlv_kv::{KvError, KvStore};
use tempfile::tempdir;

/// Each backend, with whether `drop_dbi` removes the name from `list_dbis` (LMDB only empties it).
fn backends(dir: &std::path::Path) -> Vec<(KvStore, &'static str, bool)> {
    vec![
        (KvStore::open(dir.join("lmdb"), 64 << 20, 16).unwrap(), "lmdb", false),
        (KvStore::open_sqlite(dir.join("s.db")).unwrap(), "sqlite", true),
        (KvStore::open_redb(dir.join("r.redb")).unwrap(), "redb", true),
        (KvStore::open_mem(), "mem", true),
    ]
}

#[test]
fn list_drop_and_stats_across_backends() {
    let dir = tempdir().unwrap();
    for (store, which, name_removed) in backends(dir.path()) {
        let mut w = store.write_txn().unwrap();
        let a = store.open_db(&mut w, "alpha").unwrap();
        let _b = store.open_db(&mut w, "beta").unwrap();
        a.put(&mut w, b"k", b"v").unwrap();
        w.commit().unwrap();

        let names = store.list_dbis().unwrap();
        assert!(names.contains(&"alpha".to_string()), "{which}: {names:?}");
        assert!(names.contains(&"beta".to_string()), "{which}: {names:?}");

        assert!(store.drop_dbi("alpha").unwrap(), "{which}");
        assert!(!store.drop_dbi("nosuch").unwrap(), "{which}");

        let names = store.list_dbis().unwrap();
        assert_eq!(!names.contains(&"alpha".to_string()), name_removed, "{which}: {names:?}");

        // Whatever happens to the name, the dropped DBI holds no data.
        let mut w = store.write_txn().unwrap();
        let a = store.open_db(&mut w, "alpha").unwrap();
        w.commit().unwrap();
        let r = store.read_txn().unwrap();
        assert_eq!(a.len(&r).unwrap(), 0, "{which}");
        assert_eq!(store.entries(&r, &a).unwrap(), 0, "{which}");
    }
}

#[test]
fn copy_produces_a_readable_store_on_lmdb_and_sqlite() {
    let dir = tempdir().unwrap();
    let lmdb = KvStore::open(dir.path().join("src"), 64 << 20, 16).unwrap();
    let sqlite = KvStore::open_sqlite(dir.path().join("src.db")).unwrap();
    for store in [&lmdb, &sqlite] {
        let mut w = store.write_txn().unwrap();
        let db = store.open_db(&mut w, "data").unwrap();
        db.put(&mut w, b"key", b"value").unwrap();
        w.commit().unwrap();
    }

    let copy_dir = dir.path().join("copy");
    std::fs::create_dir_all(&copy_dir).unwrap();
    lmdb.copy_to(&copy_dir.join("data.mdb")).unwrap();
    let copied = KvStore::open(&copy_dir, 64 << 20, 16).unwrap();
    let mut w = copied.write_txn().unwrap();
    let db = copied.open_db(&mut w, "data").unwrap();
    w.commit().unwrap();
    let r = copied.read_txn().unwrap();
    assert_eq!(db.get(&r, b"key").unwrap().as_deref(), Some(&b"value"[..]));
    drop(r);

    let sqlite_copy = dir.path().join("copy.db");
    sqlite.copy_to(&sqlite_copy).unwrap();
    let copied = KvStore::open_sqlite(&sqlite_copy).unwrap();
    let mut w = copied.write_txn().unwrap();
    let db = copied.open_db(&mut w, "data").unwrap();
    w.commit().unwrap();
    let r = copied.read_txn().unwrap();
    assert_eq!(db.get(&r, b"key").unwrap().as_deref(), Some(&b"value"[..]));
}

#[test]
fn copy_is_unsupported_on_redb_and_memory() {
    let dir = tempdir().unwrap();
    let redb = KvStore::open_redb(dir.path().join("r.redb")).unwrap();
    assert!(matches!(redb.copy_to(&dir.path().join("x")), Err(KvError::Unsupported(_))));
    let mem = KvStore::open_mem();
    assert!(matches!(mem.copy_to(&dir.path().join("y")), Err(KvError::Unsupported(_))));
}
