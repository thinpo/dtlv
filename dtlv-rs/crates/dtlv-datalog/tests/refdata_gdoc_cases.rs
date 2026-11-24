use std::time::Instant;
use dtlv_codec::Value;
use dtlv_datalog::parser::parse_iso8601_utc_ms;
use dtlv_datalog::{
    AttrSpec, Conn, EntityRef, PullSelector, PullValue, QueryResult, TxItem, TxVal,
};
use tempfile::tempdir;

fn gdoc_schema() -> Vec<AttrSpec> {
    vec![
        // Instrument master
        AttrSpec::new(":instrument/id").unique_identity(),
        AttrSpec::new(":instrument/perm-id").unique_identity(),
        AttrSpec::new(":instrument/isin").unique_value(),
        AttrSpec::new(":instrument/asset-class").indexed(),
        AttrSpec::new(":instrument/currency").indexed(),
        AttrSpec::new(":instrument/lifecycle-state").indexed(),
        AttrSpec::new(":instrument/name").fulltext(),
        AttrSpec::new(":instrument/issuer").indexed(),
        AttrSpec::new(":instrument/underlying").indexed(),
        AttrSpec::new(":instrument/current-version"),
        // Bitemporal version
        AttrSpec::new(":ver/instrument").indexed(),
        AttrSpec::new(":ver/status"),
        AttrSpec::new(":ver/lot-size"),
        AttrSpec::new(":ver/tick-size"),
        AttrSpec::new(":ver/ipo-price"),
        AttrSpec::new(":ver/valid-from"),
        AttrSpec::new(":ver/valid-to"),
        AttrSpec::new(":ver/tx-from"),
        AttrSpec::new(":ver/tx-to"),
        // IdMapping (no unique constraint on :id-map/value to allow symbol recycling)
        AttrSpec::new(":id-map/instrument").indexed(),
        AttrSpec::new(":id-map/scheme").indexed(),
        AttrSpec::new(":id-map/mic").indexed(),
        AttrSpec::new(":id-map/value").indexed(),
        AttrSpec::new(":id-map/valid-from"),
        AttrSpec::new(":id-map/valid-to"),
        // Listings
        AttrSpec::new(":listing/key").unique_identity(),
        AttrSpec::new(":listing/instrument").indexed(),
        AttrSpec::new(":listing/mic").indexed(),
        AttrSpec::new(":listing/ticker").indexed(),
        AttrSpec::new(":listing/lot-size"),
        AttrSpec::new(":listing/tick-size"),
        // Entity hierarchy
        AttrSpec::new(":entity/lei").unique_identity(),
        AttrSpec::new(":entity/name").fulltext(),
        AttrSpec::new(":entity/parent").indexed(),
        AttrSpec::new(":entity/sector"),
        // Corporate Actions
        AttrSpec::new(":ca/id").unique_identity(),
        AttrSpec::new(":ca/type"),
        AttrSpec::new(":ca/effective-date"),
        AttrSpec::new(":ca/source-instrument").indexed(),
        AttrSpec::new(":ca/target-instrument").indexed(),
        AttrSpec::new(":ca/exchange-ratio"),
        // Vendor Claims
        AttrSpec::new(":claim/key").unique_identity(),
        AttrSpec::new(":claim/vendor").indexed(),
        AttrSpec::new(":claim/scheme").indexed(),
        AttrSpec::new(":claim/value").indexed(),
        AttrSpec::new(":claim/updated-at"),
        AttrSpec::new(":claim/instrument").indexed(),
    ]
}

fn inst(s: &str) -> Value {
    Value::Instant(parse_iso8601_utc_ms(s).unwrap())
}

#[test]
fn test_3_2_equity_penetration_and_restricted_list() {
    let dir = tempdir().unwrap();
    let conn = Conn::open(dir.path(), gdoc_schema()).unwrap();

    conn.transact(&[
        TxItem::AssertRef(
            EntityRef::TempId("root".into()),
            vec![
                (":entity/lei".into(), TxVal::Val(Value::Str("5493001KJTIIGC8Y1R12".into()))),
                (":entity/name".into(), TxVal::Val(Value::Str("Alpha Group Holding".into()))),
            ],
        ),
        TxItem::AssertRef(
            EntityRef::TempId("sub".into()),
            vec![
                (":entity/lei".into(), TxVal::Val(Value::Str("LEI-SUB-1".into()))),
                (":entity/name".into(), TxVal::Val(Value::Str("Alpha Capital Ltd".into()))),
                (":entity/parent".into(), TxVal::TempRef("root".into())),
            ],
        ),
        TxItem::AssertRef(
            EntityRef::TempId("eq".into()),
            vec![
                (":instrument/perm-id".into(), TxVal::Val(Value::Str("PERM-ALPH".into()))),
                (":instrument/asset-class".into(), TxVal::Val(Value::Keyword(":equity".into()))),
                (":instrument/issuer".into(), TxVal::TempRef("sub".into())),
            ],
        ),
        TxItem::AssertRef(
            EntityRef::TempId("opt".into()),
            vec![
                (":instrument/perm-id".into(), TxVal::Val(Value::Str("PERM-ALPH-C100".into()))),
                (":instrument/asset-class".into(), TxVal::Val(Value::Keyword(":option".into()))),
                (":instrument/underlying".into(), TxVal::TempRef("eq".into())),
            ],
        ),
        TxItem::AssertRef(
            EntityRef::New,
            vec![
                (":listing/key".into(), TxVal::Val(Value::Str("XNAS:ALPH".into()))),
                (":listing/instrument".into(), TxVal::TempRef("eq".into())),
                (":listing/mic".into(), TxVal::Val(Value::Keyword(":XNAS".into()))),
                (":listing/ticker".into(), TxVal::Val(Value::Str("ALPH".into()))),
            ],
        ),
        TxItem::AssertRef(
            EntityRef::New,
            vec![
                (":listing/key".into(), TxVal::Val(Value::Str("XCBO:ALPH26C100".into()))),
                (":listing/instrument".into(), TxVal::TempRef("opt".into())),
                (":listing/mic".into(), TxVal::Val(Value::Keyword(":XCBO".into()))),
                (":listing/ticker".into(), TxVal::Val(Value::Str("ALPH26C100".into()))),
            ],
        ),
    ])
    .unwrap();

    let rules = r#"[
        [(descendant ?root ?child)
         [?child :entity/parent ?root]]
        [(descendant ?root ?child)
         [?mid :entity/parent ?root]
         (descendant ?mid ?child)]
        [(exposed-to-entity ?inst ?ent)
         [?inst :instrument/issuer ?ent]]
        [(exposed-to-entity ?inst ?ent)
         [?inst :instrument/underlying ?u]
         (exposed-to-entity ?u ?ent)]
    ]"#;

    let rtxn = conn.read_txn().unwrap();
    let res = conn
        .q_str_with_rules(
            &rtxn,
            r#"[:find ?mic ?ticker ?class
                :in $ % ?restricted-lei
                :where
                [?root :entity/lei ?restricted-lei]
                (or [(= ?ent ?root)]
                    (descendant ?root ?ent))
                (exposed-to-entity ?inst ?ent)
                [?inst :instrument/asset-class ?class]
                [?listing :listing/instrument ?inst]
                [?listing :listing/mic ?mic]
                [?listing :listing/ticker ?ticker]]"#,
            &[Value::Str("5493001KJTIIGC8Y1R12".into())],
            rules,
        )
        .unwrap();

    if let QueryResult::Rel(rows) = res {
        assert_eq!(rows.len(), 2);
        assert!(rows.contains(&vec![
            Value::Keyword(":XNAS".into()),
            Value::Str("ALPH".into()),
            Value::Keyword(":equity".into())
        ]));
        assert!(rows.contains(&vec![
            Value::Keyword(":XCBO".into()),
            Value::Str("ALPH26C100".into()),
            Value::Keyword(":option".into())
        ]));
    } else {
        panic!("expected Rel");
    }
}

#[test]
fn test_4_1_ipo_when_issued_to_active() {
    let dir = tempdir().unwrap();
    let conn = Conn::open(dir.path(), gdoc_schema()).unwrap();

    // 1. Pre-IPO / When-Issued
    let ids = conn
        .transact(&[
            TxItem::AssertRef(
                EntityRef::TempId("inst-new".into()),
                vec![
                    (":instrument/perm-id".into(), TxVal::Val(Value::Str("PERM-2026-9901".into()))),
                    (":instrument/lifecycle-state".into(), TxVal::Val(Value::Keyword(":when-issued".into()))),
                    (":instrument/asset-class".into(), TxVal::Val(Value::Keyword(":equity".into()))),
                    (":instrument/name".into(), TxVal::Val(Value::Str("NovaAI Holdings Inc.".into()))),
                    (":instrument/current-version".into(), TxVal::TempRef("ver-wi".into())),
                ],
            ),
            TxItem::AssertRef(
                EntityRef::TempId("ver-wi".into()),
                vec![
                    (":ver/instrument".into(), TxVal::TempRef("inst-new".into())),
                    (":ver/status".into(), TxVal::Val(Value::Keyword(":when-issued".into()))),
                    (":ver/lot-size".into(), TxVal::Val(Value::I64(1))),
                    (":ver/valid-from".into(), TxVal::Val(inst("2026-11-19T00:00:00Z"))),
                    (":ver/valid-to".into(), TxVal::Val(inst("9999-12-31T23:59:59Z"))),
                ],
            ),
            TxItem::AssertRef(
                EntityRef::TempId("map-wi".into()),
                vec![
                    (":id-map/instrument".into(), TxVal::TempRef("inst-new".into())),
                    (":id-map/scheme".into(), TxVal::Val(Value::Keyword(":exchange-ticker".into()))),
                    (":id-map/mic".into(), TxVal::Val(Value::Keyword(":XNAS".into()))),
                    (":id-map/value".into(), TxVal::Val(Value::Str("NVAI.WI".into()))),
                    (":id-map/valid-from".into(), TxVal::Val(inst("2026-11-19T00:00:00Z"))),
                    (":id-map/valid-to".into(), TxVal::Val(inst("9999-12-31T23:59:59Z"))),
                ],
            ),
        ])
        .unwrap();
    let old_ver_eid = ids[1];
    let old_map_eid = ids[2];

    // 2. Listing Day atomic switch
    conn.transact(&[
        TxItem::Assert(
            Some(old_ver_eid),
            vec![(":ver/valid-to".into(), inst("2026-11-24T00:00:00Z"))],
        ),
        TxItem::Assert(
            Some(old_map_eid),
            vec![(":id-map/valid-to".into(), inst("2026-11-24T00:00:00Z"))],
        ),
        TxItem::AssertRef(
            EntityRef::LookupRef(":instrument/perm-id".into(), Value::Str("PERM-2026-9901".into())),
            vec![
                (":instrument/lifecycle-state".into(), TxVal::Val(Value::Keyword(":active".into()))),
                (":instrument/current-version".into(), TxVal::TempRef("ver-live".into())),
            ],
        ),
        TxItem::AssertRef(
            EntityRef::TempId("ver-live".into()),
            vec![
                (":ver/instrument".into(), TxVal::LookupRef(":instrument/perm-id".into(), Value::Str("PERM-2026-9901".into()))),
                (":ver/status".into(), TxVal::Val(Value::Keyword(":active".into()))),
                (":ver/lot-size".into(), TxVal::Val(Value::I64(100))),
                (":ver/tick-size".into(), TxVal::Val(Value::F64(0.01))),
                (":ver/ipo-price".into(), TxVal::Val(Value::F64(28.50))),
                (":ver/valid-from".into(), TxVal::Val(inst("2026-11-24T00:00:00Z"))),
                (":ver/valid-to".into(), TxVal::Val(inst("9999-12-31T23:59:59Z"))),
            ],
        ),
        TxItem::AssertRef(
            EntityRef::New,
            vec![
                (":id-map/instrument".into(), TxVal::LookupRef(":instrument/perm-id".into(), Value::Str("PERM-2026-9901".into()))),
                (":id-map/scheme".into(), TxVal::Val(Value::Keyword(":exchange-ticker".into()))),
                (":id-map/mic".into(), TxVal::Val(Value::Keyword(":XNAS".into()))),
                (":id-map/value".into(), TxVal::Val(Value::Str("NVAI".into()))),
                (":id-map/valid-from".into(), TxVal::Val(inst("2026-11-24T00:00:00Z"))),
                (":id-map/valid-to".into(), TxVal::Val(inst("9999-12-31T23:59:59Z"))),
            ],
        ),
    ])
    .unwrap();

    let rtxn = conn.read_txn().unwrap();
    let pulled = conn
        .prepare_pull(vec![
            PullSelector::Attr(":instrument/lifecycle-state".into()),
            PullSelector::Nested(
                ":instrument/current-version".into(),
                vec![
                    PullSelector::Attr(":ver/lot-size".into()),
                    PullSelector::Attr(":ver/ipo-price".into()),
                ],
            ),
        ])
        .execute_by_lookup(&rtxn, ":instrument/perm-id", &Value::Str("PERM-2026-9901".into()))
        .unwrap()
        .unwrap();
    assert_eq!(
        pulled.get(":instrument/lifecycle-state"),
        Some(&PullValue::Single(Value::Keyword(":active".into())))
    );
    assert!(matches!(pulled.get(":instrument/current-version"), Some(PullValue::Map(_))));
}

#[test]
fn test_4_2_ticker_change_and_symbol_recycling() {
    let dir = tempdir().unwrap();
    let conn = Conn::open(dir.path(), gdoc_schema()).unwrap();

    let ids = conn
        .transact(&[
            TxItem::AssertRef(
                EntityRef::TempId("meta".into()),
                vec![
                    (":instrument/perm-id".into(), TxVal::Val(Value::Str("PERM-META".into()))),
                    (":instrument/name".into(), TxVal::Val(Value::Str("Meta Platforms, Inc.".into()))),
                ],
            ),
            TxItem::AssertRef(
                EntityRef::TempId("fb-map".into()),
                vec![
                    (":id-map/instrument".into(), TxVal::TempRef("meta".into())),
                    (":id-map/scheme".into(), TxVal::Val(Value::Keyword(":exchange-ticker".into()))),
                    (":id-map/mic".into(), TxVal::Val(Value::Keyword(":XNAS".into()))),
                    (":id-map/value".into(), TxVal::Val(Value::Str("FB".into()))),
                    (":id-map/valid-from".into(), TxVal::Val(inst("2012-05-18T00:00:00Z"))),
                    (":id-map/valid-to".into(), TxVal::Val(inst("9999-12-31T23:59:59Z"))),
                ],
            ),
        ])
        .unwrap();
    let meta_fb_map_eid = ids[1];

    // Rename FB -> META on 2022-06-09
    conn.transact(&[
        TxItem::Assert(
            Some(meta_fb_map_eid),
            vec![(":id-map/valid-to".into(), inst("2022-06-09T00:00:00Z"))],
        ),
        TxItem::AssertRef(
            EntityRef::New,
            vec![
                (":id-map/instrument".into(), TxVal::LookupRef(":instrument/perm-id".into(), Value::Str("PERM-META".into()))),
                (":id-map/scheme".into(), TxVal::Val(Value::Keyword(":exchange-ticker".into()))),
                (":id-map/mic".into(), TxVal::Val(Value::Keyword(":XNAS".into()))),
                (":id-map/value".into(), TxVal::Val(Value::Str("META".into()))),
                (":id-map/valid-from".into(), TxVal::Val(inst("2022-06-09T00:00:00Z"))),
                (":id-map/valid-to".into(), TxVal::Val(inst("9999-12-31T23:59:59Z"))),
            ],
        ),
    ])
    .unwrap();

    // Symbol Recycling: new ETF reuses FB on 2023-01-15
    conn.transact(&[
        TxItem::AssertRef(
            EntityRef::TempId("new-etf".into()),
            vec![
                (":instrument/perm-id".into(), TxVal::Val(Value::Str("PERM-NEW-ETF".into()))),
                (":instrument/name".into(), TxVal::Val(Value::Str("Fintech Basket ETF".into()))),
            ],
        ),
        TxItem::AssertRef(
            EntityRef::New,
            vec![
                (":id-map/instrument".into(), TxVal::TempRef("new-etf".into())),
                (":id-map/scheme".into(), TxVal::Val(Value::Keyword(":exchange-ticker".into()))),
                (":id-map/mic".into(), TxVal::Val(Value::Keyword(":XNAS".into()))),
                (":id-map/value".into(), TxVal::Val(Value::Str("FB".into()))),
                (":id-map/valid-from".into(), TxVal::Val(inst("2023-01-15T00:00:00Z"))),
                (":id-map/valid-to".into(), TxVal::Val(inst("9999-12-31T23:59:59Z"))),
            ],
        ),
    ])
    .unwrap();

    let rtxn = conn.read_txn().unwrap();
    let pit_q = conn
        .prepare_query(
            r#"[:find [?perm-id ?name]
                :in $ ?mic ?ticker ?as-of
                :where
                [?m :id-map/scheme :exchange-ticker]
                [?m :id-map/mic ?mic]
                [?m :id-map/value ?ticker]
                [?m :id-map/valid-from ?vf]
                [?m :id-map/valid-to ?vt]
                [(<= ?vf ?as-of)]
                [(< ?as-of ?vt)]
                [?m :id-map/instrument ?inst]
                [?inst :instrument/perm-id ?perm-id]
                [?inst :instrument/name ?name]]"#,
            "[]",
        )
        .unwrap();

    let r1 = pit_q
        .execute(
            &rtxn,
            &[
                Value::Keyword(":XNAS".into()),
                Value::Str("FB".into()),
                inst("2022-05-01T12:00:00Z"),
            ],
        )
        .unwrap();
    assert_eq!(
        r1,
        QueryResult::Tuple(Some(vec![
            Value::Str("PERM-META".into()),
            Value::Str("Meta Platforms, Inc.".into())
        ]))
    );

    let r2 = pit_q
        .execute(
            &rtxn,
            &[
                Value::Keyword(":XNAS".into()),
                Value::Str("FB".into()),
                inst("2023-02-01T12:00:00Z"),
            ],
        )
        .unwrap();
    assert_eq!(
        r2,
        QueryResult::Tuple(Some(vec![
            Value::Str("PERM-NEW-ETF".into()),
            Value::Str("Fintech Basket ETF".into())
        ]))
    );
}

#[test]
fn test_4_3_ma_lineage_and_4_4_vendor_reconciliation() {
    let dir = tempdir().unwrap();
    let conn = Conn::open(dir.path(), gdoc_schema()).unwrap();

    // Setup M&A chain: C -> B -> A (active) + Vendor claims for PERM-1001
    conn.transact(&[
        TxItem::AssertRef(
            EntityRef::TempId("a".into()),
            vec![
                (":instrument/perm-id".into(), TxVal::Val(Value::Str("PERM-A".into()))),
                (":instrument/lifecycle-state".into(), TxVal::Val(Value::Keyword(":active".into()))),
            ],
        ),
        TxItem::AssertRef(
            EntityRef::TempId("b".into()),
            vec![
                (":instrument/perm-id".into(), TxVal::Val(Value::Str("PERM-B".into()))),
                (":instrument/lifecycle-state".into(), TxVal::Val(Value::Keyword(":delisted".into()))),
            ],
        ),
        TxItem::AssertRef(
            EntityRef::TempId("c".into()),
            vec![
                (":instrument/perm-id".into(), TxVal::Val(Value::Str("PERM-C".into()))),
                (":instrument/lifecycle-state".into(), TxVal::Val(Value::Keyword(":delisted".into()))),
            ],
        ),
        TxItem::AssertRef(
            EntityRef::New,
            vec![
                (":ca/id".into(), TxVal::Val(Value::Str("CA-1".into()))),
                (":ca/source-instrument".into(), TxVal::TempRef("c".into())),
                (":ca/target-instrument".into(), TxVal::TempRef("b".into())),
            ],
        ),
        TxItem::AssertRef(
            EntityRef::New,
            vec![
                (":ca/id".into(), TxVal::Val(Value::Str("CA-2".into()))),
                (":ca/source-instrument".into(), TxVal::TempRef("b".into())),
                (":ca/target-instrument".into(), TxVal::TempRef("a".into())),
            ],
        ),
        // Vendor claims (4.4)
        TxItem::AssertRef(
            EntityRef::TempId("p1001".into()),
            vec![(":instrument/perm-id".into(), TxVal::Val(Value::Str("PERM-1001".into())))],
        ),
        TxItem::AssertRef(
            EntityRef::New,
            vec![
                (":claim/key".into(), TxVal::Val(Value::Str("exchange:ticker:PERM-1001".into()))),
                (":claim/vendor".into(), TxVal::Val(Value::Keyword(":exchange".into()))),
                (":claim/scheme".into(), TxVal::Val(Value::Keyword(":ticker".into()))),
                (":claim/value".into(), TxVal::Val(Value::Str("NEWX".into()))),
                (":claim/instrument".into(), TxVal::TempRef("p1001".into())),
            ],
        ),
        TxItem::AssertRef(
            EntityRef::New,
            vec![
                (":claim/key".into(), TxVal::Val(Value::Str("bloomberg:ticker:PERM-1001".into()))),
                (":claim/vendor".into(), TxVal::Val(Value::Keyword(":bloomberg".into()))),
                (":claim/scheme".into(), TxVal::Val(Value::Keyword(":ticker".into()))),
                (":claim/value".into(), TxVal::Val(Value::Str("NEWX".into()))),
                (":claim/instrument".into(), TxVal::TempRef("p1001".into())),
            ],
        ),
        TxItem::AssertRef(
            EntityRef::New,
            vec![
                (":claim/key".into(), TxVal::Val(Value::Str("refinitiv:ticker:PERM-1001".into()))),
                (":claim/vendor".into(), TxVal::Val(Value::Keyword(":refinitiv".into()))),
                (":claim/scheme".into(), TxVal::Val(Value::Keyword(":ticker".into()))),
                (":claim/value".into(), TxVal::Val(Value::Str("OLDX".into()))),
                (":claim/instrument".into(), TxVal::TempRef("p1001".into())),
            ],
        ),
    ])
    .unwrap();

    let lineage_rules = r#"[
        [(successor ?old ?new ?ca)
         [?ca :ca/source-instrument ?old]
         [?ca :ca/target-instrument ?new]]
        [(successor ?old ?final ?ca)
         [?step :ca/source-instrument ?old]
         [?step :ca/target-instrument ?mid]
         (successor ?mid ?final ?ca)]
    ]"#;

    let rtxn = conn.read_txn().unwrap();

    // 4.3: Multi-hop M&A lineage from PERM-C -> PERM-A
    let lin_res = conn
        .q_str_with_rules(
            &rtxn,
            r#"[:find ?final-perm-id ?state
                :in $ % ?old-perm-id
                :where
                [?old :instrument/perm-id ?old-perm-id]
                (successor ?old ?final _)
                [?final :instrument/perm-id ?final-perm-id]
                [?final :instrument/lifecycle-state ?state]
                [(= ?state :active)]]"#,
            &[Value::Str("PERM-C".into())],
            lineage_rules,
        )
        .unwrap();
    assert_eq!(
        lin_res,
        QueryResult::Rel(vec![vec![
            Value::Str("PERM-A".into()),
            Value::Keyword(":active".into())
        ]])
    );

    // 4.4: Cross-vendor ticker discrepancy detection
    let disc = conn
        .q_str(
            &rtxn,
            r#"[:find ?perm-id ?v1 ?val1 ?v2 ?val2
                :where
                [?c1 :claim/instrument ?inst]
                [?c1 :claim/scheme :ticker]
                [?c1 :claim/vendor ?v1]
                [?c1 :claim/value ?val1]
                [?c2 :claim/instrument ?inst]
                [?c2 :claim/scheme :ticker]
                [?c2 :claim/vendor ?v2]
                [?c2 :claim/value ?val2]
                [(< ?v1 ?v2)]
                [(not= ?val1 ?val2)]
                [?inst :instrument/perm-id ?perm-id]]"#,
            &[],
        )
        .unwrap();
    if let QueryResult::Rel(rows) = disc {
        assert_eq!(rows.len(), 2);
        assert!(rows.contains(&vec![
            Value::Str("PERM-1001".into()),
            Value::Keyword(":bloomberg".into()),
            Value::Str("NEWX".into()),
            Value::Keyword(":refinitiv".into()),
            Value::Str("OLDX".into()),
        ]));
        assert!(rows.contains(&vec![
            Value::Str("PERM-1001".into()),
            Value::Keyword(":exchange".into()),
            Value::Str("NEWX".into()),
            Value::Keyword(":refinitiv".into()),
            Value::Str("OLDX".into()),
        ]));
    } else {
        panic!("expected Rel");
    }

    // 5.2: PreparedQuery hot-path latency check (< 100 us)
    let pq = conn
        .prepare_query(
            r#"[:find ?perm-id . :in $ ?k :where [?c :claim/key ?k] [?c :claim/instrument ?i] [?i :instrument/perm-id ?perm-id]]"#,
            "[]",
        )
        .unwrap();
    let t0 = Instant::now();
    for _ in 0..500 {
        let out = pq
            .execute(&rtxn, &[Value::Str("bloomberg:ticker:PERM-1001".into())])
            .unwrap();
        assert_eq!(out, QueryResult::Scalar(Some(Value::Str("PERM-1001".into()))));
    }
    let avg_us = t0.elapsed().as_nanos() as f64 / 500.0 / 1000.0;
    assert!(avg_us < 250.0, "PreparedQuery avg_us too high: {}", avg_us);
}
