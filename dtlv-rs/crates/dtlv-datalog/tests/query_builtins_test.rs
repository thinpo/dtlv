use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, QueryInput, QueryResult, TxItem};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn conn() -> Conn {
    let schema = vec![
        AttrSpec::new(":person/id").unique_identity(),
        AttrSpec::new(":person/name").indexed(),
        AttrSpec::new(":person/nick"),
        AttrSpec::new(":person/age"),
    ];
    let conn = Conn::open_mem(schema).unwrap();
    conn.transact(&[
        TxItem::Assert(
            None,
            vec![
                (":person/id".into(), Value::I64(1)),
                (":person/name".into(), s("Ann")),
                (":person/nick".into(), s("A")),
                (":person/age".into(), Value::I64(30)),
            ],
        ),
        TxItem::Assert(
            None,
            vec![
                (":person/id".into(), Value::I64(2)),
                (":person/name".into(), s("Bob")),
                (":person/age".into(), Value::I64(7)),
            ],
        ),
    ])
    .unwrap();
    conn
}

fn rows(res: QueryResult) -> Vec<Vec<Value>> {
    match res {
        QueryResult::Rel(r) => r,
        other => panic!("expected Rel, got {other:?}"),
    }
}

#[test]
fn get_else_falls_back_and_get_some_picks_first_present_attr() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let r = rows(
        conn.q_str(
            &rtxn,
            r#"[:find ?name ?nick :where [?e :person/name ?name]
                [(get-else $ ?e :person/nick "none") ?nick]]"#,
            &[],
        )
        .unwrap(),
    );
    let mut r = r;
    r.sort_by_key(|row| format!("{:?}", row[0]));
    assert_eq!(r, vec![vec![s("Ann"), s("A")], vec![s("Bob"), s("none")]]);

    let r = rows(
        conn.q_str(
            &rtxn,
            r#"[:find ?name ?a ?v :where [?e :person/name ?name]
                [(get-some $ ?e :person/nick :person/age) [?a ?v]]]"#,
            &[],
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 2);
    assert!(r.contains(&vec![s("Ann"), Value::Keyword(":person/nick".into()), s("A")]));
    assert!(r.contains(&vec![s("Bob"), Value::Keyword(":person/age".into()), Value::I64(7)]));
}

#[test]
fn missing_filter_ground_tuple_and_untuple() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let r = rows(
        conn.q_str(
            &rtxn,
            r#"[:find ?name :where [?e :person/name ?name]
                [(missing? $ ?e :person/nick)]]"#,
            &[],
        )
        .unwrap(),
    );
    assert_eq!(r, vec![vec![s("Bob")]]);

    let r = rows(
        conn.q_str(
            &rtxn,
            r#"[:find ?x ?t ?a ?b :where [(ground 5) ?x]
                [(tuple ?x "k") ?t] [(untuple ?t) [?a ?b]]]"#,
            &[],
        )
        .unwrap(),
    );
    assert_eq!(r, vec![vec![Value::I64(5), Value::Tuple(vec![Value::I64(5), s("k")]), Value::I64(5), s("k")]]);
}

#[test]
fn arithmetic_bindings_and_integer_semantics() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let r = rows(
        conn.q_str(
            &rtxn,
            r#"[:find ?n ?y ?d ?m ?q ?f :where [?e :person/age ?n]
                [(+ ?n 1) ?y] [(/ ?n 2) ?d] [(mod -7 3) ?m] [(quot -7 2) ?q]
                [(/ ?n 4.0) ?f] [(= ?n ?n)] [(> ?n 10)]]"#,
            &[],
        )
        .unwrap(),
    );
    assert_eq!(r, vec![vec![Value::I64(30), Value::I64(31), Value::I64(15), Value::I64(2), Value::I64(-3), Value::F64(7.5)]]);

    let r = rows(
        conn.q_str(&rtxn, r#"[:find ?d :where [(/ 7 2) ?d]]"#, &[]).unwrap(),
    );
    assert_eq!(r, vec![vec![Value::F64(3.5)]]);

    let err = conn.q_str(&rtxn, r#"[:find ?d :where [(/ 1 0) ?d]]"#, &[]).unwrap_err();
    assert!(matches!(err, DatalogError::Function(_)), "got {err:?}");

    let err = conn
        .q_str(&rtxn, &format!("[:find ?d :where [(+ {} 1) ?d]]", i64::MAX), &[])
        .unwrap_err();
    assert!(matches!(err, DatalogError::Function(_)), "got {err:?}");
}

#[test]
fn not_like_in_and_not_in() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let r = rows(
        conn.q_str(
            &rtxn,
            r#"[:find ?n :where [?e :person/name ?n] [(not-like ?n "A%")]]"#,
            &[],
        )
        .unwrap(),
    );
    assert_eq!(r, vec![vec![s("Bob")]]);

    let r = rows(
        conn.q_str(
            &rtxn,
            r#"[:find ?n :where [?e :person/name ?n] [(in ?n ["Ann" "Cy"])]]"#,
            &[],
        )
        .unwrap(),
    );
    assert_eq!(r, vec![vec![s("Ann")]]);

    let r = rows(
        conn.q_str(
            &rtxn,
            r#"[:find ?n :where [?e :person/name ?n] [(not-in ?n ["Ann" "Cy"])]]"#,
            &[],
        )
        .unwrap(),
    );
    assert_eq!(r, vec![vec![s("Bob")]]);
}

#[test]
fn in_collection_tuple_and_relation_bindings() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let names = QueryInput::Coll(vec![s("Ann"), s("Bob")]);
    let r = rows(
        conn.q_str_inputs(
            &rtxn,
            r#"[:find ?e ?n :in [?n ...] :where [?e :person/name ?n]]"#,
            &[names],
            "[]",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 2);

    let pair = QueryInput::Tuple(vec![Value::I64(1), s("Ann")]);
    let r = rows(
        conn.q_str_inputs(
            &rtxn,
            r#"[:find ?n :in [?id ?n] :where [?e :person/id ?id]]"#,
            &[pair],
            "[]",
        )
        .unwrap(),
    );
    assert_eq!(r, vec![vec![s("Ann")]]);

    let rel = QueryInput::Rel(vec![
        vec![Value::I64(1), s("Ann")],
        vec![Value::I64(2), s("Zed")],
    ]);
    let r = rows(
        conn.q_str_inputs(
            &rtxn,
            r#"[:find ?name :in [[?id ?name]] :where [?e :person/id ?id] [?e :person/name ?name]]"#,
            &[rel],
            "[]",
        )
        .unwrap(),
    );
    assert_eq!(r, vec![vec![s("Ann")]]);

    // Shape mismatch is an error, not a silent truncation.
    let err = conn
        .q_str_inputs(
            &rtxn,
            r#"[:find ?n :in [?a ?b] :where [?e :person/name ?n]]"#,
            &[QueryInput::Coll(vec![])],
            "[]",
        )
        .unwrap_err();
    assert!(matches!(err, DatalogError::Parse(_)), "got {err:?}");
}

#[test]
fn prepared_query_accepts_typed_inputs_and_database_source() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let p = conn
        .prepare_query(
            r#"[:find [?n ...] :in $ [?id ...] :where [?e :person/id ?id] [?e :person/name ?n]]"#,
            "[]",
        )
        .unwrap();
    let r = p
        .execute_inputs(&rtxn, &[QueryInput::Coll(vec![Value::I64(2), Value::I64(9)])])
        .unwrap();
    assert_eq!(r, QueryResult::Coll(vec![s("Bob")]));
}

#[test]
fn unknown_function_is_a_parse_error() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let err = conn
        .q_str(&rtxn, r#"[:find ?n :where [?e :person/name ?n] [(frobnicate ?n) ?x]]"#, &[])
        .unwrap_err();
    assert!(matches!(err, DatalogError::Parse(_)), "got {err:?}");
}

#[test]
fn not_join_scopes_inner_variables_to_join_vars() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    // `?a` is not a join var, so inside the not-join it is fresh: any age matches, so nothing survives.
    let r = rows(
        conn.q_str(
            &rtxn,
            r#"[:find ?n :where [?e :person/name ?n] [?e :person/age ?a]
                (not-join [?n] [?p :person/age ?a])]"#,
            &[],
        )
        .unwrap(),
    );
    assert!(r.is_empty(), "got {r:?}");
}

#[test]
fn or_join_keeps_only_outer_and_join_vars() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let mut r = rows(
        conn.q_str(
            &rtxn,
            r#"[:find ?n ?z :where [?e :person/name ?n]
                (or-join [?e ?z] (and [?e :person/nick ?z]) [(ground "x") ?z])]"#,
            &[],
        )
        .unwrap(),
    );
    r.sort_by_key(|row| format!("{row:?}"));
    assert_eq!(r, vec![vec![s("Ann"), s("A")], vec![s("Ann"), s("x")], vec![s("Bob"), s("x")]]);
}
