use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, TxItem};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

#[test]
fn noindex_attributes_are_kept_but_not_value_indexed() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":n/secret").indexed().noindex(), AttrSpec::new(":n/name")]).unwrap();
    let e = conn
        .transact(&[TxItem::Assert(None, vec![(":n/secret".into(), s("hunter2")), (":n/name".into(), s("ann"))])])
        .unwrap()[0];
    let rtxn = conn.read_txn().unwrap();
    // Entity reads still see the value.
    assert_eq!(conn.get_attr(&rtxn, e, ":n/secret").unwrap(), Some(s("hunter2")));
    // Nothing was written to the value index, so value lookups find nothing.
    assert_eq!(conn.lookup_entity_by_av(&rtxn, ":n/secret", &s("hunter2")).unwrap(), None);
    // Unconstrained scans still work.
    let rows = conn.q_str(&rtxn, "[:find ?v :where [?e :n/secret ?v]]", &[]).unwrap();
    assert!(matches!(rows, dtlv_datalog::QueryResult::Rel(ref r) if r.len() == 1));
}

#[test]
fn value_conditions_on_noindex_attributes_are_rejected() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":n/secret").noindex()]).unwrap();
    conn.transact(&[TxItem::Assert(None, vec![(":n/secret".into(), s("x"))])]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    let err = conn.q_str(&rtxn, r#"[:find ?e :where [?e :n/secret "x"]]"#, &[]).unwrap_err();
    assert!(matches!(err, DatalogError::Type(_)), "got {err:?}");
}

#[test]
fn unique_attributes_stay_indexed_even_when_marked_noindex() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":n/key").unique_identity().noindex()]).unwrap();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":n/key".into(), s("k"))])]).unwrap()[0];
    let rtxn = conn.read_txn().unwrap();
    assert_eq!(conn.lookup_entity_by_av(&rtxn, ":n/key", &s("k")).unwrap(), Some(e));
}
