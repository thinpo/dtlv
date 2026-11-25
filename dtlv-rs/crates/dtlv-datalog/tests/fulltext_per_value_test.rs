use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, TxItem};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn hits(conn: &Conn, q: &str) -> Vec<u64> {
    let rtxn = conn.read_txn().unwrap();
    let mut e: Vec<u64> = conn.fulltext_search(&rtxn, q, 100).unwrap().into_iter().map(|h| h.0).collect();
    e.sort();
    e
}

fn schema() -> Vec<AttrSpec> {
    vec![AttrSpec::new(":doc/title").fulltext().many(), AttrSpec::new(":doc/body").fulltext()]
}

#[test]
fn every_value_of_a_multi_valued_attribute_is_searchable() {
    let conn = Conn::open_mem(schema()).unwrap();
    let e = conn
        .transact(&[TxItem::Assert(None, vec![(":doc/title".into(), s("apple pie")), (":doc/title".into(), s("banana bread"))])])
        .unwrap()[0];
    assert_eq!(hits(&conn, "apple"), vec![e]);
    assert_eq!(hits(&conn, "banana"), vec![e]);
}

#[test]
fn different_fulltext_attributes_of_one_entity_do_not_overwrite_each_other() {
    let conn = Conn::open_mem(schema()).unwrap();
    let e = conn
        .transact(&[TxItem::Assert(None, vec![(":doc/title".into(), s("alpha")), (":doc/body".into(), s("omega"))])])
        .unwrap()[0];
    assert_eq!(hits(&conn, "alpha"), vec![e]);
    assert_eq!(hits(&conn, "omega"), vec![e]);
}

#[test]
fn retracting_one_value_keeps_the_others_searchable() {
    let conn = Conn::open_mem(schema()).unwrap();
    let e = conn
        .transact(&[TxItem::Assert(None, vec![(":doc/title".into(), s("apple")), (":doc/title".into(), s("banana"))])])
        .unwrap()[0];
    conn.transact(&[TxItem::Retract(e, ":doc/title".into(), s("apple"))]).unwrap();
    assert!(hits(&conn, "apple").is_empty());
    assert_eq!(hits(&conn, "banana"), vec![e]);
}

#[test]
fn replacing_a_single_value_removes_only_the_old_text() {
    let conn = Conn::open_mem(schema()).unwrap();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":doc/body".into(), s("first text"))])]).unwrap()[0];
    conn.transact(&[TxItem::Assert(Some(e), vec![(":doc/body".into(), s("second text"))])]).unwrap();
    assert!(hits(&conn, "first").is_empty());
    assert_eq!(hits(&conn, "second"), vec![e]);
}

#[test]
fn retracting_an_entity_removes_all_of_its_documents() {
    let conn = Conn::open_mem(schema()).unwrap();
    let e = conn
        .transact(&[TxItem::Assert(None, vec![(":doc/title".into(), s("apple")), (":doc/body".into(), s("pear"))])])
        .unwrap()[0];
    conn.transact(&[TxItem::RetractEntity(e)]).unwrap();
    assert!(hits(&conn, "apple").is_empty());
    assert!(hits(&conn, "pear").is_empty());
}

#[test]
fn fulltext_datoms_report_the_matched_attribute_and_value() {
    let conn = Conn::open_mem(schema()).unwrap();
    let e = conn
        .transact(&[TxItem::Assert(None, vec![(":doc/title".into(), s("apple pie")), (":doc/body".into(), s("pie crust"))])])
        .unwrap()[0];
    let rtxn = conn.read_txn().unwrap();
    let mut got = conn.fulltext_datoms(&rtxn, "apple", 10).unwrap();
    got.sort_by_key(|d| d.1.clone());
    assert_eq!(got, vec![(e, ":doc/title".to_string(), s("apple pie"))]);
    let mut pies = conn.fulltext_datoms(&rtxn, "pie", 10).unwrap();
    pies.sort_by_key(|d| d.1.clone());
    assert_eq!(pies.len(), 2, "both the title and the body mention pie");
}
