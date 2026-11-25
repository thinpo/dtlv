use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, FtsQuery, FulltextConfig, TxItem};

fn docs(conn: &Conn, texts: &[&str]) {
    let items: Vec<TxItem> = texts
        .iter()
        .map(|t| TxItem::Assert(None, vec![(":doc/text".into(), Value::Str((*t).into()))]))
        .collect();
    conn.transact(&items).unwrap();
}

fn conn_with(cfg: Option<FulltextConfig>) -> Conn {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":doc/text").fulltext()]).unwrap();
    if let Some(cfg) = cfg {
        conn.set_fulltext_config(cfg).unwrap();
    }
    conn
}

fn hits(conn: &Conn, q: &FtsQuery) -> Vec<u64> {
    let rtxn = conn.read_txn().unwrap();
    let mut eids: Vec<u64> = conn.fulltext_query(&rtxn, q, 100).unwrap().into_iter().map(|(e, _)| e).collect();
    eids.sort();
    eids
}

#[test]
fn default_analyzer_drops_stopwords() {
    let conn = conn_with(None);
    docs(&conn, &["the quick fox", "a lazy dog"]);
    let rtxn = conn.read_txn().unwrap();
    assert!(conn.fulltext_search(&rtxn, "the", 10).unwrap().is_empty());
    assert_eq!(conn.fulltext_search(&rtxn, "fox", 10).unwrap().len(), 1);
}

#[test]
fn stemming_matches_inflected_forms_when_enabled_before_indexing() {
    let mut conn = conn_with(None);
    conn.set_fulltext_config(FulltextConfig { stem: true, ..FulltextConfig::default() }).unwrap();
    docs(&conn, &["the dogs were running"]);
    assert_eq!(hits(&conn, &FtsQuery::Text("run".into())), vec![1]);
    assert_eq!(hits(&conn, &FtsQuery::Text("dog".into())), vec![1]);
}

#[test]
fn config_cannot_change_after_documents_are_indexed() {
    let conn_docs = {
        let mut c = conn_with(None);
        docs(&c, &["quick fox"]);
        let err = c.set_fulltext_config(FulltextConfig { stem: true, ..FulltextConfig::default() }).unwrap_err();
        assert!(matches!(err, DatalogError::Kv(_)), "got {err:?}");
        // Setting the same settings again is fine.
        c.set_fulltext_config(FulltextConfig::default()).unwrap();
        c
    };
    assert_eq!(conn_docs.fulltext_config(), FulltextConfig::default());
}

#[test]
fn phrase_requires_positions_and_respects_word_order() {
    let conn = conn_with(None);
    docs(&conn, &["the quick brown fox"]);
    let rtxn = conn.read_txn().unwrap();
    let err = conn.fulltext_query(&rtxn, &FtsQuery::Phrase("quick brown".into()), 10).unwrap_err();
    assert!(matches!(err, DatalogError::Kv(_)), "got {err:?}");
    drop(rtxn);

    let conn = conn_with(Some(FulltextConfig { index_positions: true, ..FulltextConfig::default() }));
    docs(&conn, &["the quick brown fox", "brown quick fox", "quick and brown"]);
    // "quick and brown" has a stopword gap, so it is not an exact phrase match for "quick brown".
    assert_eq!(hits(&conn, &FtsQuery::Phrase("quick brown".into())), vec![1]);
    assert_eq!(hits(&conn, &FtsQuery::Phrase("brown fox".into())), vec![1]);
    assert!(hits(&conn, &FtsQuery::Phrase("fox quick".into())).is_empty());
}

#[test]
fn boolean_and_or_not_compose() {
    let conn = conn_with(None);
    docs(&conn, &["red fox", "red sheep", "black sheep", "black sheep yellow"]);
    let red = FtsQuery::Term("red".into());
    let black = FtsQuery::Term("black".into());
    let sheep = FtsQuery::Term("sheep".into());
    let yellow = FtsQuery::Term("yellow".into());

    assert_eq!(hits(&conn, &FtsQuery::Or(vec![red.clone(), black.clone()])), vec![1, 2, 3, 4]);
    assert_eq!(hits(&conn, &FtsQuery::And(vec![black.clone(), sheep.clone()])), vec![3, 4]);
    // black AND sheep AND NOT yellow
    let q = FtsQuery::And(vec![black, sheep, FtsQuery::Not(Box::new(yellow))]);
    assert_eq!(hits(&conn, &q), vec![3]);
    // NOT red
    assert_eq!(hits(&conn, &FtsQuery::Not(Box::new(red))), vec![3, 4]);
}

#[test]
fn reindexing_replaces_old_postings() {
    let conn = conn_with(None);
    conn.transact(&[TxItem::Assert(Some(1), vec![(":doc/text".into(), Value::Str("alpha".into()))])]).unwrap();
    conn.transact(&[TxItem::Assert(Some(1), vec![(":doc/text".into(), Value::Str("beta".into()))])]).unwrap();
    assert!(hits(&conn, &FtsQuery::Term("alpha".into())).is_empty());
    assert_eq!(hits(&conn, &FtsQuery::Term("beta".into())), vec![1]);
}
