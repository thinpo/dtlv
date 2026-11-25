use dtlv_datalog::{idoc_value, pattern_predicates, AttrSpec, Conn, DatalogError, PathSeg, TxItem};
use serde_json::json;

fn key(k: &str) -> PathSeg {
    PathSeg::Key(k.into())
}

fn count(conn: &Conn, attr: &str, pat: serde_json::Value) -> usize {
    let rtxn = conn.read_txn().unwrap();
    conn.idoc_match(&rtxn, Some(attr), &pattern_predicates(&pat).unwrap()).unwrap().len()
}

fn doc() -> dtlv_codec::Value {
    idoc_value(r#"{"status":"active","profile":{"age":30},"tags":["x"]}"#).unwrap()
}

#[test]
fn attributes_in_one_domain_share_its_selectors() {
    let mut conn = Conn::open_mem(vec![
        AttrSpec::new(":a/doc").idoc().idoc_domain("profiles"),
        AttrSpec::new(":b/doc").idoc().idoc_domain("profiles"),
    ])
    .unwrap();
    conn.set_idoc_domain("profiles", Some(vec![vec![key("status")]]), vec![]).unwrap();
    conn.transact(&[
        TxItem::Assert(None, vec![(":a/doc".into(), doc())]),
        TxItem::Assert(None, vec![(":b/doc".into(), doc())]),
    ])
    .unwrap();
    assert_eq!(count(&conn, ":a/doc", json!({"status": "active"})), 1);
    assert_eq!(count(&conn, ":b/doc", json!({"status": "active"})), 1);
    assert_eq!(count(&conn, ":a/doc", json!({"tags": "x"})), 0, "tags is outside the domain");
}

#[test]
fn an_attribute_with_its_own_selectors_overrides_its_domain() {
    let mut conn = Conn::open_mem(vec![
        AttrSpec::new(":a/doc").idoc().idoc_domain("profiles").idoc_indexed_paths(vec![vec![key("tags")]]),
    ])
    .unwrap();
    conn.set_idoc_domain("profiles", Some(vec![vec![key("status")]]), vec![]).unwrap();
    conn.transact(&[TxItem::Assert(None, vec![(":a/doc".into(), doc())])]).unwrap();
    assert_eq!(count(&conn, ":a/doc", json!({"tags": "x"})), 1);
    assert_eq!(count(&conn, ":a/doc", json!({"status": "active"})), 0);
}

#[test]
fn excluded_domain_paths_are_omitted() {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":a/doc").idoc().idoc_domain("p")]).unwrap();
    conn.set_idoc_domain("p", None, vec![vec![key("profile")]]).unwrap();
    conn.transact(&[TxItem::Assert(None, vec![(":a/doc".into(), doc())])]).unwrap();
    assert_eq!(count(&conn, ":a/doc", json!({"status": "active"})), 1);
    assert_eq!(count(&conn, ":a/doc", json!({"profile": {"age": 30}})), 0);
}

#[test]
fn domains_cannot_change_once_documents_are_indexed() {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":a/doc").idoc().idoc_domain("p")]).unwrap();
    conn.transact(&[TxItem::Assert(None, vec![(":a/doc".into(), doc())])]).unwrap();
    let err = conn.set_idoc_domain("p", None, vec![]).unwrap_err();
    assert!(matches!(err, DatalogError::Type(_)), "got {err:?}");
}
