//! Regression tests for the dedup, retraction, scan-binding, sum and save fixes.

use std::collections::BTreeMap;
use std::path::Path;

use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, PullValue, QueryResult, TxItem};
use tempfile::tempdir;

fn person_conn(dir: &Path, n: i64, names: &[&str]) -> Conn {
    let schema = vec![
        AttrSpec::new(":person/id").unique_identity(),
        AttrSpec::new(":person/name"),
    ];
    let conn = Conn::open(dir, schema).unwrap();
    let items: Vec<TxItem> = (0..n)
        .map(|i| {
            TxItem::Assert(
                None,
                vec![
                    (":person/id".into(), Value::I64(i)),
                    (":person/name".into(), Value::Str(names[i as usize].into())),
                ],
            )
        })
        .collect();
    conn.transact(&items).unwrap();
    conn
}

fn node_conn(dir: &Path) -> Conn {
    let schema = vec![AttrSpec::new(":node/name"), AttrSpec::new(":node/next").indexed()];
    Conn::open(dir, schema).unwrap()
}

fn add_node(conn: &Conn, name: &str) -> u64 {
    conn.transact(&[TxItem::Assert(None, vec![(":node/name".into(), Value::Str(name.into()))])])
        .unwrap()[0]
}

fn link(conn: &Conn, from: u64, to: u64) {
    conn.transact(&[TxItem::Assert(Some(from), vec![(":node/next".into(), Value::Ref(to))])])
        .unwrap();
}

// --- Pull inside :find must not collapse distinct entities (dedup regression) ---

#[test]
fn pull_without_order_by_returns_every_entity() {
    let dir = tempdir().unwrap();
    let conn = person_conn(dir.path(), 3, &["A", "B", "C"]);
    let rtxn = conn.read_txn().unwrap();
    let res = conn
        .q_str(
            &rtxn,
            r#"{:find [[(pull ?e [:person/name]) ...]] :where [[?e :person/id ?id]]}"#,
            &[],
        )
        .unwrap();
    let mut names: Vec<String> = match res {
        QueryResult::PulledColl(maps) => maps
            .iter()
            .map(|m| match m.get(":person/name") {
                Some(PullValue::Single(Value::Str(s))) => s.clone(),
                other => panic!("unexpected pull value: {other:?}"),
            })
            .collect(),
        other => panic!("expected PulledColl, got {other:?}"),
    };
    names.sort();
    assert_eq!(names, vec!["A", "B", "C"]);
}

#[test]
fn pull_with_limit_and_no_order_by_respects_limit() {
    let dir = tempdir().unwrap();
    let conn = person_conn(dir.path(), 4, &["A", "B", "C", "D"]);
    let rtxn = conn.read_txn().unwrap();
    let res = conn
        .q_str(
            &rtxn,
            r#"{:find [[(pull ?e [:person/name]) ...]] :where [[?e :person/id ?id]] :limit 2}"#,
            &[],
        )
        .unwrap();
    match res {
        QueryResult::PulledColl(maps) => {
            assert_eq!(maps.len(), 2);
            let m: &BTreeMap<String, PullValue> = &maps[0];
            assert!(m.contains_key(":person/name"));
        }
        other => panic!("expected PulledColl, got {other:?}"),
    }
}

// --- Dedup happens before offset/limit, and still removes duplicates ---

#[test]
fn dedup_before_limit_returns_distinct_values() {
    let dir = tempdir().unwrap();
    // Three rows carry "A"; without dedup-before-limit, limit 2 can return ["A", "A"].
    let conn = person_conn(dir.path(), 4, &["A", "A", "A", "B"]);
    let rtxn = conn.read_txn().unwrap();
    let res = conn
        .q_str(&rtxn, r#"{:find [?n ...] :where [[?e :person/name ?n]] :limit 2}"#, &[])
        .unwrap();
    match res {
        QueryResult::Coll(vals) => {
            assert_eq!(vals.len(), 2);
            assert_ne!(vals[0], vals[1], "duplicates leaked past dedup: {vals:?}");
        }
        other => panic!("expected Coll, got {other:?}"),
    }
}

#[test]
fn rel_query_dedups_duplicate_projections() {
    let dir = tempdir().unwrap();
    let conn = person_conn(dir.path(), 3, &["A", "A", "B"]);
    let rtxn = conn.read_txn().unwrap();
    let res = conn.q_str(&rtxn, r#"[:find ?n :where [?e :person/name ?n]]"#, &[]).unwrap();
    match res {
        QueryResult::Rel(rows) => assert_eq!(rows.len(), 2),
        other => panic!("expected Rel, got {other:?}"),
    }
}

// --- Retraction removes incoming references (eav, ave and vae) ---

#[test]
fn retract_entity_removes_incoming_refs_from_all_indexes() {
    let dir = tempdir().unwrap();
    let conn = node_conn(dir.path());
    let a = add_node(&conn, "a");
    let b = add_node(&conn, "b");
    link(&conn, a, b);

    let before = {
        let rtxn = conn.read_txn().unwrap();
        conn.q_str(&rtxn, "[:find ?e :in $ ?t :where [?e :node/next ?t]]", &[Value::Ref(b)])
            .unwrap()
    };
    assert_eq!(before, QueryResult::Rel(vec![vec![Value::Ref(a)]]));

    conn.transact(&[TxItem::RetractEntity(b)]).unwrap();

    let rtxn = conn.read_txn().unwrap();
    // Value-bound lookup goes through the ave index.
    let by_value = conn
        .q_str(&rtxn, "[:find ?e :in $ ?t :where [?e :node/next ?t]]", &[Value::Ref(b)])
        .unwrap();
    assert_eq!(by_value, QueryResult::Rel(vec![]));
    // Full scan goes through eav.
    let all = conn.q_str(&rtxn, "[:find ?e ?t :where [?e :node/next ?t]]", &[]).unwrap();
    assert_eq!(all, QueryResult::Rel(vec![]));
    // The retracted entity itself is gone too.
    let names = conn.q_str(&rtxn, "[:find ?n :where [?e :node/name ?n]]", &[]).unwrap();
    assert_eq!(names, QueryResult::Rel(vec![vec![Value::Str("a".into())]]));
}

#[test]
fn retract_entity_removes_self_reference() {
    let dir = tempdir().unwrap();
    let conn = node_conn(dir.path());
    let a = add_node(&conn, "a");
    link(&conn, a, a);
    conn.transact(&[TxItem::RetractEntity(a)]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    let all = conn.q_str(&rtxn, "[:find ?e ?t :where [?e :node/next ?t]]", &[]).unwrap();
    assert_eq!(all, QueryResult::Rel(vec![]));
}

#[test]
fn retract_entity_keeps_unrelated_refs() {
    let dir = tempdir().unwrap();
    let conn = node_conn(dir.path());
    let a = add_node(&conn, "a");
    let b = add_node(&conn, "b");
    let c = add_node(&conn, "c");
    link(&conn, a, b);
    link(&conn, a, c);
    conn.transact(&[TxItem::RetractEntity(b)]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    let res = conn.q_str(&rtxn, "[:find ?t :where [?e :node/next ?t]]", &[]).unwrap();
    assert_eq!(res, QueryResult::Rel(vec![vec![Value::Ref(c)]]));
}

// --- Shared variable across pattern positions must unify, not overwrite ---

#[test]
fn shared_var_in_entity_and_value_position_unifies() {
    let dir = tempdir().unwrap();
    let conn = node_conn(dir.path());
    let a = add_node(&conn, "a");
    let b = add_node(&conn, "b");
    link(&conn, a, b); // a -> b, not a self-loop
    link(&conn, b, b); // b -> b, self-loop
    let rtxn = conn.read_txn().unwrap();
    let res = conn.q_str(&rtxn, "[:find ?x :where [?x :node/next ?x]]", &[]).unwrap();
    assert_eq!(res, QueryResult::Rel(vec![vec![Value::Ref(b)]]));
}

// --- Sum: exact i64 arithmetic and overflow reporting ---

fn values_conn(dir: &Path, vals: &[i64]) -> Conn {
    let schema = vec![AttrSpec::new(":m/v")];
    let conn = Conn::open(dir, schema).unwrap();
    let items: Vec<TxItem> = vals
        .iter()
        .map(|v| TxItem::Assert(None, vec![(":m/v".into(), Value::I64(*v))]))
        .collect();
    conn.transact(&items).unwrap();
    conn
}

#[test]
fn sum_of_i64_is_exact_beyond_f64_precision() {
    let dir = tempdir().unwrap();
    let big = (1i64 << 53) + 1; // not representable as f64
    let conn = values_conn(dir.path(), &[big, 2]);
    let rtxn = conn.read_txn().unwrap();
    let res = conn.q_str(&rtxn, "[:find (sum ?v) :where [?e :m/v ?v]]", &[]).unwrap();
    assert_eq!(res, QueryResult::Rel(vec![vec![Value::I64(big + 2)]]));
}

#[test]
fn sum_overflow_is_an_error_not_a_wrap() {
    let dir = tempdir().unwrap();
    let conn = values_conn(dir.path(), &[i64::MAX, 1]);
    let rtxn = conn.read_txn().unwrap();
    let err = conn.q_str(&rtxn, "[:find (sum ?v) :where [?e :m/v ?v]]", &[]).unwrap_err();
    assert!(matches!(err, DatalogError::SumOverflow), "got {err:?}");
}

// --- save_to_sqlite: atomic replace, no stale sidecars or temp files ---

#[test]
fn save_to_sqlite_replaces_destination_and_leaves_no_temp_files() {
    let dir = tempdir().unwrap();
    let dst = dir.path().join("snap.sqlite");
    let dst_tmp = dir.path().join("snap.sqlite.tmp");

    // Pre-existing junk at the destination, plus a sidecar from an old database.
    std::fs::write(&dst, b"not a database").unwrap();
    std::fs::write(dir.path().join("snap.sqlite-wal"), b"stale wal bytes").unwrap();

    let first = person_conn(&dir.path().join("lmdb1"), 3, &["A", "B", "C"]);
    {
        let rtxn = first.read_txn().unwrap();
        first.save_to_sqlite(&rtxn, &dst).unwrap();
    }
    assert!(!dir.path().join("snap.sqlite-wal").exists(), "stale sidecar survived save");
    assert!(!dst_tmp.exists(), "temp file left behind");

    // Second save with fewer entities must fully replace the first snapshot.
    let second = person_conn(&dir.path().join("lmdb2"), 1, &["Z"]);
    {
        let rtxn = second.read_txn().unwrap();
        second.save_to_sqlite(&rtxn, &dst).unwrap();
    }
    let restored = Conn::load_from_sqlite(
        dir.path().join("lmdb3"),
        &dst,
        vec![
            AttrSpec::new(":person/id").unique_identity(),
            AttrSpec::new(":person/name"),
        ],
    )
    .unwrap();
    let rtxn = restored.read_txn().unwrap();
    let names = restored.q_str(&rtxn, "[:find ?n :where [?e :person/name ?n]]", &[]).unwrap();
    assert_eq!(names, QueryResult::Rel(vec![vec![Value::Str("Z".into())]]));
}
