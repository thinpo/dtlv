use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, EntityRef, QueryResult, TxItem, TxVal};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn schema() -> Vec<AttrSpec> {
    vec![
        AttrSpec::new(":person/first"),
        AttrSpec::new(":person/last"),
        AttrSpec::new(":person/full").tuple_of(&[":person/first", ":person/last"]).unique_identity(),
        AttrSpec::new(":counter/n"),
        AttrSpec::new(":counter/id").unique_identity(),
    ]
}

fn eids_where(conn: &Conn, query: &str) -> Vec<Value> {
    let rtxn = conn.read_txn().unwrap();
    match conn.q_str(&rtxn, query, &[]).unwrap() {
        QueryResult::Coll(v) => v,
        other => panic!("expected Coll, got {other:?}"),
    }
}

#[test]
fn tuple_attribute_is_maintained_from_sources() {
    let conn = Conn::open_mem(schema()).unwrap();
    let eids = conn
        .transact(&[TxItem::Assert(
            None,
            vec![(":person/first".into(), s("Ann")), (":person/last".into(), s("Lee"))],
        )])
        .unwrap();
    let e = eids[0];

    let hit = eids_where(&conn, r#"[:find [?e ...] :where [?e :person/full ["Ann" "Lee"]]]"#);
    assert_eq!(hit, vec![Value::Ref(e)]);

    // Changing a source rewrites the tuple; the old tuple no longer matches.
    conn.transact(&[TxItem::Assert(
        Some(e),
        vec![(":person/last".into(), s("Kim"))],
    )])
    .unwrap();
    assert!(eids_where(&conn, r#"[:find [?e ...] :where [?e :person/full ["Ann" "Lee"]]]"#).is_empty());
    assert_eq!(eids_where(&conn, r#"[:find [?e ...] :where [?e :person/full ["Ann" "Kim"]]]"#).len(), 1);

    // Retracting a source removes the tuple.
    conn.transact(&[TxItem::RetractAttribute(e, ":person/first".into())]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    assert_eq!(conn.get_attr(&rtxn, e, ":person/full").unwrap(), None);
}

#[test]
fn tuple_attribute_cannot_be_written_directly() {
    let conn = Conn::open_mem(schema()).unwrap();
    let err = conn
        .transact(&[TxItem::Assert(
            None,
            vec![(":person/full".into(), Value::Tuple(vec![s("A"), s("B")]))],
        )])
        .unwrap_err();
    assert!(matches!(err, DatalogError::TupleWrite(ref a) if a == ":person/full"), "got {err:?}");
}

#[test]
fn tuple_source_must_be_in_schema_and_cardinality_one() {
    let bad_missing = vec![AttrSpec::new(":t/x").tuple_of(&[":t/missing"])];
    assert!(matches!(Conn::open_mem(bad_missing), Err(DatalogError::Schema(_))));

    let bad_many = vec![AttrSpec::new(":t/a").many(), AttrSpec::new(":t/x").tuple_of(&[":t/a"])];
    assert!(matches!(Conn::open_mem(bad_many), Err(DatalogError::Schema(_))));
}

#[test]
fn retract_attribute_removes_all_values() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":tags/t").many()]).unwrap();
    let e = conn
        .transact(&[TxItem::Assert(
            None,
            vec![(":tags/t".into(), s("a")), (":tags/t".into(), s("b"))],
        )])
        .unwrap()[0];
    conn.transact(&[TxItem::RetractAttribute(e, ":tags/t".into())]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    assert_eq!(conn.get_attr(&rtxn, e, ":tags/t").unwrap(), None);
}

#[test]
fn transaction_function_reads_in_flight_state_and_writes_atomically() {
    let mut conn = Conn::open_mem(schema()).unwrap();
    conn.register_tx_fn(":counter/inc", |view, args| {
        let id = match &args[0] {
            Value::Str(s) => s.clone(),
            _ => unreachable!(),
        };
        let eid = view.lookup(":counter/id", &s(&id))?;
        let cur = match eid {
            Some(e) => view.get_attr(e, ":counter/n")?,
            None => None,
        };
        let n = match cur {
            Some(Value::I64(n)) => n,
            _ => 0,
        };
        Ok(vec![TxItem::AssertRef(
            match eid {
                Some(e) => EntityRef::Id(e),
                None => EntityRef::New,
            },
            vec![
                (":counter/id".into(), TxVal::Val(s(&id))),
                (":counter/n".into(), TxVal::Val(Value::I64(n + 1))),
            ],
        )])
    });

    for _ in 0..3 {
        conn.transact(&[TxItem::Call(":counter/inc".into(), vec![s("hits")])]).unwrap();
    }
    let rtxn = conn.read_txn().unwrap();
    let e = conn.lookup_entity_by_av(&rtxn, ":counter/id", &s("hits")).unwrap().unwrap();
    assert_eq!(conn.get_attr(&rtxn, e, ":counter/n").unwrap(), Some(Value::I64(3)));
}

#[test]
fn failing_transaction_function_rolls_back_the_whole_transaction() {
    let mut conn = Conn::open_mem(schema()).unwrap();
    conn.register_tx_fn(":counter/boom", |_view, _args| {
        Err(DatalogError::Function("boom".into()))
    });
    let err = conn
        .transact(&[
            TxItem::Assert(None, vec![(":counter/id".into(), s("before"))]),
            TxItem::Call(":counter/boom".into(), vec![]),
        ])
        .unwrap_err();
    assert!(matches!(err, DatalogError::Function(_)), "got {err:?}");
    let rtxn = conn.read_txn().unwrap();
    assert_eq!(conn.lookup_entity_by_av(&rtxn, ":counter/id", &s("before")).unwrap(), None);
}

#[test]
fn unknown_transaction_function_is_an_error() {
    let conn = Conn::open_mem(schema()).unwrap();
    let err = conn
        .transact(&[TxItem::Call(":nope".into(), vec![])])
        .unwrap_err();
    assert!(matches!(err, DatalogError::UnknownTxFn(_)), "got {err:?}");
}
