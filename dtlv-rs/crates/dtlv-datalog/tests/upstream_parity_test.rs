use std::collections::BTreeMap;
use dtlv_codec::Value;
use dtlv_datalog::{
    AttrSpec, Conn, DatalogError, PullValue, QueryResult, TxItem,
};
use tempfile::tempdir;

#[test]
fn upstream_order_limit_offset_and_pull_in_find() {
    let dir = tempdir().unwrap();
    let schema = vec![
        AttrSpec::new(":person/id").unique_identity(),
        AttrSpec::new(":person/name").indexed(),
    ];
    let conn = Conn::open(dir.path(), schema).unwrap();
    conn.transact(&[
        TxItem::Assert(None, vec![(":person/id".into(), Value::I64(1)), (":person/name".into(), Value::Str("Zoe".into()))]),
        TxItem::Assert(None, vec![(":person/id".into(), Value::I64(2)), (":person/name".into(), Value::Str("Amy".into()))]),
        TxItem::Assert(None, vec![(":person/id".into(), Value::I64(3)), (":person/name".into(), Value::Str("Bob".into()))]),
        TxItem::Assert(None, vec![(":person/id".into(), Value::I64(4)), (":person/name".into(), Value::Str("Dan".into()))]),
    ])
    .unwrap();

    let rtxn = conn.read_txn().unwrap();

    // Map-form query with :order-by :asc, :limit, :offset
    let res = conn
        .q_str(
            &rtxn,
            r#"{:find [?name ...]
                :where [[?e :person/id ?id]
                        [?e :person/name ?name]]
                :order-by [[?id :asc]]
                :limit 2
                :offset 1}"#,
            &[],
        )
        .unwrap();
    assert_eq!(
        res,
        QueryResult::Coll(vec![Value::Str("Amy".into()), Value::Str("Bob".into())])
    );

    // :order-by :desc with (pull ?e [:person/name]) in :find
    let pulled = conn
        .q_str(
            &rtxn,
            r#"{:find [[(pull ?e [:person/name]) ...]]
                :where [[?e :person/id ?id]]
                :order-by [[?id :desc]]
                :limit 2}"#,
            &[],
        )
        .unwrap();
    let mut m4 = BTreeMap::new();
    m4.insert(":person/name".into(), PullValue::Single(Value::Str("Dan".into())));
    let mut m3 = BTreeMap::new();
    m3.insert(":person/name".into(), PullValue::Single(Value::Str("Bob".into())));
    assert_eq!(pulled, QueryResult::PulledColl(vec![m4, m3]));

    // :keys return map
    let maps = conn
        .q_str(
            &rtxn,
            r#"{:find [?id ?name]
                :where [[?e :person/id ?id] [?e :person/name ?name]]
                :order-by [[?id :asc]]
                :limit 1
                :keys [id name]}"#,
            &[],
        )
        .unwrap();
    let mut expected_map = BTreeMap::new();
    expected_map.insert("id".into(), Value::I64(1));
    expected_map.insert("name".into(), Value::Str("Zoe".into()));
    assert_eq!(maps, QueryResult::Maps(vec![expected_map]));
}

#[test]
fn upstream_index_range_seek_count_and_like() {
    let dir = tempdir().unwrap();
    let schema = vec![
        AttrSpec::new(":sec/ticker").unique_identity(),
        AttrSpec::new(":sec/price").indexed(),
        AttrSpec::new(":sec/name").indexed(),
    ];
    let conn = Conn::open(dir.path(), schema).unwrap();
    conn.transact(&[
        TxItem::Assert(None, vec![(":sec/ticker".into(), Value::Str("AAPL".into())), (":sec/price".into(), Value::F64(180.0)), (":sec/name".into(), Value::Str("Apple Inc.".into()))]),
        TxItem::Assert(None, vec![(":sec/ticker".into(), Value::Str("AMZN".into())), (":sec/price".into(), Value::F64(190.0)), (":sec/name".into(), Value::Str("Amazon.com".into()))]),
        TxItem::Assert(None, vec![(":sec/ticker".into(), Value::Str("MSFT".into())), (":sec/price".into(), Value::F64(420.0)), (":sec/name".into(), Value::Str("Microsoft".into()))]),
        TxItem::Assert(None, vec![(":sec/ticker".into(), Value::Str("NVDA".into())), (":sec/price".into(), Value::F64(140.0)), (":sec/name".into(), Value::Str("NVIDIA".into()))]),
    ])
    .unwrap();

    let rtxn = conn.read_txn().unwrap();

    // index_range on :sec/price [150.0 ..= 200.0]
    let range = conn
        .index_range(&rtxn, ":sec/price", &Value::F64(150.0), &Value::F64(200.0))
        .unwrap();
    assert_eq!(range.len(), 2);

    // seek_datoms >= 185.0
    let seek = conn.seek_datoms(&rtxn, ":sec/price", &Value::F64(185.0)).unwrap();
    assert_eq!(seek.len(), 2);

    // count_datoms
    assert_eq!(conn.count_datoms(&rtxn, ":sec/ticker", &Value::Str("AAPL".into())).unwrap(), 1);

    // SQL-style like wildcard matching
    let like_res = conn
        .q_str(
            &rtxn,
            r#"[:find [?t ...] :where [?e :sec/ticker ?t] [?e :sec/name ?n] [(like ?n "A_a%")]]"#,
            &[],
        )
        .unwrap();
    assert_eq!(like_res, QueryResult::Coll(vec![Value::Str("AMZN".into())]));
}

#[test]
fn upstream_composite_tuple_uuid_doc_and_cas() {
    let dir = tempdir().unwrap();
    let schema = vec![
        AttrSpec::new(":listing/tuple").unique_identity(),
        AttrSpec::new(":sec/uuid").unique_value(),
        AttrSpec::new(":sec/extra-terms"),
        AttrSpec::new(":sec/lot-size"),
    ];
    let conn = Conn::open(dir.path(), schema).unwrap();
    let uuid1 = [1u8; 16];
    let tup = Value::Tuple(vec![Value::Keyword(":XNAS".into()), Value::Str("AAPL".into())]);
    let ids = conn
        .transact(&[TxItem::Assert(
            None,
            vec![
                (":listing/tuple".into(), tup.clone()),
                (":sec/uuid".into(), Value::Uuid(uuid1)),
                (":sec/extra-terms".into(), Value::Doc(r#"{"boardLot":100,"tickTable":"US"}"#.into())),
                (":sec/lot-size".into(), Value::I64(100)),
            ],
        )])
        .unwrap();
    let eid = ids[0];

    // Lookup by composite tuple
    {
        let rtxn = conn.read_txn().unwrap();
        assert_eq!(conn.entid(&rtxn, ":listing/tuple", &tup).unwrap(), Some(eid));
        assert_eq!(conn.entid(&rtxn, ":sec/uuid", &Value::Uuid(uuid1)).unwrap(), Some(eid));
    }

    // CAS success
    conn.transact(&[TxItem::Cas(eid, ":sec/lot-size".into(), Value::I64(100), Value::I64(200))])
        .unwrap();
    // CAS failure when expected doesn't match
    let err = conn.transact(&[TxItem::Cas(eid, ":sec/lot-size".into(), Value::I64(100), Value::I64(300))]);
    assert!(matches!(err, Err(DatalogError::CasFailed { .. })));

    let rtxn2 = conn.read_txn().unwrap();
    assert_eq!(conn.get_attr(&rtxn2, eid, ":sec/lot-size").unwrap(), Some(Value::I64(200)));
}
