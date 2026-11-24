use dtlv_codec::Value;
use dtlv_datalog::{
    AttrSpec, Conn, EntityRef, IndexKind, PullSelector, PullValue, QueryResult, TxItem, TxVal,
};
use tempfile::tempdir;

fn schema() -> Vec<AttrSpec> {
    vec![
        AttrSpec::new(":sec/id").unique_identity(),
        AttrSpec::new(":sec/ticker").unique_identity(),
        AttrSpec::new(":sec/name").fulltext(),
        AttrSpec::new(":sec/price").indexed(),
        AttrSpec::new(":sec/parent").indexed(),
        AttrSpec::new(":sec/venue").component(),
        AttrSpec::new(":venue/mic").unique_value(),
    ]
}

#[test]
fn sqlite_backend_full_datalog_fts_vector_and_reopen() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("refdata.sqlite");

    let sec1_eid;
    {
        let conn = Conn::open_sqlite(&db_path, schema()).unwrap();
        let ids = conn
            .transact(&[
                TxItem::AssertRef(
                    EntityRef::TempId("v1".into()),
                    vec![(":venue/mic".into(), TxVal::Val(Value::Str("XNAS".into())))],
                ),
                TxItem::AssertRef(
                    EntityRef::TempId("s1".into()),
                    vec![
                        (":sec/id".into(), TxVal::Val(Value::Str("SEC-1".into()))),
                        (":sec/ticker".into(), TxVal::Val(Value::Str("AAPL".into()))),
                        (":sec/name".into(), TxVal::Val(Value::Str("Apple Inc.".into()))),
                        (":sec/price".into(), TxVal::Val(Value::F64(185.0))),
                        (":sec/venue".into(), TxVal::TempRef("v1".into())),
                    ],
                ),
                TxItem::AssertRef(
                    EntityRef::TempId("s2".into()),
                    vec![
                        (":sec/id".into(), TxVal::Val(Value::Str("SEC-2".into()))),
                        (":sec/ticker".into(), TxVal::Val(Value::Str("SUB1".into()))),
                        (":sec/name".into(), TxVal::Val(Value::Str("Apple Subsidiary".into()))),
                        (":sec/price".into(), TxVal::Val(Value::F64(42.0))),
                        (":sec/parent".into(), TxVal::TempRef("s1".into())),
                    ],
                ),
            ])
            .unwrap();
        sec1_eid = ids[1];
        conn.add_vec(sec1_eid, &[1.0, 0.0, 0.0]).unwrap();
        conn.sync().unwrap();
    }

    // Reopen from the single .sqlite file and verify all data, FTS, vector, and recursive rules
    let conn2 = Conn::open_sqlite(&db_path, schema()).unwrap();
    let rtxn = conn2.read_txn().unwrap();

    assert_eq!(
        conn2.entid(&rtxn, ":sec/ticker", &Value::Str("AAPL".into())).unwrap(),
        Some(sec1_eid)
    );
    assert_eq!(
        conn2.index_range(&rtxn, ":sec/price", &Value::F64(40.0), &Value::F64(200.0)).unwrap().len(),
        2
    );

    let pulled = conn2
        .pull_pattern(
            &rtxn,
            sec1_eid,
            &[
                PullSelector::Attr(":sec/ticker".into()),
                PullSelector::Nested(":sec/venue".into(), vec![PullSelector::All]),
            ],
        )
        .unwrap();
    assert_eq!(
        pulled.get(":sec/ticker"),
        Some(&PullValue::Single(Value::Str("AAPL".into())))
    );
    assert!(matches!(pulled.get(":sec/venue"), Some(PullValue::Map(_))));

    let fts_hits = conn2.fulltext_search(&rtxn, "subsidiary", 5).unwrap();
    assert_eq!(fts_hits.len(), 1);

    let v_hits = conn2.search_vec(&rtxn, &[0.99, 0.01, 0.0], 1).unwrap();
    assert_eq!(v_hits[0].0, sec1_eid);

    let rules = r#"[
        [(desc ?a ?b) [?b :sec/parent ?a]]
        [(desc ?a ?b) [?m :sec/parent ?a] (desc ?m ?b)]
    ]"#;
    let res = conn2
        .q_str_with_rules(
            &rtxn,
            r#"[:find ?t . :in $ % :where [?r :sec/ticker "AAPL"] (desc ?r ?c) [?c :sec/ticker ?t]]"#,
            &[],
            rules,
        )
        .unwrap();
    assert_eq!(res, QueryResult::Scalar(Some(Value::Str("SUB1".into()))));
}

#[test]
fn lmdb_to_sqlite_snapshot_and_restore() {
    let dir = tempdir().unwrap();
    let lmdb_src = dir.path().join("lmdb_src");
    let sqlite_snap = dir.path().join("snapshot.sqlite");
    let lmdb_dst = dir.path().join("lmdb_dst");

    let eid1;
    {
        let conn = Conn::open(&lmdb_src, schema()).unwrap();
        let ids = conn
            .transact(&[TxItem::Assert(
                None,
                vec![
                    (":sec/id".into(), Value::Str("SEC-1".into())),
                    (":sec/ticker".into(), Value::Str("NVDA".into())),
                    (":sec/name".into(), Value::Str("NVIDIA Corporation".into())),
                    (":sec/price".into(), Value::F64(140.0)),
                ],
            )])
            .unwrap();
        eid1 = ids[0];
        conn.add_vec(eid1, &[0.0, 1.0, 0.0]).unwrap();
        let rtxn = conn.read_txn().unwrap();
        conn.save_to_sqlite(&rtxn, &sqlite_snap).unwrap();
    }

    // Restore from SQLite snapshot into a fresh LMDB directory
    let restored = Conn::load_from_sqlite(&lmdb_dst, &sqlite_snap, schema()).unwrap();
    let rtxn = restored.read_txn().unwrap();
    assert_eq!(
        restored.entid(&rtxn, ":sec/ticker", &Value::Str("NVDA".into())).unwrap(),
        Some(eid1)
    );
    assert!(!restored.datoms(&rtxn, IndexKind::Eavt).unwrap().is_empty());
    assert_eq!(restored.fulltext_search(&rtxn, "nvidia", 5).unwrap().len(), 1);
    assert_eq!(restored.search_vec(&rtxn, &[0.0, 1.0, 0.0], 1).unwrap()[0].0, eid1);
}

#[test]
fn redb_persistence_and_mem_ephemeral_backends() {
    let dir = tempdir().unwrap();
    let redb_path = dir.path().join("refdata.redb");

    let eid1;
    {
        let conn = Conn::open_redb(&redb_path, schema()).unwrap();
        let ids = conn
            .transact(&[TxItem::Assert(
                None,
                vec![
                    (":sec/id".into(), Value::Str("SEC-10".into())),
                    (":sec/ticker".into(), Value::Str("TSLA".into())),
                    (":sec/name".into(), Value::Str("Tesla Inc.".into())),
                    (":sec/price".into(), Value::F64(250.0)),
                ],
            )])
            .unwrap();
        eid1 = ids[0];
        conn.add_vec(eid1, &[0.5, 0.5, 0.0]).unwrap();
    }

    // Reopen .redb file and verify persistence
    let conn2 = Conn::open_redb(&redb_path, schema()).unwrap();
    let rtxn2 = conn2.read_txn().unwrap();
    assert_eq!(
        conn2.entid(&rtxn2, ":sec/ticker", &Value::Str("TSLA".into())).unwrap(),
        Some(eid1)
    );
    assert_eq!(conn2.fulltext_search(&rtxn2, "tesla", 5).unwrap().len(), 1);
    assert_eq!(conn2.search_vec(&rtxn2, &[0.5, 0.5, 0.0], 1).unwrap()[0].0, eid1);

    // Ephemeral in-memory BTreeMap backend
    let mem = Conn::open_mem(schema()).unwrap();
    mem.transact(&[TxItem::Assert(
        None,
        vec![
            (":sec/id".into(), Value::Str("SEC-20".into())),
            (":sec/ticker".into(), Value::Str("META".into())),
            (":sec/name".into(), Value::Str("Meta Platforms".into())),
            (":sec/price".into(), Value::F64(580.0)),
        ],
    )])
    .unwrap();
    let mrtxn = mem.read_txn().unwrap();
    let qres = mem
        .q_str(
            &mrtxn,
            r#"[:find ?p . :where [?e :sec/ticker "META"] [?e :sec/price ?p]]"#,
            &[],
        )
        .unwrap();
    assert_eq!(qres, QueryResult::Scalar(Some(Value::F64(580.0))));
}
