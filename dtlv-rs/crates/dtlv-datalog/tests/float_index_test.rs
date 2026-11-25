use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, QueryResult, TxItem};

#[test]
fn attribute_index_treats_negative_and_positive_zero_as_one_value() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":f/x").indexed()]).unwrap();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":f/x".into(), Value::F64(-0.0))])]).unwrap()[0];
    let rtxn = conn.read_txn().unwrap();
    // Lookup with +0.0 finds the entity stored as -0.0, as Value equality says it should.
    assert_eq!(conn.lookup_entity_by_av(&rtxn, ":f/x", &Value::F64(0.0)).unwrap(), Some(e));
    assert_eq!(conn.lookup_entity_by_av(&rtxn, ":f/x", &Value::F64(-0.0)).unwrap(), Some(e));
    // Range queries cover both spellings too.
    let r = conn.index_range(&rtxn, ":f/x", &Value::F64(0.0), &Value::F64(0.0)).unwrap();
    assert_eq!(r.len(), 1);
}

#[test]
fn query_on_an_indexed_zero_matches_either_sign() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":f/x").indexed()]).unwrap();
    conn.transact(&[TxItem::Assert(None, vec![(":f/x".into(), Value::F64(-0.0))])]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    let r = conn.q_str(&rtxn, "[:find ?e :where [?e :f/x 0.0]]", &[]).unwrap();
    assert!(matches!(r, QueryResult::Rel(ref rows) if rows.len() == 1), "got {r:?}");
}

#[test]
fn aggregates_count_negative_and_positive_zero_once() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":f/x"), AttrSpec::new(":f/k")]).unwrap();
    conn.transact(&[
        TxItem::Assert(None, vec![(":f/x".into(), Value::F64(-0.0)), (":f/k".into(), Value::I64(1))]),
        TxItem::Assert(None, vec![(":f/x".into(), Value::F64(0.0)), (":f/k".into(), Value::I64(2))]),
    ])
    .unwrap();
    let rtxn = conn.read_txn().unwrap();
    // Set semantics over the find variable: -0.0 and 0.0 are one value, so one distinct value.
    let r = conn.q_str(&rtxn, "[:find (count ?x) . :where [?e :f/k ?k] [?e :f/x ?x]]", &[]).unwrap();
    assert_eq!(r, QueryResult::Scalar(Some(Value::I64(1))));
}

#[test]
fn exact_eav_lookup_matches_either_zero_sign_without_index() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":f/x"), AttrSpec::new(":f/k")]).unwrap();
    let e_neg = conn
        .transact(&[TxItem::Assert(None, vec![(":f/x".into(), Value::F64(-0.0)), (":f/k".into(), Value::I64(1))])])
        .unwrap()[0];
    let e_pos = conn
        .transact(&[TxItem::Assert(None, vec![(":f/x".into(), Value::F64(0.0)), (":f/k".into(), Value::I64(2))])])
        .unwrap()[0];
    let rtxn = conn.read_txn().unwrap();

    // Bound entity + bound 0.0 literal finds the entity stored with -0.0 (and vice versa).
    let r1 = conn.q_str(&rtxn, "[:find ?e :where [?e :f/k 1] [?e :f/x 0.0]]", &[]).unwrap();
    assert_eq!(r1, QueryResult::Rel(vec![vec![Value::Ref(e_neg)]]));

    let r2 = conn.q_str(&rtxn, "[:find ?e :where [?e :f/k 2] [?e :f/x -0.0]]", &[]).unwrap();
    assert_eq!(r2, QueryResult::Rel(vec![vec![Value::Ref(e_pos)]]));
}

#[test]
fn indexed_retraction_of_zero_keeps_eav_and_ave_in_sync() {
    // Cardinality-one indexed attribute: retracting with 0.0 removes the -0.0 datom and its AVE entry.
    let conn = Conn::open_mem(vec![AttrSpec::new(":f/x").indexed()]).unwrap();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":f/x".into(), Value::F64(-0.0))])]).unwrap()[0];
    // Retracting an absent value must not disturb the surviving -0.0 AVE entry.
    conn.transact(&[TxItem::Retract(e, ":f/x".into(), Value::F64(1.0))]).unwrap();
    {
        let rtxn = conn.read_txn().unwrap();
        assert_eq!(conn.lookup_entity_by_av(&rtxn, ":f/x", &Value::F64(-0.0)).unwrap(), Some(e));
        assert_eq!(conn.lookup_entity_by_av(&rtxn, ":f/x", &Value::F64(0.0)).unwrap(), Some(e));
    }
    // Retracting with +0.0 removes both EAV and AVE.
    conn.transact(&[TxItem::Retract(e, ":f/x".into(), Value::F64(0.0))]).unwrap();
    {
        let rtxn = conn.read_txn().unwrap();
        assert_eq!(conn.get_attr(&rtxn, e, ":f/x").unwrap(), None);
        assert_eq!(conn.lookup_entity_by_av(&rtxn, ":f/x", &Value::F64(-0.0)).unwrap(), None);
        assert_eq!(conn.count_datoms(&rtxn, ":f/x", &Value::F64(0.0)).unwrap(), 0);
    }

    // Cardinality-many indexed attribute: asserting -0.0 and 0.0 on one entity is one datom;
    // retracting one removes both EAV and AVE with no orphaned entries.
    let conn_many = Conn::open_mem(vec![AttrSpec::new(":f/m").many().indexed()]).unwrap();
    let e2 = conn_many
        .transact(&[
            TxItem::Assert(None, vec![(":f/m".into(), Value::F64(-0.0))]),
        ])
        .unwrap()[0];
    conn_many
        .transact(&[TxItem::Assert(Some(e2), vec![(":f/m".into(), Value::F64(0.0))])])
        .unwrap();
    {
        let rtxn = conn_many.read_txn().unwrap();
        assert_eq!(conn_many.get_attr_many(&rtxn, e2, ":f/m").unwrap().len(), 1);
    }
    conn_many
        .transact(&[TxItem::Retract(e2, ":f/m".into(), Value::F64(-0.0))])
        .unwrap();
    let rtxn = conn_many.read_txn().unwrap();
    assert!(conn_many.get_attr_many(&rtxn, e2, ":f/m").unwrap().is_empty());
    assert_eq!(conn_many.lookup_entity_by_av(&rtxn, ":f/m", &Value::F64(0.0)).unwrap(), None);
}

#[test]
fn tuples_containing_negative_and_positive_zero_match_in_index_and_hash_join() {
    let conn = Conn::open_mem(vec![
        AttrSpec::new(":t/idx").indexed(),
        AttrSpec::new(":t/a"),
        AttrSpec::new(":t/b"),
    ])
    .unwrap();
    let t_neg = Value::Tuple(vec![Value::F64(-0.0), Value::I64(1)]);
    let t_pos = Value::Tuple(vec![Value::F64(0.0), Value::I64(1)]);
    let t_nan1 = Value::Tuple(vec![Value::F64(f64::NAN), Value::I64(1)]);
    let t_nan2 = Value::Tuple(vec![Value::F64(f64::NAN), Value::I64(1)]);

    let e1 = conn
        .transact(&[TxItem::Assert(
            None,
            vec![(":t/idx".into(), t_neg.clone()), (":t/a".into(), t_neg)],
        )])
        .unwrap()[0];
    let e2 = conn
        .transact(&[TxItem::Assert(None, vec![(":t/b".into(), t_pos.clone())])])
        .unwrap()[0];
    // NaN inside tuples must not match in hash join.
    conn.transact(&[TxItem::Assert(None, vec![(":t/a".into(), t_nan1)])]).unwrap();
    conn.transact(&[TxItem::Assert(None, vec![(":t/b".into(), t_nan2)])]).unwrap();

    let rtxn = conn.read_txn().unwrap();
    // Indexed AVE lookup for Tuple([0.0, 1]) finds Tuple([-0.0, 1]).
    assert_eq!(conn.lookup_entity_by_av(&rtxn, ":t/idx", &t_pos).unwrap(), Some(e1));
    // Hash join on unindexed attributes :t/a and :t/b matches Tuple([-0.0, 1]) with Tuple([0.0, 1]),
    // and excludes Tuple([NaN, 1]).
    let r = conn.q_str(&rtxn, "[:find ?x ?y :where [?x :t/a ?t] [?y :t/b ?t]]", &[]).unwrap();
    assert_eq!(r, QueryResult::Rel(vec![vec![Value::Ref(e1), Value::Ref(e2)]]));

    // Set deduplication and grouping also treat -0.0 and 0.0 (including in tuples) as one value,
    // and indexed NaN lookups follow NaN != NaN.
    let dedup = conn.q_str(&rtxn, "[:find ?v :where [?x :t/a ?v]]", &[]).unwrap();
    let QueryResult::Rel(drows) = dedup else { panic!("expected Rel, got {dedup:?}") };
    assert_eq!(drows.len(), 2);

    let grouped = conn.q_str(&rtxn, "[:find ?t (count ?x) :where [?x :t/idx ?t]]", &[]).unwrap();
    let QueryResult::Rel(grows) = grouped else { panic!("expected Rel, got {grouped:?}") };
    assert_eq!(grows.len(), 1);
    assert_eq!(grows[0][1], Value::I64(1));

    // Exact and indexed NaN lookups never match stored NaN.
    assert_eq!(
        conn.q_str(&rtxn, "[:find ?x :where [?x :t/a ?t] [?x :t/a ?t]]", &[]).unwrap(),
        QueryResult::Rel(vec![vec![Value::Ref(e1)]])
    );
}
