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

fn euclid(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>().sqrt()
}

fn add_all(store: &KvStore, idx: &VectorIndex, items: &[(u64, Vec<f32>)]) {
    let mut w = store.write_txn().unwrap();
    for (eid, v) in items {
        idx.add_vec(&mut w, *eid, v).unwrap();
    }
    w.commit().unwrap();
}

fn remove(store: &KvStore, idx: &VectorIndex, eid: u64) {
    let mut w = store.write_txn().unwrap();
    idx.remove_vec(&mut w, eid).unwrap();
    w.commit().unwrap();
}

/// Top-k entity ids by brute force over the live set.
fn exact_top(live: &std::collections::BTreeMap<u64, Vec<f32>>, q: &[f32], k: usize) -> Vec<u64> {
    let mut all: Vec<(f32, u64)> = live.iter().map(|(e, v)| (euclid(q, v), *e)).collect();
    all.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    all.into_iter().take(k).map(|x| x.1).collect()
}

fn recall(store: &KvStore, idx: &VectorIndex, live: &std::collections::BTreeMap<u64, Vec<f32>>, queries: usize) -> f64 {
    let rtxn = store.read_txn().unwrap();
    let mut rng = Lcg(4242);
    let k = 10;
    let mut found = 0;
    for _ in 0..queries {
        let q = rng.vec(8);
        let truth = exact_top(live, &q, k);
        let got: Vec<u64> = idx.search_vec(&rtxn, &q, k).unwrap().into_iter().map(|x| x.0).collect();
        found += got.iter().filter(|e| truth.contains(e)).count();
    }
    found as f64 / (queries * k) as f64
}

#[test]
fn small_writes_after_a_search_are_replayed_without_rebuilding() {
    let dir = tempdir().unwrap();
    let store = KvStore::open(dir.path(), 256 * 1024 * 1024, 8).unwrap();
    let mut w = store.write_txn().unwrap();
    let idx = VectorIndex::open(&store, &mut w, "v", Metric::Euclidean).unwrap();
    w.commit().unwrap();

    let mut rng = Lcg(7);
    let mut live = std::collections::BTreeMap::new();
    let initial: Vec<(u64, Vec<f32>)> = (1..=1500u64).map(|e| (e, rng.vec(8))).collect();
    add_all(&store, &idx, &initial);
    live.extend(initial.iter().cloned());

    // First search builds the graph once.
    {
        let rtxn = store.read_txn().unwrap();
        idx.search_vec(&rtxn, &rng.vec(8), 5).unwrap();
    }
    let before = idx.stats();
    assert_eq!(before.rebuilds, 1);

    // Ten small batches, each followed by a search: all should replay incrementally.
    for batch in 0..10u64 {
        let items: Vec<(u64, Vec<f32>)> = (0..5).map(|i| (10_000 + batch * 5 + i, rng.vec(8))).collect();
        add_all(&store, &idx, &items);
        live.extend(items.iter().cloned());
        let rtxn = store.read_txn().unwrap();
        idx.search_vec(&rtxn, &rng.vec(8), 5).unwrap();
    }
    let after = idx.stats();
    assert_eq!(after.rebuilds, 1, "small batches should not trigger a rebuild");
    assert!(after.incremental_updates >= before.incremental_updates + 10);
    assert!(recall(&store, &idx, &live, 40) >= 0.9);
}

#[test]
fn removed_and_replaced_vectors_never_come_back() {
    let dir = tempdir().unwrap();
    let store = KvStore::open(dir.path(), 256 * 1024 * 1024, 8).unwrap();
    let mut w = store.write_txn().unwrap();
    let idx = VectorIndex::open(&store, &mut w, "v", Metric::Euclidean).unwrap();
    w.commit().unwrap();

    let mut rng = Lcg(11);
    let mut live = std::collections::BTreeMap::new();
    let initial: Vec<(u64, Vec<f32>)> = (1..=1000u64).map(|e| (e, rng.vec(8))).collect();
    add_all(&store, &idx, &initial);
    live.extend(initial.iter().cloned());
    {
        let rtxn = store.read_txn().unwrap();
        idx.search_vec(&rtxn, &rng.vec(8), 5).unwrap();
    }

    // Remove 1..=100, and move entity 500 to a point at the origin.
    for e in 1..=100u64 {
        remove(&store, &idx, e);
        live.remove(&e);
    }
    let moved = vec![0.0; 8];
    add_all(&store, &idx, &[(500, moved.clone())]);
    live.insert(500, moved.clone());

    let rtxn = store.read_txn().unwrap();
    let hits: Vec<u64> = idx.search_vec(&rtxn, &moved, 5).unwrap().into_iter().map(|x| x.0).collect();
    drop(rtxn);
    assert_eq!(hits[0], 500, "the replaced vector should be the nearest hit at its new position");
    for e in 1..=100u64 {
        assert!(!hits.contains(&e), "removed entity {e} reappeared");
    }
    assert!(recall(&store, &idx, &live, 30) >= 0.9);
    assert_eq!(idx.stats().rebuilds, 1, "removals and replacements should be replayed, not rebuilt");
}

#[test]
fn removing_an_absent_entity_does_not_change_the_index() {
    let dir = tempdir().unwrap();
    let store = KvStore::open(dir.path(), 64 * 1024 * 1024, 8).unwrap();
    let mut w = store.write_txn().unwrap();
    let idx = VectorIndex::open(&store, &mut w, "v", Metric::Euclidean).unwrap();
    w.commit().unwrap();
    add_all(&store, &idx, &[(1, vec![0.0; 8])]);
    remove(&store, &idx, 99);
    let rtxn = store.read_txn().unwrap();
    assert_eq!(idx.search_vec(&rtxn, &[0.0; 8], 1).unwrap()[0].0, 1);
}
