use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, QueryResult, TxItem};

#[test]
fn attribute_index_treats_negative_and_positive_zero_as_one_value() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":f/x").indexed()]).unwrap();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":f/x".into(), Value::F64(-0.0))])]).unwrap()[0];
    let rtxn = conn.read_txn().unwrap();
    // Lookup with +0.0 finds the entity stored as -0.0, as Value equality says it should.
    assert_eq!(conn.lookup_entity_by_av(&rtxn, ":f/x", &Value::F64(0.0)).unwrap(), Some(e));
    assert_eq!(conn.lookup_entity_by_av(&rtxn, ":f/x", &Value::F64(-0.0)).unwrap(), Some(e));
    // Range queries cover both spellings too.
    let r = conn.index_range(&rtxn, ":f/x", &Value::F64(0.0), &Value::F64(0.0)).unwrap();
    assert_eq!(r.len(), 1);
}

#[test]
fn query_on_an_indexed_zero_matches_either_sign() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":f/x").indexed()]).unwrap();
    conn.transact(&[TxItem::Assert(None, vec![(":f/x".into(), Value::F64(-0.0))])]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    let r = conn.q_str(&rtxn, "[:find ?e :where [?e :f/x 0.0]]", &[]).unwrap();
    assert!(matches!(r, QueryResult::Rel(ref rows) if rows.len() == 1), "got {r:?}");
}

#[test]
fn aggregates_count_negative_and_positive_zero_once() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":f/x"), AttrSpec::new(":f/k")]).unwrap();
    conn.transact(&[
        TxItem::Assert(None, vec![(":f/x".into(), Value::F64(-0.0)), (":f/k".into(), Value::I64(1))]),
        TxItem::Assert(None, vec![(":f/x".into(), Value::F64(0.0)), (":f/k".into(), Value::I64(2))]),
    ])
    .unwrap();
    let rtxn = conn.read_txn().unwrap();
    // Set semantics over the find variable: -0.0 and 0.0 are one value, so one distinct value.
    let r = conn.q_str(&rtxn, "[:find (count ?x) . :where [?e :f/k ?k] [?e :f/x ?x]]", &[]).unwrap();
    assert_eq!(r, QueryResult::Scalar(Some(Value::I64(1))));
}
