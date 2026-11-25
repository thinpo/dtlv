//! Join evaluation: correctness and cost of joining unindexed attributes on a shared variable.
//! Timings print with `--nocapture`.

use std::time::{Duration, Instant};

use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, QueryResult, TxItem};

fn build(n: i64) -> Conn {
    // Neither attribute is indexed, so each pattern without a bound entity is a scan of its attribute.
    let conn = Conn::open_mem(vec![AttrSpec::new(":a/x"), AttrSpec::new(":b/x")]).unwrap();
    let mut items = Vec::new();
    for i in 0..n {
        items.push(TxItem::Assert(None, vec![(":a/x".into(), Value::I64(i))]));
        items.push(TxItem::Assert(None, vec![(":b/x".into(), Value::I64(i * 2))]));
    }
    conn.transact(&items).unwrap();
    conn
}

#[test]
fn unindexed_join_returns_the_right_rows() {
    let conn = build(50);
    let rtxn = conn.read_txn().unwrap();
    // Values 0..50 on the left, even values 0..98 on the right: the shared values are the even numbers below 50.
    let r = conn.q_str(&rtxn, "[:find (count ?v) . :where [?a :a/x ?v] [?b :b/x ?v]]", &[]).unwrap();
    assert_eq!(r, QueryResult::Scalar(Some(Value::I64(25))));
}

#[test]
fn unindexed_join_over_two_thousand_rows_stays_fast() {
    let conn = build(2_000);
    let rtxn = conn.read_txn().unwrap();
    let t = Instant::now();
    let r = conn.q_str(&rtxn, "[:find (count ?v) . :where [?a :a/x ?v] [?b :b/x ?v]]", &[]).unwrap();
    let elapsed = t.elapsed();
    println!("unindexed join, 2k x 2k: {elapsed:?}");
    assert_eq!(r, QueryResult::Scalar(Some(Value::I64(1_000))));
    assert!(elapsed < Duration::from_secs(60), "took {elapsed:?}");
}

#[test]
fn hash_probe_handles_duplicate_values_on_both_sides() {
    // Value 7 occurs twice on each side: the join must produce all four pairs.
    let conn = Conn::open_mem(vec![AttrSpec::new(":l/x"), AttrSpec::new(":r/x"), AttrSpec::new(":l/n"), AttrSpec::new(":r/n")]).unwrap();
    conn.transact(&[
        TxItem::Assert(None, vec![(":l/x".into(), Value::I64(7)), (":l/n".into(), Value::I64(1))]),
        TxItem::Assert(None, vec![(":l/x".into(), Value::I64(7)), (":l/n".into(), Value::I64(2))]),
        TxItem::Assert(None, vec![(":l/x".into(), Value::I64(8)), (":l/n".into(), Value::I64(3))]),
        TxItem::Assert(None, vec![(":r/x".into(), Value::I64(7)), (":r/n".into(), Value::I64(10))]),
        TxItem::Assert(None, vec![(":r/x".into(), Value::I64(7)), (":r/n".into(), Value::I64(20))]),
    ])
    .unwrap();
    let rtxn = conn.read_txn().unwrap();
    // Count pairs of entities: keep ?b in the set so each pair counts once.
    let r = conn.q_str(&rtxn, "[:find (count ?a) . :with ?b :where [?a :l/x ?v] [?a :l/n ?ln] [?b :r/x ?v] [?b :r/n ?rn]]", &[]).unwrap();
    assert_eq!(r, QueryResult::Scalar(Some(Value::I64(4))));
}

#[test]
fn hash_probe_respects_bindings_already_in_the_environment() {
    // A: left value 5. B: left and right value 5. C: right value 5 only.
    // ?b must have both attributes at the same value, so ?b is B; then ?a is any left entity with value 5: A or B.
    let conn = Conn::open_mem(vec![AttrSpec::new(":l/x"), AttrSpec::new(":r/x")]).unwrap();
    let a = conn.transact(&[TxItem::Assert(None, vec![(":l/x".into(), Value::I64(5))])]).unwrap()[0];
    let b = conn
        .transact(&[TxItem::Assert(None, vec![(":l/x".into(), Value::I64(5)), (":r/x".into(), Value::I64(5))])])
        .unwrap()[0];
    conn.transact(&[TxItem::Assert(None, vec![(":r/x".into(), Value::I64(5))])]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    let r = match conn.q_str(&rtxn, "[:find [?a ...] :where [?a :l/x ?v] [?b :r/x ?v] [?b :l/x ?v]]", &[]).unwrap() {
        QueryResult::Coll(v) => {
            let mut v: Vec<Value> = v;
            v.sort_by_key(|x| format!("{x:?}"));
            v
        }
        other => panic!("{other:?}"),
    };
    let mut expected = vec![Value::Ref(a), Value::Ref(b)];
    expected.sort_by_key(|x| format!("{x:?}"));
    assert_eq!(r, expected);
}

#[test]
fn hash_probe_scales_past_quadratic_cost() {
    use std::time::{Duration, Instant};
    let conn = Conn::open_mem(vec![AttrSpec::new(":l/x"), AttrSpec::new(":r/x")]).unwrap();
    let mut items = Vec::new();
    for i in 0..20_000i64 {
        items.push(TxItem::Assert(None, vec![(":l/x".into(), Value::I64(i))]));
        items.push(TxItem::Assert(None, vec![(":r/x".into(), Value::I64(i))]));
    }
    conn.transact(&items).unwrap();
    let rtxn = conn.read_txn().unwrap();
    let t = Instant::now();
    let r = conn.q_str(&rtxn, "[:find (count ?v) . :where [?a :l/x ?v] [?b :r/x ?v]]", &[]).unwrap();
    let elapsed = t.elapsed();
    println!("unindexed join, 20k x 20k: {elapsed:?}");
    assert_eq!(r, QueryResult::Scalar(Some(Value::I64(20_000))));
    // 400 million comparisons without hashing; a hash probe is about 20 thousand lookups.
    assert!(elapsed < Duration::from_secs(60), "took {elapsed:?}");
}
