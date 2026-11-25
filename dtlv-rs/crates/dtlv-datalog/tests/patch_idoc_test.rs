use dtlv_codec::Value;
use dtlv_datalog::{idoc_get, idoc_value, pattern_predicates, AttrSpec, Conn, DatalogError, PatchOp, PathSeg, TxItem};
use serde_json::json;

fn key(k: &str) -> PathSeg {
    PathSeg::Key(k.into())
}

fn setup() -> (Conn, u64) {
    let conn = Conn::open_mem(vec![AttrSpec::new(":doc/edn").idoc()]).unwrap();
    let doc = r#"{"status":"active","profile":{"age":30,"middle":"J"},"tags":["a","b"],"score":10}"#;
    let e = conn.transact(&[TxItem::Assert(None, vec![(":doc/edn".into(), idoc_value(doc).unwrap())])]).unwrap()[0];
    (conn, e)
}

fn stored(conn: &Conn, e: u64) -> String {
    let rtxn = conn.read_txn().unwrap();
    match conn.get_attr(&rtxn, e, ":doc/edn").unwrap() {
        Some(Value::Doc(t)) => t,
        other => panic!("expected a document, got {other:?}"),
    }
}

fn patch(conn: &Conn, e: u64, ops: Vec<PatchOp>) -> Result<(), DatalogError> {
    conn.transact(&[TxItem::PatchIdoc(e, ":doc/edn".into(), ops)]).map(|_| ())
}

fn matches(conn: &Conn, json_pattern: serde_json::Value) -> usize {
    let rtxn = conn.read_txn().unwrap();
    conn.idoc_match(&rtxn, Some(":doc/edn"), &pattern_predicates(&json_pattern).unwrap()).unwrap().len()
}

#[test]
fn set_unset_conj_merge_assoc_dissoc_inc_dec() {
    let (conn, e) = setup();
    patch(
        &conn,
        e,
        vec![
            PatchOp::Set(vec![key("profile"), key("age")], json!(31)),
            PatchOp::Unset(vec![key("profile"), key("middle")]),
            PatchOp::Conj(vec![key("tags")], json!("c")),
            PatchOp::Merge(vec![key("profile")], json!({"city": "Oslo", "age": 32})),
            PatchOp::Assoc(vec![], "extra".into(), json!(true)),
            PatchOp::Dissoc(vec![], "status".into()),
            PatchOp::Inc(vec![key("score")], 5),
            PatchOp::Dec(vec![key("score")], 2),
        ],
    )
    .unwrap();
    let doc: serde_json::Value = serde_json::from_str(&stored(&conn, e)).unwrap();
    assert_eq!(
        doc,
        json!({"profile": {"age": 32, "city": "Oslo"}, "tags": ["a", "b", "c"], "score": 13, "extra": true})
    );
}

#[test]
fn set_replaces_a_vector_element_and_unset_removes_it() {
    let (conn, e) = setup();
    patch(&conn, e, vec![PatchOp::Set(vec![key("tags"), PathSeg::Index(0)], json!("z"))]).unwrap();
    patch(&conn, e, vec![PatchOp::Unset(vec![key("tags"), PathSeg::Index(1)])]).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&stored(&conn, e)).unwrap();
    assert_eq!(doc["tags"], json!(["z"]));
}

#[test]
fn index_follows_the_patched_document() {
    let (conn, e) = setup();
    assert_eq!(matches(&conn, json!({"status": "active"})), 1);
    patch(&conn, e, vec![PatchOp::Set(vec![key("status")], json!("retired"))]).unwrap();
    assert_eq!(matches(&conn, json!({"status": "active"})), 0);
    assert_eq!(matches(&conn, json!({"status": "retired"})), 1);
    assert_eq!(matches(&conn, json!({"tags": "a"})), 1);
    patch(&conn, e, vec![PatchOp::Conj(vec![key("tags")], json!("q"))]).unwrap();
    assert_eq!(matches(&conn, json!({"tags": "q"})), 1);
    assert_eq!(idoc_get(&stored(&conn, e), &[key("status")].to_vec()), Some(json!("retired")));
}

#[test]
fn patch_is_atomic_with_the_rest_of_the_transaction() {
    let (conn, e) = setup();
    // The second op fails (no such key to inc), so nothing from this transaction may apply.
    let err = conn.transact(&[
        TxItem::Assert(None, vec![(":doc/edn".into(), idoc_value(r#"{"other":1}"#).unwrap())]),
        TxItem::PatchIdoc(e, ":doc/edn".into(), vec![PatchOp::Set(vec![key("status")], json!("x")), PatchOp::Inc(vec![key("nope")], 1)]),
    ]);
    assert!(err.is_err());
    let doc: serde_json::Value = serde_json::from_str(&stored(&conn, e)).unwrap();
    assert_eq!(doc["status"], json!("active"));
    assert_eq!(matches(&conn, json!({"other": 1})), 0);
}

#[test]
fn invalid_patches_are_rejected() {
    let (conn, e) = setup();
    // Target does not exist.
    assert!(matches!(
        patch(&conn, e, vec![PatchOp::Conj(vec![key("missing")], json!(1))]),
        Err(DatalogError::Type(_))
    ));
    // Inc on a string.
    assert!(matches!(patch(&conn, e, vec![PatchOp::Inc(vec![key("status")], 1)]), Err(DatalogError::Type(_))));
    // Root must stay a map or vector.
    assert!(matches!(patch(&conn, e, vec![PatchOp::Set(vec![], json!(3))]), Err(DatalogError::Type(_))));
    // Entity without a document.
    let other = conn.transact(&[TxItem::Assert(None, vec![])]).unwrap()[0];
    assert!(matches!(patch(&conn, other, vec![PatchOp::Assoc(vec![], "a".into(), json!(1))]), Err(DatalogError::Type(_))));
}

#[test]
fn cardinality_many_patch_targets_the_document_given_as_old() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":doc/many").idoc().many()]).unwrap();
    let a = idoc_value(r#"{"id":1,"status":"a"}"#).unwrap();
    let b = idoc_value(r#"{"id":2,"status":"b"}"#).unwrap();
    let e = conn
        .transact(&[TxItem::Assert(None, vec![(":doc/many".into(), a.clone()), (":doc/many".into(), b.clone())])])
        .unwrap()[0];
    conn.transact(&[TxItem::PatchIdocOf(e, ":doc/many".into(), a, vec![PatchOp::Set(vec![key("status")], json!("done"))])])
        .unwrap();

    let rtxn = conn.read_txn().unwrap();
    let docs: Vec<String> = conn
        .get_attr_many(&rtxn, e, ":doc/many")
        .unwrap()
        .into_iter()
        .map(|v| match v {
            Value::Doc(t) => t,
            other => panic!("{other:?}"),
        })
        .collect();
    drop(rtxn);
    assert_eq!(docs.len(), 2);
    assert!(docs.contains(&r#"{"id":1,"status":"done"}"#.to_string()));
    assert!(docs.contains(&r#"{"id":2,"status":"b"}"#.to_string()));
    let count = |status: &str| {
        let rtxn = conn.read_txn().unwrap();
        conn.idoc_match(&rtxn, Some(":doc/many"), &pattern_predicates(&json!({"status": status})).unwrap())
            .unwrap()
            .len()
    };
    assert_eq!(count("done"), 1);
    assert_eq!(count("a"), 0);
}

#[test]
fn cardinality_many_patch_with_a_missing_document_is_rejected() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":doc/many").idoc().many()]).unwrap();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":doc/many".into(), idoc_value(r#"{"x":1}"#).unwrap())])]).unwrap()[0];
    let stale = idoc_value(r#"{"x":2}"#).unwrap();
    let err = conn.transact(&[TxItem::PatchIdocOf(e, ":doc/many".into(), stale, vec![PatchOp::Set(vec![key("x")], json!(3))])]);
    assert!(matches!(err, Err(DatalogError::Type(_))));
}
