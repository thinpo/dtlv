use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use dtlv_kv::{KvStore, WriteTxn};

fn read_u64(store: &KvStore, db_name: &str, key: &[u8]) -> Option<u64> {
    let rtxn = store.read_txn().unwrap();
    let mut w = store.write_txn().unwrap();
    let db = store.open_db(&mut w, db_name).unwrap();
    let v = db.get_rw(&w, key).unwrap().map(|c| u64::from_be_bytes(c.as_ref().try_into().unwrap()));
    drop(rtxn);
    v
}

#[test]
fn read_txn_does_not_block_on_open_write_txn() {
    let store = Arc::new(KvStore::open_mem());
    let (tx, rx) = mpsc::channel();
    let s = store.clone();
    thread::spawn(move || {
        let mut w = s.write_txn().unwrap();
        let _db = s.open_db(&mut w, "t").unwrap();
        // Previously this blocked forever: the write txn held the data lock.
        let r = s.read_txn().unwrap();
        drop(r);
        drop(w);
        tx.send(()).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("read_txn deadlocked while a write txn was open");
}

#[test]
fn uncommitted_writes_are_invisible_and_committed_ones_visible() {
    let store = KvStore::open_mem();
    {
        let mut w = store.write_txn().unwrap();
        let db = store.open_db(&mut w, "t").unwrap();
        db.put(&mut w, b"k", &1u64.to_be_bytes()).unwrap();
        // Dropped without commit.
    }
    assert_eq!(read_u64(&store, "t", b"k"), None);

    {
        let mut w = store.write_txn().unwrap();
        let db = store.open_db(&mut w, "t").unwrap();
        db.put(&mut w, b"k", &2u64.to_be_bytes()).unwrap();
        w.commit().unwrap();
    }
    assert_eq!(read_u64(&store, "t", b"k"), Some(2));
}

#[test]
fn concurrent_read_modify_write_loses_no_updates() {
    let store = Arc::new(KvStore::open_mem());
    {
        let mut w = store.write_txn().unwrap();
        let db = store.open_db(&mut w, "c").unwrap();
        db.put(&mut w, b"n", &0u64.to_be_bytes()).unwrap();
        w.commit().unwrap();
    }
    let threads = 8;
    let per_thread = 200;
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let s = store.clone();
            thread::spawn(move || {
                for _ in 0..per_thread {
                    // Read-modify-write in one txn: only correct if writers are serialized.
                    let mut w: WriteTxn<'_> = s.write_txn().unwrap();
                    let db = s.open_db(&mut w, "c").unwrap();
                    let cur = db
                        .get_rw(&w, b"n")
                        .unwrap()
                        .map(|c| u64::from_be_bytes(c.as_ref().try_into().unwrap()))
                        .unwrap();
                    db.put(&mut w, b"n", &(cur + 1).to_be_bytes()).unwrap();
                    w.commit().unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(read_u64(&store, "c", b"n"), Some(threads * per_thread));
}
