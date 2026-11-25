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
