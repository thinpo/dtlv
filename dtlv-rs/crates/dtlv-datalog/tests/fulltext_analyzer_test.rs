use std::sync::Arc;

use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, FtsQuery, FulltextConfig, TxItem};
use dtlv_search::Analyzer;

/// Splits on `-` and whitespace, keeps case, drops nothing, positions 0, 1, 2...
fn custom() -> Analyzer {
    Arc::new(|text: &str| {
        text.split(|c: char| c == '-' || c.is_whitespace())
            .filter(|t| !t.is_empty())
            .enumerate()
            .map(|(i, t)| (t.to_string(), i as u32))
            .collect()
    })
}

fn open(analyzer: Option<Analyzer>) -> Conn {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":doc/text").fulltext()]).unwrap();
    conn.set_fulltext_analyzer(analyzer);
    conn
}

fn docs(conn: &Conn, texts: &[&str]) {
    let items: Vec<TxItem> = texts
        .iter()
        .map(|t| TxItem::Assert(None, vec![(":doc/text".into(), Value::Str((*t).into()))]))
        .collect();
    conn.transact(&items).unwrap();
}

fn hits(conn: &Conn, q: &FtsQuery) -> Vec<u64> {
    let rtxn = conn.read_txn().unwrap();
    let mut eids: Vec<u64> = conn.fulltext_query(&rtxn, q, 100).unwrap().into_iter().map(|(e, _)| e).collect();
    eids.sort();
    eids
}

#[test]
fn custom_analyzer_keeps_case_and_splits_on_dash() {
    let conn = open(Some(custom()));
    docs(&conn, &["Foo-Bar baz"]);
    assert_eq!(hits(&conn, &FtsQuery::Text("Bar".into())), vec![1]);
    assert!(hits(&conn, &FtsQuery::Text("bar".into())).is_empty());
}

#[test]
fn default_analyzer_lowercases_for_comparison() {
    let conn = open(None);
    docs(&conn, &["Foo-Bar baz"]);
    assert_eq!(hits(&conn, &FtsQuery::Text("bar".into())), vec![1]);
}

#[test]
fn term_query_matches_custom_analyzer_terms() {
    let conn = open(Some(custom()));
    docs(&conn, &["Foo-Bar baz"]);
    assert_eq!(hits(&conn, &FtsQuery::Term("Foo".into())), vec![1]);
}

#[test]
fn phrase_uses_custom_analyzer_positions() {
    let mut conn = open(Some(custom()));
    conn.set_fulltext_config(FulltextConfig { index_positions: true, ..FulltextConfig::default() }).unwrap();
    docs(&conn, &["Foo-Bar baz"]);
    assert_eq!(hits(&conn, &FtsQuery::Phrase("Foo-Bar".into())), vec![1]);
}

#[test]
fn clearing_analyzer_restores_default_for_new_docs() {
    let mut conn = open(Some(custom()));
    docs(&conn, &["Foo-Bar baz"]);
    conn.set_fulltext_analyzer(None);
    docs(&conn, &["Quux Corge"]);
    assert_eq!(hits(&conn, &FtsQuery::Text("quux".into())), vec![2]);
}
