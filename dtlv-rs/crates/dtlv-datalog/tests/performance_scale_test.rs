//! Scale guards for the rules engine, multi-way joins, and the vector index. Bounds are generous for debug
//! builds; they catch complexity regressions. Timings print with `--nocapture`.

use std::time::{Duration, Instant};

use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, QueryResult, TxItem};

fn report(label: &str, elapsed: Duration, bound: Duration) {
    println!("{label}: {elapsed:?} (bound {bound:?})");
    assert!(elapsed < bound, "{label} took {elapsed:?}, over {bound:?}");
}

#[test]
fn transitive_closure_over_a_long_chain() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":e/to").many(), AttrSpec::new(":e/id").unique_identity()]).unwrap();
    let n = 200i64;
    let items: Vec<TxItem> = (0..n)
        .map(|i| {
            TxItem::Assert(
                None,
                vec![(":e/id".into(), Value::I64(i)), (":e/to".into(), Value::I64(i + 1))],
            )
        })
        .collect();
    conn.transact(&items).unwrap();
    let rules = r#"[
        [(reach ?a ?b) [?e :e/id ?a] [?e :e/to ?b]]
        [(reach ?a ?c) [?e :e/id ?a] [?e :e/to ?b] (reach ?b ?c)]
    ]"#;
    let rtxn = conn.read_txn().unwrap();
    let t = Instant::now();
    let r = conn.q_str_with_rules(&rtxn, "[:find (count ?b) . :where (reach 0 ?b)]", &[], rules).unwrap();
    report("transitive closure, 200-node chain from one source", t.elapsed(), Duration::from_secs(60));
    // Node 0 reaches nodes 1..=200 (201 is not stored as an entity, but it is a target).
    assert_eq!(r, QueryResult::Scalar(Some(Value::I64(n))));
}

#[test]
fn three_way_join_over_thousands_of_entities() {
    let conn = Conn::open_mem(vec![
        AttrSpec::new(":o/cust").indexed(),
        AttrSpec::new(":o/prod").indexed(),
        AttrSpec::new(":p/name").unique_identity(),
        AttrSpec::new(":c/name").unique_identity(),
    ])
    .unwrap();
    let mut items = Vec::new();
    for p in 0..200i64 {
        items.push(TxItem::Assert(None, vec![(":p/name".into(), Value::Str(format!("p{p}")))]));
    }
    for c in 0..200i64 {
        items.push(TxItem::Assert(None, vec![(":c/name".into(), Value::Str(format!("c{c}")))]));
    }
    for o in 0..5_000i64 {
        items.push(TxItem::Assert(
            None,
            vec![(":o/cust".into(), Value::I64(o % 200)), (":o/prod".into(), Value::I64((o * 7) % 200))],
        ));
    }
    conn.transact(&items).unwrap();
    let rtxn = conn.read_txn().unwrap();
    // Selective constant on the customer side should drive the join, not a scan of all orders first.
    let q = r#"[:find (count ?o) . :where [?o :o/cust 5] [?o :o/prod ?p] [?pe :p/name ?n]]"#;
    let t = Instant::now();
    let r = conn.q_str(&rtxn, q, &[]).unwrap();
    report("three-way join, selective first clause", t.elapsed(), Duration::from_secs(10));
    assert_eq!(r, QueryResult::Scalar(Some(Value::I64(25))));
    let steps = conn.analyze(&rtxn, q, &[]).unwrap();
    assert_eq!(steps[0].rows, 25, "the first step should bind only the 25 matching orders: {steps:?}");
}

#[test]
fn vector_search_at_scale_with_interleaved_writes() {
    use dtlv_kv::KvStore;
    use dtlv_search::{Metric, VectorIndex, VectorStats};
    let dir = tempfile::tempdir().unwrap();
    let store = KvStore::open(dir.path(), 512 * 1024 * 1024, 8).unwrap();
    let mut w = store.write_txn().unwrap();
    let idx = VectorIndex::open(&store, &mut w, "v", Metric::Euclidean).unwrap();
    w.commit().unwrap();

    let dim = 16;
    let mut seed = 99u64;
    let mut rnd = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    };
    let t = Instant::now();
    let mut w = store.write_txn().unwrap();
    for e in 0..20_000u64 {
        let v: Vec<f32> = (0..dim).map(|_| rnd()).collect();
        idx.add_vec(&mut w, e, &v).unwrap();
    }
    w.commit().unwrap();
    report("load 20k vectors", t.elapsed(), Duration::from_secs(120));

    let t = Instant::now();
    let q: Vec<f32> = (0..dim).map(|_| rnd()).collect();
    {
        let rtxn = store.read_txn().unwrap();
        idx.search_vec(&rtxn, &q, 10).unwrap();
    }
    report("first graph search (build)", t.elapsed(), Duration::from_secs(120));

    // Interleave small writes and searches; each search should replay, not rebuild.
    let t = Instant::now();
    for round in 0..50u64 {
        let mut w = store.write_txn().unwrap();
        for j in 0..4u64 {
            let v: Vec<f32> = (0..dim).map(|_| rnd()).collect();
            idx.add_vec(&mut w, 100_000 + round * 4 + j, &v).unwrap();
        }
        w.commit().unwrap();
        let rtxn = store.read_txn().unwrap();
        let qq: Vec<f32> = (0..dim).map(|_| rnd()).collect();
        assert_eq!(idx.search_vec(&rtxn, &qq, 10).unwrap().len(), 10);
    }
    report("50 rounds of 4 writes + search", t.elapsed(), Duration::from_secs(120));
    let stats: VectorStats = idx.stats();
    assert_eq!(stats.rebuilds, 1, "interleaved small writes should not rebuild: {stats:?}");
    assert!(stats.incremental_updates >= 50, "{stats:?}");
}
