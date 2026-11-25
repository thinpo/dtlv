use dtlv_kv::KvStore;
use dtlv_search::{Metric, VectorIndex};
use tempfile::tempdir;

struct Lcg(u64);

impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
    fn vec(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.next_f32()).collect()
    }
}

const MAP_SIZE: usize = 256 * 1024 * 1024;

fn open(path: &std::path::Path) -> (KvStore, VectorIndex) {
    let store = KvStore::open(path, MAP_SIZE, 8).unwrap();
    let mut w = store.write_txn().unwrap();
    let idx = VectorIndex::open(&store, &mut w, "v", Metric::Euclidean).unwrap();
    w.commit().unwrap();
    (store, idx)
}

fn add_all(store: &KvStore, idx: &VectorIndex, items: &[(u64, Vec<f32>)]) {
    let mut w = store.write_txn().unwrap();
    for (eid, v) in items {
        idx.add_vec(&mut w, *eid, v).unwrap();
    }
    w.commit().unwrap();
}

fn search(store: &KvStore, idx: &VectorIndex, q: &[f32], k: usize) -> Vec<(u64, f32)> {
    let rtxn = store.read_txn().unwrap();
    idx.search_vec(&rtxn, q, k).unwrap()
}

fn initial_items(seed: u64) -> Vec<(u64, Vec<f32>)> {
    let mut rng = Lcg(seed);
    (1..=1000u64).map(|e| (e, rng.vec(8))).collect()
}

#[test]
fn saved_graph_is_loaded_after_restart_without_rebuild() {
    let dir = tempdir().unwrap();
    let query = Lcg(5).vec(8);
    let before;
    {
        let (store, idx) = open(dir.path());
        add_all(&store, &idx, &initial_items(1));
        before = search(&store, &idx, &query, 10);
        assert_eq!(idx.stats().rebuilds, 1);
        let mut w = store.write_txn().unwrap();
        assert!(idx.save_graph(&mut w).unwrap());
        w.commit().unwrap();
    }
    let (store, idx) = open(dir.path());
    let after = search(&store, &idx, &query, 10);
    assert_eq!(idx.stats().loads, 1);
    assert_eq!(idx.stats().rebuilds, 0);
    assert_eq!(after, before);
}

#[test]
fn loaded_graph_catches_up_with_writes_made_after_restart() {
    let dir = tempdir().unwrap();
    {
        let (store, idx) = open(dir.path());
        add_all(&store, &idx, &initial_items(2));
        search(&store, &idx, &[0.0; 8], 5);
        let mut w = store.write_txn().unwrap();
        assert!(idx.save_graph(&mut w).unwrap());
        w.commit().unwrap();
    }
    let (store, idx) = open(dir.path());
    let mut rng = Lcg(77);
    let new_items: Vec<(u64, Vec<f32>)> = (0..5u64).map(|i| (10_000 + i, rng.vec(8))).collect();
    add_all(&store, &idx, &new_items);
    search(&store, &idx, &[0.0; 8], 5);
    assert_eq!(idx.stats().loads, 1);
    assert_eq!(idx.stats().rebuilds, 0);
    for (eid, v) in &new_items {
        assert_eq!(search(&store, &idx, v, 1)[0].0, *eid);
    }
}

#[test]
fn save_graph_without_a_cached_graph_returns_false() {
    let dir = tempdir().unwrap();
    let (store, idx) = open(dir.path());
    add_all(&store, &idx, &initial_items(3));
    let mut w = store.write_txn().unwrap();
    assert!(!idx.save_graph(&mut w).unwrap());
    w.commit().unwrap();
}

#[test]
fn corrupt_saved_graph_falls_back_to_rebuild() {
    let dir = tempdir().unwrap();
    let query = Lcg(6).vec(8);
    let before;
    {
        let (store, idx) = open(dir.path());
        add_all(&store, &idx, &initial_items(4));
        before = search(&store, &idx, &query, 10);
        let mut w = store.write_txn().unwrap();
        assert!(idx.save_graph(&mut w).unwrap());
        let meta = store.open_db(&mut w, "v__meta").unwrap();
        meta.put(&mut w, b"graph", b"junk").unwrap();
        w.commit().unwrap();
    }
    let (store, idx) = open(dir.path());
    let after = search(&store, &idx, &query, 10);
    assert_eq!(idx.stats().loads, 0);
    assert_eq!(idx.stats().rebuilds, 1);
    assert_eq!(after, before);
}

#[test]
fn first_write_after_rebuild_saves_graph_automatically() {
    let dir = tempdir().unwrap();
    let query = Lcg(9).vec(8);
    let mut rng = Lcg(10);
    let added: Vec<(u64, Vec<f32>)> = (0..2u64).map(|i| (20_000 + i, rng.vec(8))).collect();
    {
        let (store, idx) = open(dir.path());
        add_all(&store, &idx, &initial_items(5));
        search(&store, &idx, &query, 10);
        assert_eq!(idx.stats().rebuilds, 1);
        assert_eq!(idx.stats().saves, 0);
        add_all(&store, &idx, &added[..1]);
        assert_eq!(idx.stats().saves, 1);
        add_all(&store, &idx, &added[1..]);
        assert_eq!(idx.stats().saves, 1);
    }
    let (store, idx) = open(dir.path());
    search(&store, &idx, &query, 10);
    assert_eq!(idx.stats().loads, 1);
    assert_eq!(idx.stats().rebuilds, 0);
    for (eid, v) in &added {
        assert_eq!(search(&store, &idx, v, 1)[0].0, *eid);
    }
}

#[test]
fn remove_of_absent_entity_after_rebuild_does_not_save() {
    let dir = tempdir().unwrap();
    let (store, idx) = open(dir.path());
    add_all(&store, &idx, &initial_items(6));
    search(&store, &idx, &[0.0; 8], 5);
    assert_eq!(idx.stats().rebuilds, 1);
    let mut w = store.write_txn().unwrap();
    idx.remove_vec(&mut w, 99_999).unwrap();
    w.commit().unwrap();
    assert_eq!(idx.stats().saves, 0);
}

#[test]
fn writes_without_a_search_never_save() {
    let dir = tempdir().unwrap();
    let (store, idx) = open(dir.path());
    add_all(&store, &idx, &initial_items(7));
    assert_eq!(idx.stats().rebuilds, 0);
    assert_eq!(idx.stats().saves, 0);
}
