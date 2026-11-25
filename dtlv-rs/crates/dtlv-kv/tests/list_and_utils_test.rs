use std::ops::Bound;

use dtlv_kv::{KvStore, ListDb};
use tempfile::tempdir;

/// Runs `f` against each backend in turn, so every behavior is checked on LMDB, SQLite, redb, and memory.
fn each_backend(f: impl Fn(&KvStore, &str)) {
    let dir = tempdir().unwrap();
    f(&KvStore::open(dir.path().join("lmdb"), 64 << 20, 16).unwrap(), "lmdb");
    f(&KvStore::open_sqlite(dir.path().join("s.db")).unwrap(), "sqlite");
    f(&KvStore::open_redb(dir.path().join("r.redb")).unwrap(), "redb");
    f(&KvStore::open_mem(), "mem");
}

#[test]
fn list_items_are_sorted_per_key_and_isolated_across_keys() {
    each_backend(|store, which| {
        let mut w = store.write_txn().unwrap();
        let list = ListDb::open(store, &mut w, "lists").unwrap();
        // "a" and "ab" are prefix-related keys; the escaping must keep their items apart.
        list.put_item(&mut w, b"a", b"z").unwrap();
        list.put_item(&mut w, b"a", b"b").unwrap();
        list.put_item(&mut w, b"ab", b"x").unwrap();
        list.put_item(&mut w, b"a", b"b").unwrap(); // duplicate item is a no-op
        w.commit().unwrap();

        let r = store.read_txn().unwrap();
        assert_eq!(list.values(&r, b"a").unwrap(), vec![b"b".to_vec(), b"z".to_vec()], "{which}");
        assert_eq!(list.values(&r, b"ab").unwrap(), vec![b"x".to_vec()], "{which}");
        assert_eq!(list.count(&r, b"a").unwrap(), 2, "{which}");
        assert_eq!(list.count(&r, b"nope").unwrap(), 0, "{which}");
        assert_eq!(list.all(&r).unwrap().len(), 2, "{which}");
    });
}

#[test]
fn list_delete_item_and_clear_key() {
    each_backend(|store, which| {
        let mut w = store.write_txn().unwrap();
        let list = ListDb::open(store, &mut w, "lists").unwrap();
        for v in [b"1", b"2", b"3"] {
            list.put_item(&mut w, b"k", v).unwrap();
        }
        list.put_item(&mut w, b"other", b"9").unwrap();
        assert!(list.del_item(&mut w, b"k", b"2").unwrap(), "{which}");
        assert!(!list.del_item(&mut w, b"k", b"2").unwrap(), "{which}");
        assert_eq!(list.clear_key(&mut w, b"k").unwrap(), 2, "{which}");
        w.commit().unwrap();

        let r = store.read_txn().unwrap();
        assert_eq!(list.count(&r, b"k").unwrap(), 0, "{which}");
        assert_eq!(list.values(&r, b"other").unwrap(), vec![b"9".to_vec()], "{which}");
    });
}

#[test]
fn rank_by_rank_range_count_first_n_and_clear() {
    each_backend(|store, which| {
        let mut w = store.write_txn().unwrap();
        let db = store.open_db(&mut w, "ordered").unwrap();
        for k in [b"b", b"d", b"a", b"c", b"e"] {
            db.put(&mut w, k, b"v").unwrap();
        }
        w.commit().unwrap();

        let r = store.read_txn().unwrap();
        assert_eq!(db.rank(&r, b"c").unwrap(), 2, "{which}");
        assert_eq!(db.rank(&r, b"a").unwrap(), 0, "{which}");
        assert_eq!(db.by_rank(&r, 3).unwrap().unwrap().0, b"d".to_vec(), "{which}");
        assert_eq!(db.by_rank(&r, 9).unwrap(), None, "{which}");
        assert_eq!(
            db.key_range_count(&r, Bound::Included(b"b"), Bound::Excluded(b"e")).unwrap(),
            3,
            "{which}"
        );
        let first: Vec<Vec<u8>> = db.first_n(&r, 2).unwrap().into_iter().map(|(k, _)| k).collect();
        assert_eq!(first, vec![b"a".to_vec(), b"b".to_vec()], "{which}");
        drop(r);

        let mut w = store.write_txn().unwrap();
        assert_eq!(db.clear(&mut w).unwrap(), 5, "{which}");
        w.commit().unwrap();
        let r = store.read_txn().unwrap();
        assert_eq!(db.len(&r).unwrap(), 0, "{which}");
    });
}
