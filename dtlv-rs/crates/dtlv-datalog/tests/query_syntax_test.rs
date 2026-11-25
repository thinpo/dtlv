use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, FulltextConfig, QueryInput, QueryResult, TxItem, idoc_value};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn conn() -> Conn {
    let mut conn = Conn::open_mem(vec![
        AttrSpec::new(":doc/text").fulltext(),
        AttrSpec::new(":doc/edn").idoc(),
    ])
    .unwrap();
    conn.set_fulltext_config(FulltextConfig { index_positions: true, ..FulltextConfig::default() }).unwrap();
    conn.transact(&[
        TxItem::Assert(
            None,
            vec![
                (":doc/text".into(), s("the red fox")),
                (":doc/edn".into(), idoc_value(r#"{"status":"active","tags":["a","b"]}"#).unwrap()),
            ],
        ),
        TxItem::Assert(
            None,
            vec![
                (":doc/text".into(), s("a red sheep")),
                (":doc/edn".into(), idoc_value(r#"{"status":"inactive","tags":["b"]}"#).unwrap()),
            ],
        ),
    ])
    .unwrap();
    conn
}

fn rel(conn: &Conn, q: &str, inputs: &[QueryInput]) -> Vec<Vec<Value>> {
    let rtxn = conn.read_txn().unwrap();
    match conn.q_str_inputs(&rtxn, q, inputs, "[]").unwrap() {
        QueryResult::Rel(r) => {
            let mut r = r;
            r.sort_by_key(|row| format!("{row:?}"));
            r
        }
        other => panic!("expected Rel, got {other:?}"),
    }
}

#[test]
fn fulltext_boolean_and_phrase_in_edn() {
    let conn = conn();
    let q = r#"[:find ?e :where [(fulltext $ [:or "fox" {:phrase "red sheep"}]) [[?e _ _]]]]"#;
    assert_eq!(rel(&conn, q, &[]).len(), 2);
    let q = r#"[:find ?e :where [(fulltext $ [:and "red" [:not "fox"]]) [[?e _ _]]]]"#;
    assert_eq!(rel(&conn, q, &[]), vec![vec![Value::Ref(2)]]);
    let q = r#"[:find ?e :where [(fulltext $ {:phrase "red fox"}) [[?e _ _]]]]"#;
    assert_eq!(rel(&conn, q, &[]), vec![vec![Value::Ref(1)]]);
}

#[test]
fn fulltext_takes_a_bound_string_variable() {
    let conn = conn();
    let q = "[:find ?e :in $ ?q :where [(fulltext $ ?q) ?e]]";
    assert_eq!(rel(&conn, q, &[QueryInput::Scalar(s("sheep"))]), vec![vec![Value::Ref(2)]]);
}

#[test]
fn idoc_match_binds_entity_attribute_and_document() {
    let conn = conn();
    let q = r#"[:find ?e ?a :where [(idoc-match $ :doc/edn {:status "active"}) [[?e ?a _]]]]"#;
    assert_eq!(rel(&conn, q, &[]), vec![vec![Value::Ref(1), Value::Keyword(":doc/edn".into())]]);

    let q = r#"[:find ?e :where [(idoc-match $ {:tags "b"}) [[?e _ _]]]]"#;
    assert_eq!(rel(&conn, q, &[]).len(), 2);
}

#[test]
fn idoc_match_pattern_can_use_an_input_variable() {
    let conn = conn();
    let q = r#"[:find ?e :in $ ?s :where [(idoc-match $ :doc/edn {:status ?s}) [[?e _ _]]]]"#;
    assert_eq!(rel(&conn, q, &[QueryInput::Scalar(s("inactive"))]), vec![vec![Value::Ref(2)]]);
}

#[test]
fn malformed_fulltext_operator_is_a_parse_error() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let err = conn
        .q_str(&rtxn, r#"[:find ?e :where [(fulltext $ [:xor "a" "b"]) [[?e _ _]]]]"#, &[])
        .unwrap_err();
    assert!(matches!(err, DatalogError::Parse(_)), "got {err:?}");
}
