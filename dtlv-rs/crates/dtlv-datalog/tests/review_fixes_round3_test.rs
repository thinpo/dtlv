use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, QueryResult, TxItem};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn one(conn: &Conn, q: &str) -> QueryResult {
    let rtxn = conn.read_txn().unwrap();
    conn.q_str(&rtxn, q, &[]).unwrap()
}

#[test]
fn aggregates_count_distinct_bindings_by_default() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":p/name"), AttrSpec::new(":p/g")]).unwrap();
    conn.transact(&[
        TxItem::Assert(None, vec![(":p/name".into(), s("Ann"))]),
        TxItem::Assert(None, vec![(":p/name".into(), s("Ann"))]),
    ])
    .unwrap();
    // Two entities share the name: the default set semantics count the name once.
    assert_eq!(one(&conn, "[:find (count ?n) . :where [?e :p/name ?n]]"), QueryResult::Scalar(Some(Value::I64(1))));
    // :with keeps the entity in the aggregated set, so both are counted.
    assert_eq!(
        one(&conn, "{:find [(count ?n) ...] :with [?e] :where [[?e :p/name ?n]]}"),
        QueryResult::Coll(vec![Value::I64(2)])
    );
}

#[test]
fn duplicate_derivations_do_not_inflate_counts() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":p/name")]).unwrap();
    conn.transact(&[TxItem::Assert(None, vec![(":p/name".into(), s("Ann"))])]).unwrap();
    // Both branches derive the same binding for ?n.
    let q = "[:find (count ?n) . :where (or [?e :p/name ?n] [?e :p/name ?n])]";
    assert_eq!(one(&conn, q), QueryResult::Scalar(Some(Value::I64(1))));
}

#[test]
fn aggregate_queries_honor_order_offset_and_limit() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":p/g")]).unwrap();
    let items: Vec<TxItem> = [1, 1, 2, 3, 3, 3]
        .iter()
        .map(|g| TxItem::Assert(None, vec![(":p/g".into(), Value::I64(*g))]))
        .collect();
    conn.transact(&items).unwrap();
    // Groups by ?g with counts 2, 1, 3 (distinct ?e per group). Order by ?g descending, skip 1, take 1.
    let r = one(&conn, "{:find [?g (count ?e)] :with [?e] :where [[?e :p/g ?g]] :order-by [[?g :desc]] :offset 1 :limit 1}");
    assert_eq!(r, QueryResult::Rel(vec![vec![Value::I64(2), Value::I64(1)]]));
}

#[test]
fn fulltext_clause_returns_every_match() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":doc/text").fulltext()]).unwrap();
    let items: Vec<TxItem> = (0..150)
        .map(|i| TxItem::Assert(None, vec![(":doc/text".into(), s(&format!("common text {i}")))]))
        .collect();
    conn.transact(&items).unwrap();
    let r = one(&conn, "[:find (count ?e) . :where [(fulltext $ \"common\") [[?e _ _]]]]");
    assert_eq!(r, QueryResult::Scalar(Some(Value::I64(150))));
}

#[test]
fn explicit_ids_cannot_share_a_unique_identity_value() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":u/key").unique_identity()]).unwrap();
    conn.transact(&[TxItem::Assert(Some(1), vec![(":u/key".into(), s("k"))])]).unwrap();
    // A second explicit id with the same identity value would make lookup ambiguous: rejected.
    let err = conn.transact(&[TxItem::Assert(Some(2), vec![(":u/key".into(), s("k"))])]);
    assert!(matches!(err, Err(dtlv_datalog::DatalogError::UniqueViolation { .. })), "got {err:?}");
    // Re-asserting the same value on the same entity is fine.
    conn.transact(&[TxItem::Assert(Some(1), vec![(":u/key".into(), s("k"))])]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    assert_eq!(conn.lookup_entity_by_av(&rtxn, ":u/key", &s("k")).unwrap(), Some(1));
}
