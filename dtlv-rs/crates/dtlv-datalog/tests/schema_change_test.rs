use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, TxItem};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn values(conn: &Conn, eid: u64, attr: &str) -> Vec<Value> {
    let rtxn = conn.read_txn().unwrap();
    conn.get_attr_many(&rtxn, eid, attr).unwrap()
}

fn count_attr(conn: &Conn, attr: &str) -> usize {
    let rtxn = conn.read_txn().unwrap();
    conn.datoms(&rtxn, dtlv_datalog::IndexKind::Eavt)
        .unwrap()
        .iter()
        .filter(|(_, a, _)| a == attr)
        .count()
}

fn assert_schema_err(res: Result<(), DatalogError>, needles: &[&str]) {
    match res {
        Err(DatalogError::Schema(msg)) => {
            for n in needles {
                assert!(msg.contains(n), "message {msg:?} lacks {n:?}");
            }
        }
        other => panic!("expected Schema error, got {other:?}"),
    }
}

#[test]
fn one_to_many_keeps_second_value() {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":p/tag")]).unwrap();
    let e1 = conn.transact(&[TxItem::Assert(None, vec![(":p/tag".into(), s("a"))])]).unwrap()[0];
    conn.update_schema(vec![AttrSpec::new(":p/tag").many()], &[], &[]).unwrap();
    conn.transact(&[TxItem::Assert(Some(e1), vec![(":p/tag".into(), s("b"))])]).unwrap();
    let vals = values(&conn, e1, ":p/tag");
    assert_eq!(vals.len(), 2);
    assert!(vals.contains(&s("a")) && vals.contains(&s("b")));
}

#[test]
fn many_to_one_with_single_valued_entities_succeeds() {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":p/tag").many()]).unwrap();
    let e1 = conn.transact(&[TxItem::Assert(None, vec![(":p/tag".into(), s("a"))])]).unwrap()[0];
    let e2 = conn.transact(&[TxItem::Assert(None, vec![(":p/tag".into(), s("b"))])]).unwrap()[0];
    conn.update_schema(vec![AttrSpec::new(":p/tag")], &[], &[]).unwrap();
    conn.transact(&[TxItem::Assert(Some(e1), vec![(":p/tag".into(), s("c"))])]).unwrap();
    assert_eq!(values(&conn, e1, ":p/tag"), vec![s("c")]);
    assert_eq!(values(&conn, e2, ":p/tag"), vec![s("b")]);
}

#[test]
fn many_to_one_with_two_values_is_rejected() {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":p/tag").many()]).unwrap();
    let e1 = conn
        .transact(&[TxItem::Assert(None, vec![(":p/tag".into(), s("a"))])])
        .unwrap()[0];
    conn.transact(&[TxItem::Assert(Some(e1), vec![(":p/tag".into(), s("b"))])]).unwrap();
    assert_schema_err(
        conn.update_schema(vec![AttrSpec::new(":p/tag")], &[], &[]),
        &[":p/tag", "more than one value"],
    );
    // Still cardinality many: a third value is kept alongside the first two.
    conn.transact(&[TxItem::Assert(Some(e1), vec![(":p/tag".into(), s("c"))])]).unwrap();
    assert_eq!(values(&conn, e1, ":p/tag").len(), 3);
}

#[test]
fn none_to_identity_with_distinct_values_succeeds() {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":p/id")]).unwrap();
    let e1 = conn.transact(&[TxItem::Assert(None, vec![(":p/id".into(), s("x"))])]).unwrap()[0];
    let e2 = conn.transact(&[TxItem::Assert(None, vec![(":p/id".into(), s("y"))])]).unwrap()[0];
    conn.update_schema(vec![AttrSpec::new(":p/id").unique_identity()], &[], &[]).unwrap();
    // Identity semantics on a fresh connection: asserting an existing value upserts that entity.
    let again = conn.transact(&[TxItem::Assert(None, vec![(":p/id".into(), s("y"))])]).unwrap()[0];
    assert_eq!(again, e2);
    let rtxn = conn.read_txn().unwrap();
    assert_eq!(conn.lookup_entity_by_av(&rtxn, ":p/id", &s("x")).unwrap(), Some(e1));
    drop(rtxn);
    assert_eq!(count_attr(&conn, ":p/id"), 2);
}

#[test]
fn none_to_unique_with_duplicate_is_rejected() {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":p/id")]).unwrap();
    conn.transact(&[TxItem::Assert(None, vec![(":p/id".into(), s("x"))])]).unwrap();
    conn.transact(&[TxItem::Assert(None, vec![(":p/id".into(), s("x"))])]).unwrap();
    assert_schema_err(
        conn.update_schema(vec![AttrSpec::new(":p/id").unique_value()], &[], &[]),
        &[":p/id", "duplicate value"],
    );
    // Nothing changed: both duplicates are still stored and the attribute is not unique.
    assert_eq!(count_attr(&conn, ":p/id"), 2);
    let e3 = conn.transact(&[TxItem::Assert(None, vec![(":p/id".into(), s("x"))])]).unwrap()[0];
    assert!(e3 > 2, "a non-unique attribute must not upsert");
    assert_eq!(count_attr(&conn, ":p/id"), 3);
}

#[test]
fn unique_to_none_allows_duplicate_on_new_entity() {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":p/mic").unique_value()]).unwrap();
    let e1 = conn.transact(&[TxItem::Assert(None, vec![(":p/mic".into(), s("x"))])]).unwrap()[0];
    conn.update_schema(vec![AttrSpec::new(":p/mic")], &[], &[]).unwrap();
    let e2 = conn.transact(&[TxItem::Assert(None, vec![(":p/mic".into(), s("x"))])]).unwrap()[0];
    assert_ne!(e1, e2);
    assert_eq!(count_attr(&conn, ":p/mic"), 2);
}

#[test]
fn is_component_change_is_rejected() {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":p/c")]).unwrap();
    assert_schema_err(
        conn.update_schema(vec![AttrSpec::new(":p/c").component()], &[], &[]),
        &["cannot change the properties of existing attribute :p/c"],
    );
}
