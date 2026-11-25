use dtlv_kv::KvStore;
use dtlv_search::{Metric, VectorIndex, BRUTE_FORCE_MAX};
use tempfile::tempdir;

/// Deterministic pseudo-random floats in [-1, 1).
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

fn load(store: &KvStore, n: usize, dim: usize, seed: u64) -> (Vec<Vec<f32>>, VectorIndex) {
    let mut rng = Lcg(seed);
    let data: Vec<Vec<f32>> = (0..n).map(|_| rng.vec(dim)).collect();
    let mut wtxn = store.write_txn().unwrap();
    let idx = VectorIndex::open(store, &mut wtxn, "vecs", Metric::Euclidean).unwrap();
    for (i, v) in data.iter().enumerate() {
        idx.add_vec(&mut wtxn, (i + 1) as u64, v).unwrap();
    }
    wtxn.commit().unwrap();
    (data, idx)
}

#[test]
fn graph_search_has_high_recall_against_exact_neighbors() {
    let dir = tempdir().unwrap();
    let store = KvStore::open(dir.path(), 256 * 1024 * 1024, 8).unwrap();
    let n = 3000;
    assert!(n as u64 > BRUTE_FORCE_MAX, "test must exercise the graph path");
    let (data, idx) = load(&store, n, 16, 7);

    let rtxn = store.read_txn().unwrap();
    let mut rng = Lcg(99);
    let k = 10;
    let mut found = 0;
    let queries = 100;
    for _ in 0..queries {
        let q = rng.vec(16);
        let mut exact: Vec<(f32, u64)> = data.iter().enumerate().map(|(i, v)| (euclid(&q, v), (i + 1) as u64)).collect();
        exact.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let truth: Vec<u64> = exact.iter().take(k).map(|x| x.1).collect();
        let got: Vec<u64> = idx.search_vec(&rtxn, &q, k).unwrap().into_iter().map(|x| x.0).collect();
        assert_eq!(got.len(), k);
        found += got.iter().filter(|e| truth.contains(e)).count();
    }
    let recall = found as f64 / (queries * k) as f64;
    assert!(recall >= 0.9, "recall {recall:.3} below 0.9");
}

#[test]
fn small_index_is_searched_exactly() {
    let dir = tempdir().unwrap();
    let store = KvStore::open(dir.path(), 64 * 1024 * 1024, 8).unwrap();
    let (data, idx) = load(&store, 100, 4, 3);
    let rtxn = store.read_txn().unwrap();
    let q = vec![0.1, -0.2, 0.3, 0.0];
    let mut exact: Vec<(f32, u64)> = data.iter().enumerate().map(|(i, v)| (euclid(&q, v), (i + 1) as u64)).collect();
    exact.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let got = idx.search_vec(&rtxn, &q, 5).unwrap();
    let want: Vec<u64> = exact.iter().take(5).map(|x| x.1).collect();
    assert_eq!(got.iter().map(|x| x.0).collect::<Vec<_>>(), want);
}

#[test]
fn writes_after_a_graph_search_are_visible_to_the_next_search() {
    let dir = tempdir().unwrap();
    let store = KvStore::open(dir.path(), 256 * 1024 * 1024, 8).unwrap();
    let (_, idx) = load(&store, 1000, 8, 11);
    let q = vec![0.5; 8];
    {
        let rtxn = store.read_txn().unwrap();
        idx.search_vec(&rtxn, &q, 3).unwrap(); // builds the cached graph
    }

    // A new vector exactly at the query must now be the nearest hit.
    let mut wtxn = store.write_txn().unwrap();
    idx.add_vec(&mut wtxn, 50_000, &q).unwrap();
    wtxn.commit().unwrap();
    {
        let rtxn = store.read_txn().unwrap();
        assert_eq!(idx.search_vec(&rtxn, &q, 1).unwrap()[0].0, 50_000);
    }

    // Removing it makes it disappear from the next search.
    let mut wtxn = store.write_txn().unwrap();
    idx.remove_vec(&mut wtxn, 50_000).unwrap();
    wtxn.commit().unwrap();
    let rtxn = store.read_txn().unwrap();
    assert!(idx.search_vec(&rtxn, &q, 5).unwrap().iter().all(|x| x.0 != 50_000));
}
