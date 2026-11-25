use dtlv_datalog::{idoc_value, idoc_value_edn, pattern_predicates, AttrSpec, Conn, DatalogError, TxItem};
use serde_json::json;

fn count(conn: &Conn, status: &str) -> usize {
    let rtxn = conn.read_txn().unwrap();
    conn.idoc_match(&rtxn, Some(":doc/edn"), &pattern_predicates(&json!({"status": status})).unwrap())
        .unwrap()
        .len()
}

#[test]
fn edn_documents_store_and_index_like_json() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":doc/edn").idoc()]).unwrap();
    let edn = idoc_value_edn(r#"{:status "active" :profile {:age 30} :tags ["a" "b"]}"#).unwrap();
    let json = idoc_value(r#"{"status":"active","profile":{"age":30},"tags":["a","b"]}"#).unwrap();
    assert_eq!(edn, json, "EDN and JSON forms of the same document should be identical");
    conn.transact(&[TxItem::Assert(None, vec![(":doc/edn".into(), edn)])]).unwrap();
    assert_eq!(count(&conn, "active"), 1);
}

#[test]
fn edn_inputs_outside_the_document_model_are_rejected() {
    assert!(matches!(idoc_value_edn("[1 2"), Err(DatalogError::Type(_))));
    assert!(matches!(idoc_value_edn("3"), Err(DatalogError::Type(_))));
    assert!(matches!(idoc_value_edn(r#"{:k :some-keyword}"#), Err(DatalogError::Type(_))));
    assert!(matches!(idoc_value_edn(r#"{:k (1 2)}"#), Err(DatalogError::Type(_))));
    assert!(matches!(idoc_value_edn(r#"{1 2}"#), Err(DatalogError::Type(_))));
}
