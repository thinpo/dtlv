//! Indexed lookups per binding: bindings that share a bound key share one index scan per clause.
//! Timings print with `--nocapture`.

use std::time::{Duration, Instant};

use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, QueryResult, TxItem};

#[test]
fn bindings_sharing_an_indexed_key_join_correctly_and_quickly() {
    // 2,000 outer entities in 20 groups (attribute not indexed, scanned once). 2,000 inner entities with an
    // indexed group attribute, 100 per group. Each outer binding probes its group: 2,000 x 100 = 200,000 pairs.
    let conn = Conn::open_mem(vec![AttrSpec::new(":o/g"), AttrSpec::new(":i/g").indexed()]).unwrap();
    let mut items = Vec::new();
    for n in 0..2_000i64 {
        items.push(TxItem::Assert(None, vec![(":o/g".into(), Value::I64(n % 20))]));
        items.push(TxItem::Assert(None, vec![(":i/g".into(), Value::I64(n % 20))]));
    }
    conn.transact(&items).unwrap();
    let rtxn = conn.read_txn().unwrap();
    let t = Instant::now();
    let r = conn
        .q_str(&rtxn, "[:find (count ?i) . :with ?o :where [?o :o/g ?g] [?i :i/g ?g]]", &[])
        .unwrap();
    let elapsed = t.elapsed();
    println!("indexed join, 2k outer x 100-per-group inner: {elapsed:?}");
    assert_eq!(r, QueryResult::Scalar(Some(Value::I64(200_000))));
    assert!(elapsed < Duration::from_secs(60), "took {elapsed:?}");
}

#[test]
fn entity_bound_lookups_memoize_per_entity() {
    // Many bindings for the same entity: its attribute values are read once.
    let conn = Conn::open_mem(vec![AttrSpec::new(":e/v").many(), AttrSpec::new(":o/e")]).unwrap();
    let e = conn
        .transact(&[TxItem::Assert(None, vec![(":e/v".into(), Value::I64(1)), (":e/v".into(), Value::I64(2))])])
        .unwrap()[0];
    let items: Vec<TxItem> = (0..500).map(|_| TxItem::Assert(None, vec![(":o/e".into(), Value::Ref(e))])).collect();
    conn.transact(&items).unwrap();
    let rtxn = conn.read_txn().unwrap();
    let r = conn.q_str(&rtxn, "[:find (count ?v) . :with ?o :where [?o :o/e ?x] [?x :e/v ?v]]", &[]).unwrap();
    assert_eq!(r, QueryResult::Scalar(Some(Value::I64(1000))));
}
