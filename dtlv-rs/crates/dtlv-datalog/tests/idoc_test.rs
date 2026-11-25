use dtlv_codec::Value;
use dtlv_datalog::{idoc_get, idoc_value, pattern_predicates, AttrSpec, Conn, DatalogError, PathSeg, TxItem};

fn doc(text: &str) -> Value {
    idoc_value(text).unwrap()
}

fn schema() -> Vec<AttrSpec> {
    vec![
        AttrSpec::new(":doc/edn").idoc(),
        AttrSpec::new(":doc/other").idoc(),
        AttrSpec::new(":plain/name"),
    ]
}

fn sample(conn: &Conn) -> u64 {
    conn.transact(&[TxItem::Assert(
        None,
        vec![(
            ":doc/edn".into(),
            doc(r#"{"status":"active","profile":{"age":30,"name":"Alice"},"tags":["a","b","b"],"items":[{"sku":"x1"},{"sku":"y2"}]}"#),
        )],
    )])
    .unwrap()[0]
}

fn eids(hits: Vec<(u64, String, Value)>) -> Vec<u64> {
    let mut e: Vec<u64> = hits.into_iter().map(|h| h.0).collect();
    e.sort();
    e.dedup();
    e
}

fn pred(json: &str) -> Vec<(Vec<PathSeg>, Value)> {
    pattern_predicates(&serde_json::from_str(json).unwrap()).unwrap()
}

#[test]
fn idoc_values_are_validated_and_canonicalized() {
    assert!(matches!(idoc_value("3"), Err(DatalogError::Type(_))));
    assert!(matches!(idoc_value("{bad"), Err(DatalogError::Type(_))));
    // Key order is normalized, so equal documents store equal values.
    assert_eq!(doc(r#"{"b":1,"a":2}"#), doc(r#"{"a":2,"b":1}"#));
    assert!(idoc_value("[1,2,3]").is_ok());
}

#[test]
fn idoc_attributes_reject_non_document_values() {
    let conn = Conn::open_mem(schema()).unwrap();
    let err = conn
        .transact(&[TxItem::Assert(None, vec![(":doc/edn".into(), Value::Str("nope".into()))])])
        .unwrap_err();
    assert!(matches!(err, DatalogError::Type(_)), "got {err:?}");
}

#[test]
fn match_by_nested_path_array_membership_and_conjunction() {
    let conn = Conn::open_mem(schema()).unwrap();
    let e = sample(&conn);
    let rtxn = conn.read_txn().unwrap();

    let hits = conn.idoc_match(&rtxn, Some(":doc/edn"), &pred(r#"{"status":"active"}"#)).unwrap();
    assert_eq!(eids(hits), vec![e]);

    // Nested map path.
    assert_eq!(eids(conn.idoc_match(&rtxn, Some(":doc/edn"), &pred(r#"{"profile":{"age":30}}"#)).unwrap()), vec![e]);
    assert!(conn.idoc_match(&rtxn, Some(":doc/edn"), &pred(r#"{"profile":{"age":31}}"#)).unwrap().is_empty());

    // Array membership: a scalar matches any element.
    assert_eq!(eids(conn.idoc_match(&rtxn, Some(":doc/edn"), &pred(r#"{"tags":"b"}"#)).unwrap()), vec![e]);
    assert!(conn.idoc_match(&rtxn, Some(":doc/edn"), &pred(r#"{"tags":"z"}"#)).unwrap().is_empty());

    // Predicates inside an array of maps.
    assert_eq!(eids(conn.idoc_match(&rtxn, Some(":doc/edn"), &pred(r#"{"items":{"sku":"y2"}}"#)).unwrap()), vec![e]);

    // Conjunction: all predicates must hold.
    let both = [pred(r#"{"status":"active"}"#), pred(r#"{"profile":{"name":"Alice"}}"#)].concat();
    assert_eq!(eids(conn.idoc_match(&rtxn, Some(":doc/edn"), &both).unwrap()), vec![e]);
    let mismatch = [pred(r#"{"status":"active"}"#), pred(r#"{"profile":{"name":"Bob"}}"#)].concat();
    assert!(conn.idoc_match(&rtxn, Some(":doc/edn"), &mismatch).unwrap().is_empty());
}

#[test]
fn positional_predicates_use_the_concrete_path() {
    let conn = Conn::open_mem(schema()).unwrap();
    let e = sample(&conn);
    let rtxn = conn.read_txn().unwrap();
    let first_tag_is_a = vec![(vec![PathSeg::Key("tags".into()), PathSeg::Index(0)], Value::Str("a".into()))];
    assert_eq!(eids(conn.idoc_match(&rtxn, Some(":doc/edn"), &first_tag_is_a).unwrap()), vec![e]);
    let second_tag_is_a = vec![(vec![PathSeg::Key("tags".into()), PathSeg::Index(1)], Value::Str("a".into()))];
    assert!(conn.idoc_match(&rtxn, Some(":doc/edn"), &second_tag_is_a).unwrap().is_empty());
}

#[test]
fn full_database_search_spans_idoc_attributes() {
    let conn = Conn::open_mem(schema()).unwrap();
    let a = sample(&conn);
    let b = conn
        .transact(&[TxItem::Assert(None, vec![(":doc/other".into(), doc(r#"{"status":"active"}"#))])])
        .unwrap()[0];
    let rtxn = conn.read_txn().unwrap();
    let hits = conn.idoc_match(&rtxn, None, &pred(r#"{"status":"active"}"#)).unwrap();
    let mut pairs: Vec<(u64, String)> = hits.into_iter().map(|h| (h.0, h.1)).collect();
    pairs.sort();
    assert_eq!(pairs, vec![(a, ":doc/edn".to_string()), (b, ":doc/other".to_string())]);
}

#[test]
fn root_vector_matches_any_element() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":attr1").idoc()]).unwrap();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":attr1".into(), doc("[1,2,5,3,5]"))])]).unwrap()[0];
    let rtxn = conn.read_txn().unwrap();
    let hits = conn
        .idoc_match(&rtxn, Some(":attr1"), &[(vec![], Value::I64(5))])
        .unwrap();
    assert_eq!(eids(hits), vec![e]);
    assert_eq!(conn.get_attr(&rtxn, e, ":attr1").unwrap(), Some(doc("[1,2,5,3,5]")));
}

#[test]
fn replacing_retracting_and_removing_update_the_index() {
    let conn = Conn::open_mem(schema()).unwrap();
    let e = sample(&conn);
    conn.transact(&[TxItem::Assert(Some(e), vec![(":doc/edn".into(), doc(r#"{"status":"inactive"}"#))])]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    assert!(conn.idoc_match(&rtxn, Some(":doc/edn"), &pred(r#"{"tags":"b"}"#)).unwrap().is_empty());
    assert_eq!(eids(conn.idoc_match(&rtxn, Some(":doc/edn"), &pred(r#"{"status":"inactive"}"#)).unwrap()), vec![e]);
    drop(rtxn);

    conn.transact(&[TxItem::RetractAttribute(e, ":doc/edn".into())]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    assert!(conn.idoc_match(&rtxn, Some(":doc/edn"), &pred(r#"{"status":"inactive"}"#)).unwrap().is_empty());
    drop(rtxn);

    let e2 = sample(&conn);
    conn.transact(&[TxItem::RetractEntity(e2)]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    assert!(conn.idoc_match(&rtxn, Some(":doc/edn"), &pred(r#"{"status":"active"}"#)).unwrap().is_empty());
}

#[test]
fn idoc_get_walks_paths_and_missing_paths_are_none() {
    let text = r#"{"profile":{"age":30},"tags":["a","b"]}"#;
    assert_eq!(idoc_get(text, &[PathSeg::Key("profile".into()), PathSeg::Key("age".into())]), Some(serde_json::json!(30)));
    assert_eq!(idoc_get(text, &[PathSeg::Key("tags".into()), PathSeg::Index(1)]), Some(serde_json::json!("b")));
    assert_eq!(idoc_get(text, &[PathSeg::Key("tags".into()), PathSeg::Index(9)]), None);
    assert_eq!(idoc_get(text, &[PathSeg::Key("nope".into())]), None);
}

#[test]
fn patterns_with_arrays_are_rejected() {
    assert!(matches!(
        pattern_predicates(&serde_json::json!({"tags": ["a"]})),
        Err(DatalogError::Type(_))
    ));
}
