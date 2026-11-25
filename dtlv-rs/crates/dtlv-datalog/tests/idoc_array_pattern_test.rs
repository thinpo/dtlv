use dtlv_codec::Value;
use dtlv_datalog::{idoc_value, pattern_predicates, AttrSpec, Conn, DatalogError, PathSeg, TxItem};

fn schema() -> Vec<AttrSpec> {
    vec![AttrSpec::new(":doc/edn").idoc()]
}

fn doc(text: &str) -> Value {
    idoc_value(text).unwrap()
}

fn pred(json: &str) -> Vec<(Vec<PathSeg>, Value)> {
    pattern_predicates(&serde_json::from_str(json).unwrap()).unwrap()
}

fn eids(hits: Vec<(u64, String, Value)>) -> Vec<u64> {
    let mut e: Vec<u64> = hits.into_iter().map(|h| h.0).collect();
    e.sort();
    e.dedup();
    e
}

/// Asserts a match query returns exactly the documents named by `expected` (in insertion order A, B, C, D).
fn assert_matches(conn: &Conn, json: &str, expected: &[u64]) {
    let rtxn = conn.read_txn().unwrap();
    let hits = conn.idoc_match(&rtxn, Some(":doc/edn"), &pred(json)).unwrap();
    assert_eq!(eids(hits), expected, "pattern {json}");
}

#[test]
fn array_patterns_match_by_containment() {
    let conn = Conn::open_mem(schema()).unwrap();
    let assert_doc =|text: &str| {
        conn.transact(&[TxItem::Assert(None, vec![(":doc/edn".into(), doc(text))])]).unwrap()[0]
    };
    let a = assert_doc(r#"{"tags":["a","b","c"]}"#);
    let b = assert_doc(r#"{"tags":["a"]}"#);
    let c = assert_doc(r#"{"tags":"a"}"#); // scalar, not an array
    let d = assert_doc(r#"{"items":[{"sku":1,"qty":2},{"sku":3,"qty":4}]}"#);

    // Scalar vs array: an unpositioned path matches across array elements, and a scalar leaf
    // sits at the same unpositioned path, so `{"tags": ["a"]}` (predicate `tags = "a"`) matches
    // the scalar document C as well as the arrays A and B.
    assert_matches(&conn, r#"{"tags":["a"]}"#, &[a, b, c]);
    // Containment: every listed element must be present.
    assert_matches(&conn, r#"{"tags":["a","b"]}"#, &[a]);
    assert_matches(&conn, r#"{"tags":["a","z"]}"#, &[]);
    // Object elements match across array elements, field by field.
    assert_matches(&conn, r#"{"items":[{"sku":3}]}"#, &[d]);
    assert_matches(&conn, r#"{"items":[{"sku":3,"qty":4}]}"#, &[d]);
}

#[test]
fn empty_array_pattern_is_an_error() {
    let err = pattern_predicates(&serde_json::json!({"tags": []})).unwrap_err();
    assert!(
        matches!(&err, DatalogError::Type(m) if m == "idoc patterns cannot contain empty arrays"),
        "got {err:?}"
    );
}

#[test]
fn root_must_be_an_object() {
    assert!(matches!(
        pattern_predicates(&serde_json::json!(["a"])),
        Err(DatalogError::Type(_))
    ));
}
