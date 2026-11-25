use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, QueryInput, QueryResult, TxItem};

const NAMES: [&str; 5] = ["item0", "item1", "item2", "item3", "item4"];

// Vectors at angles 0, 10, 20, 30 and 40 degrees from the query [1, 0]. Cosine distance (the vector index metric)
// grows with the angle, so the nearest-first order is item0, item1, ...
fn conn() -> Conn {
    let conn = Conn::open_mem(vec![AttrSpec::new(":item/name").unique_identity()]).unwrap();
    for (i, name) in NAMES.iter().enumerate() {
        let eids = conn
            .transact(&[TxItem::Assert(None, vec![(":item/name".into(), Value::Str((*name).into()))])])
            .unwrap();
        let theta = (i as f32 * 10.0).to_radians();
        conn.add_vec(eids[0], &[theta.cos(), theta.sin()]).unwrap();
    }
    conn
}

fn query_vec() -> QueryInput {
    QueryInput::Scalar(Value::Tuple(vec![Value::F64(1.0), Value::F64(0.0)]))
}

fn rows(res: QueryResult) -> Vec<Vec<Value>> {
    match res {
        QueryResult::Rel(r) => r,
        other => panic!("expected Rel, got {other:?}"),
    }
}

fn dist(i: usize) -> f64 {
    1.0 - ((i as f64) * 10.0).to_radians().cos()
}

#[test]
fn rel_binding_returns_nearest_names_with_distances() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let q = r#"[:find ?name ?d :in $ ?q :where [(vec-neighbors ?q 3) [[?e ?d]]] [?e :item/name ?name]]"#;
    let mut r = rows(conn.q_str_inputs(&rtxn, q, &[query_vec()], "[]").unwrap());
    let d_of = |row: &Vec<Value>| match row[1] {
        Value::F64(d) => d,
        ref other => panic!("distance not F64: {other:?}"),
    };
    r.sort_by(|a, b| d_of(a).partial_cmp(&d_of(b)).unwrap());
    assert_eq!(r.len(), 3, "{r:?}");
    for (i, row) in r.iter().enumerate() {
        assert_eq!(row[0], Value::Str(NAMES[i].into()));
        match row[1] {
            Value::F64(d) => assert!((d - dist(i)).abs() < 1e-4, "row {i}: {d}"),
            ref other => panic!("distance not F64: {other:?}"),
        }
    }
}

#[test]
fn wildcard_position_is_ignored() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let q = r#"[:find ?name :in $ ?q :where [(vec-neighbors ?q 3) [[?e _]]] [?e :item/name ?name]]"#;
    let r = rows(conn.q_str_inputs(&rtxn, q, &[query_vec()], "[]").unwrap());
    assert_eq!(r.len(), 3, "{r:?}");
}

#[test]
fn bound_entity_keeps_only_matching_rows() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let hit = r#"[:find ?d :in $ ?q :where [?e :item/name "item1"] [(vec-neighbors ?q 2) [[?e ?d]]]]"#;
    assert_eq!(rows(conn.q_str_inputs(&rtxn, hit, &[query_vec()], "[]").unwrap()).len(), 1);
    let miss = r#"[:find ?d :in $ ?q :where [?e :item/name "item4"] [(vec-neighbors ?q 2) [[?e ?d]]]]"#;
    assert!(rows(conn.q_str_inputs(&rtxn, miss, &[query_vec()], "[]").unwrap()).is_empty());
}

#[test]
fn collection_binding_yields_one_row_per_element() {
    let mut conn = conn();
    conn.register_query_fn("triple", |_| Ok(Some(Value::Tuple(vec![Value::I64(1), Value::I64(2), Value::I64(3)]))));
    let rtxn = conn.read_txn().unwrap();
    let r = rows(conn.q_str(&rtxn, r#"[:find ?x :where [(triple) [?x ...]]]"#, &[]).unwrap());
    assert_eq!(r.len(), 3, "{r:?}");
}

#[test]
fn negative_k_is_a_function_error() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let q = r#"[:find ?e :in $ ?q :where [(vec-neighbors ?q -1) [[?e ?d]]]]"#;
    let err = conn.q_str_inputs(&rtxn, q, &[query_vec()], "[]").unwrap_err();
    assert!(matches!(err, DatalogError::Function(_)), "got {err:?}");
}

#[test]
fn non_tuple_query_is_a_function_error() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let q = r#"[:find ?e :in $ ?q :where [(vec-neighbors ?q 2) [[?e ?d]]]]"#;
    let err = conn.q_str_inputs(&rtxn, q, &[QueryInput::Scalar(Value::I64(1))], "[]").unwrap_err();
    assert!(matches!(err, DatalogError::Function(_)), "got {err:?}");
}

#[test]
fn rel_binding_of_wrong_width_is_a_function_error() {
    let conn = conn();
    let rtxn = conn.read_txn().unwrap();
    let q = r#"[:find ?e :in $ ?q :where [(vec-neighbors ?q 2) [[?e ?d ?z]]]]"#;
    let err = conn.q_str_inputs(&rtxn, q, &[query_vec()], "[]").unwrap_err();
    assert!(matches!(err, DatalogError::Function(_)), "got {err:?}");
}
