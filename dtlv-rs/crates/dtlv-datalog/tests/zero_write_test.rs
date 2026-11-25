use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, QueryResult, TxDatum, TxItem, TxReport};
use std::cell::RefCell;
use std::rc::Rc;

fn conn() -> Conn {
    Conn::open_mem(vec![
        AttrSpec::new(":z/many").many().indexed(),
        AttrSpec::new(":z/plain-many").many(),
        AttrSpec::new(":z/one").indexed(),
    ])
    .unwrap()
}

/// Values stored for `attr` on `e`, as written (sign of zero included).
fn stored(conn: &Conn, e: u64, attr: &str) -> Vec<Value> {
    let rtxn = conn.read_txn().unwrap();
    conn.search_datoms(&rtxn, Some(e), Some(attr), None).unwrap().into_iter().map(|d| d.2).collect()
}

fn assert_zero(conn: &Conn, e: u64, attr: &str, x: f64) {
    conn.transact(&[TxItem::Assert(Some(e), vec![(attr.into(), Value::F64(x))])]).unwrap();
}

fn retract_zero(conn: &Conn, e: u64, attr: &str, x: f64) {
    conn.transact(&[TxItem::Retract(e, attr.into(), Value::F64(x))]).unwrap();
}

#[test]
fn asserting_negative_zero_stores_positive_zero() {
    let conn = conn();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":z/one".into(), Value::F64(-0.0))])]).unwrap()[0];
    let vals = stored(&conn, e, ":z/one");
    assert_eq!(vals.len(), 1);
    assert!(matches!(vals[0], Value::F64(x) if x == 0.0 && x.is_sign_positive()), "got {vals:?}");
}

#[test]
fn many_zero_then_negative_zero_is_one_datom() {
    let conn = conn();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":z/many".into(), Value::F64(0.0))])]).unwrap()[0];
    assert_zero(&conn, e, ":z/many", -0.0);
    let vals = stored(&conn, e, ":z/many");
    assert_eq!(vals.len(), 1, "got {vals:?}");
    assert!(matches!(vals[0], Value::F64(x) if x.is_sign_positive()));
}

#[test]
fn many_unindexed_zero_then_negative_zero_is_one_datom() {
    let conn = conn();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":z/plain-many".into(), Value::F64(0.0))])]).unwrap()[0];
    assert_zero(&conn, e, ":z/plain-many", -0.0);
    assert_eq!(stored(&conn, e, ":z/plain-many").len(), 1);
}

#[test]
fn many_retract_negative_zero_removes_positive_zero() {
    let conn = conn();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":z/many".into(), Value::F64(0.0))])]).unwrap()[0];
    retract_zero(&conn, e, ":z/many", -0.0);
    assert!(stored(&conn, e, ":z/many").is_empty());
}

#[test]
fn many_retract_zero_removes_positive_zero() {
    let conn = conn();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":z/many".into(), Value::F64(0.0))])]).unwrap()[0];
    retract_zero(&conn, e, ":z/many", 0.0);
    assert!(stored(&conn, e, ":z/many").is_empty());
}

#[test]
fn one_negative_zero_then_zero_is_one_positive_datom() {
    let conn = conn();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":z/one".into(), Value::F64(-0.0))])]).unwrap()[0];
    assert_zero(&conn, e, ":z/one", 0.0);
    let vals = stored(&conn, e, ":z/one");
    assert_eq!(vals.len(), 1, "got {vals:?}");
    assert!(matches!(vals[0], Value::F64(x) if x.is_sign_positive()));
}

#[test]
fn indexed_many_zero_query_finds_entity_once_then_nothing() {
    let conn = conn();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":z/many".into(), Value::F64(0.0))])]).unwrap()[0];
    assert_zero(&conn, e, ":z/many", -0.0);
    {
        let rtxn = conn.read_txn().unwrap();
        let r = conn.q_str(&rtxn, "[:find ?e :where [?e :z/many 0.0]]", &[]).unwrap();
        assert!(matches!(r, QueryResult::Rel(ref rows) if rows.len() == 1), "got {r:?}");
    }
    retract_zero(&conn, e, ":z/many", -0.0);
    let rtxn = conn.read_txn().unwrap();
    let r = conn.q_str(&rtxn, "[:find ?e :where [?e :z/many 0.0]]", &[]).unwrap();
    assert!(matches!(r, QueryResult::Rel(ref rows) if rows.is_empty()), "got {r:?}");
}

#[test]
fn asserting_negative_zero_over_positive_zero_reports_like_repeated_zero() {
    let mut conn = conn();
    let seen: Rc<RefCell<Vec<TxReport>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = seen.clone();
    conn.listen("log", move |r| sink.borrow_mut().push(r.clone()));
    let e = conn.transact(&[TxItem::Assert(None, vec![(":z/plain-many".into(), Value::F64(0.0))])]).unwrap()[0];
    assert_zero(&conn, e, ":z/plain-many", -0.0);
    assert_zero(&conn, e, ":z/plain-many", 0.0);
    let reports = seen.borrow();
    let added = |r: &TxReport| r.tx_data.clone();
    let expected = vec![TxDatum { eid: e, attr: ":z/plain-many".into(), value: Value::F64(0.0), added: true }];
    // Asserting -0.0 over +0.0 reports the same as asserting +0.0 again.
    assert_eq!(added(&reports[1]), expected);
    assert_eq!(added(&reports[2]), expected);
}
