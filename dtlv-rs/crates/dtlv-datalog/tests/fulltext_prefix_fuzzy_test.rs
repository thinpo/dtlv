use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, QueryResult, TxItem};

/// Docs in insert order: 0 "database systems", 1 "datalog engine", 2 "data lake", 3 "graph store".
fn conn_with_docs() -> (Conn, Vec<Value>) {
    let conn = Conn::open_mem(vec![AttrSpec::new(":doc/text").fulltext()]).unwrap();
    let texts = ["database systems", "datalog engine", "data lake", "graph store"];
    let items: Vec<TxItem> = texts
        .iter()
        .map(|t| TxItem::Assert(None, vec![(":doc/text".into(), Value::Str((*t).into()))]))
        .collect();
    let ids = conn.transact(&items).unwrap();
    (conn, ids.into_iter().map(Value::Ref).collect())
}

/// Entities matched by `(fulltext $ <fts>)`, as sorted `?e` values.
fn hits(conn: &Conn, fts: &str) -> Vec<Value> {
    let rtxn = conn.read_txn().unwrap();
    let q = format!("[:find ?e :where [(fulltext $ {fts}) [[?e _ _]]]]");
    match conn.q_str(&rtxn, &q, &[]).unwrap() {
        QueryResult::Rel(mut rows) => {
            rows.sort_by_key(|row| format!("{row:?}"));
            rows.into_iter().map(|row| row[0].clone()).collect()
        }
        other => panic!("expected Rel, got {other:?}"),
    }
}

#[test]
fn prefix_matches_every_term_with_that_start() {
    let (conn, ids) = conn_with_docs();
    let mut want = vec![ids[0].clone(), ids[1].clone(), ids[2].clone()];
    want.sort_by_key(|v| format!("{v:?}"));
    assert_eq!(hits(&conn, r#"[:prefix "dat"]"#), want);
}

#[test]
fn empty_or_unmatched_prefix_matches_nothing() {
    let (conn, _) = conn_with_docs();
    assert!(hits(&conn, r#"[:prefix "zzz"]"#).is_empty());
    assert!(hits(&conn, r#"[:prefix ""]"#).is_empty());
}

#[test]
fn fuzzy_tolerates_one_edit_by_default() {
    let (conn, ids) = conn_with_docs();
    assert_eq!(hits(&conn, r#"[:fuzzy "grph"]"#), vec![ids[3].clone()]);
}

#[test]
fn empty_fuzzy_string_matches_nothing() {
    let (conn, _) = conn_with_docs();
    assert!(hits(&conn, r#"[:fuzzy ""]"#).is_empty());
}

#[test]
fn fuzzy_distance_zero_is_exact() {
    let (conn, _) = conn_with_docs();
    assert!(hits(&conn, r#"[:fuzzy "grph" 0]"#).is_empty());
}

#[test]
fn prefix_combines_with_and() {
    let (conn, ids) = conn_with_docs();
    assert_eq!(hits(&conn, r#"[:and [:prefix "dat"] {:term "lake"}]"#), vec![ids[2].clone()]);
}

#[test]
fn fuzzy_distance_above_two_is_a_parse_error() {
    let (conn, _) = conn_with_docs();
    let rtxn = conn.read_txn().unwrap();
    let err = conn
        .q_str(&rtxn, r#"[:find ?e :where [(fulltext $ [:fuzzy "x" 3]) [[?e _ _]]]]"#, &[])
        .unwrap_err();
    assert!(matches!(err, DatalogError::Parse(_)), "got {err:?}");
}
