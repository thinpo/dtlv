use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, TxItem};
use tempfile::tempdir;

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn base_schema() -> Vec<AttrSpec> {
    vec![
        AttrSpec::new(":item/n").indexed(),
        AttrSpec::new(":item/color"),
        AttrSpec::new(":doc/text").fulltext(),
    ]
}

fn seeded() -> Conn {
    let conn = Conn::open_mem(base_schema()).unwrap();
    conn.transact(&[
        TxItem::Assert(None, vec![(":item/n".into(), Value::I64(1)), (":item/color".into(), s("red"))]),
        TxItem::Assert(None, vec![(":item/n".into(), Value::I64(5)), (":item/color".into(), s("red"))]),
        TxItem::Assert(None, vec![(":item/n".into(), Value::I64(9)), (":item/color".into(), s("blue"))]),
    ])
    .unwrap();
    conn
}

#[test]
fn max_eid_and_cardinality() {
    let conn = seeded();
    let rtxn = conn.read_txn().unwrap();
    assert_eq!(conn.max_eid(&rtxn).unwrap(), 3);
    assert_eq!(conn.cardinality(&rtxn, ":item/color").unwrap(), 2);
    assert_eq!(conn.cardinality(&rtxn, ":item/n").unwrap(), 3);
}

#[test]
fn rseek_is_seek_reversed() {
    let conn = seeded();
    let rtxn = conn.read_txn().unwrap();
    let fwd: Vec<Value> = conn
        .seek_datoms(&rtxn, ":item/n", &Value::I64(4))
        .unwrap()
        .into_iter()
        .map(|d| d.2)
        .collect();
    assert_eq!(fwd, vec![Value::I64(5), Value::I64(9)]);
    let back: Vec<Value> = conn
        .rseek_datoms(&rtxn, ":item/n", &Value::I64(5))
        .unwrap()
        .into_iter()
        .map(|d| d.2)
        .collect();
    assert_eq!(back, vec![Value::I64(5), Value::I64(1)]);
}

#[test]
fn search_datoms_wildcards_and_bound_components() {
    let conn = seeded();
    let rtxn = conn.read_txn().unwrap();
    let all = conn.search_datoms(&rtxn, None, None, None).unwrap();
    assert_eq!(all.len(), 6);

    let by_value = conn.search_datoms(&rtxn, None, Some(":item/color"), Some(&s("red"))).unwrap();
    assert_eq!(by_value.len(), 2);

    let by_entity = conn.search_datoms(&rtxn, Some(2), None, None).unwrap();
    assert_eq!(by_entity.len(), 2);
    assert!(by_entity.iter().all(|(e, _, _)| *e == 2));

    let one = conn.search_datoms(&rtxn, Some(2), Some(":item/n"), None).unwrap();
    assert_eq!(one, vec![(2, ":item/n".to_string(), Value::I64(5))]);

    assert!(conn.search_datoms(&rtxn, None, Some(":nope"), None).is_err());
}

#[test]
fn fulltext_datoms_return_matching_entity_text() {
    let conn = Conn::open_mem(base_schema()).unwrap();
    conn.transact(&[
        TxItem::Assert(None, vec![(":doc/text".into(), s("the rust compiler"))]),
        TxItem::Assert(None, vec![(":doc/text".into(), s("cooking pasta"))]),
    ])
    .unwrap();
    let rtxn = conn.read_txn().unwrap();
    let hits = conn.fulltext_datoms(&rtxn, "rust", 10).unwrap();
    assert_eq!(hits, vec![(1, ":doc/text".to_string(), s("the rust compiler"))]);
}

#[test]
fn update_schema_adds_renames_and_deletes() {
    let mut conn = seeded();
    conn.update_schema(vec![AttrSpec::new(":item/size")], &[], &[]).unwrap();
    conn.transact(&[TxItem::Assert(Some(1), vec![(":item/size".into(), s("L"))])]).unwrap();

    conn.update_schema(vec![], &[], &[(":item/color", ":item/colour")]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    assert_eq!(conn.get_attr(&rtxn, 1, ":item/colour").unwrap(), Some(s("red")));
    assert!(conn.get_attr(&rtxn, 1, ":item/color").is_err());
    drop(rtxn);

    conn.update_schema(vec![], &[":item/size"], &[]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    assert!(conn.get_attr(&rtxn, 1, ":item/size").is_err());
    assert_eq!(conn.search_datoms(&rtxn, None, None, None).unwrap().len(), 6);
}

#[test]
fn update_schema_index_upgrade_backfills_existing_data() {
    let mut conn = seeded();
    // :item/colour is not indexed yet; upgrading it must make existing values findable.
    conn.update_schema(vec![], &[], &[(":item/color", ":item/colour")]).unwrap();
    conn.update_schema(vec![AttrSpec::new(":item/colour").indexed()], &[], &[]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    let blue = conn.index_range(&rtxn, ":item/colour", &s("blue"), &s("blue")).unwrap();
    assert_eq!(blue.len(), 1);
    assert_eq!(blue[0].0, 3);
}

#[test]
fn update_schema_rejects_bad_changes_without_writing() {
    let mut conn = seeded();
    // Changing cardinality of an existing attribute is refused.
    let err = conn
        .update_schema(vec![AttrSpec::new(":item/n").many()], &[], &[])
        .unwrap_err();
    assert!(matches!(err, DatalogError::Schema(_)), "got {err:?}");

    // A clashing rename is refused before anything is written.
    let err = conn
        .update_schema(vec![], &[], &[(":item/n", ":item/color")])
        .unwrap_err();
    assert!(matches!(err, DatalogError::Schema(_)), "got {err:?}");
    let rtxn = conn.read_txn().unwrap();
    assert_eq!(conn.get_attr(&rtxn, 1, ":item/n").unwrap(), Some(Value::I64(1)));
}

#[test]
fn reopening_with_a_smaller_schema_does_not_panic() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let conn = Conn::open(&path, base_schema()).unwrap();
        conn.transact(&[TxItem::Assert(None, vec![(":item/color".into(), s("red"))])]).unwrap();
    }
    // Reopen without :item/color in the schema: the attribute is still readable and writable.
    let conn = Conn::open(&path, vec![AttrSpec::new(":item/n").indexed()]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    assert_eq!(conn.get_attr(&rtxn, 1, ":item/color").unwrap(), Some(s("red")));
    drop(rtxn);
    conn.transact(&[TxItem::Assert(Some(1), vec![(":item/color".into(), s("green"))])]).unwrap();
}
