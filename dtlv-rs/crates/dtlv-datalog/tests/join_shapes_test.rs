//! Join shapes checked against a nested-loop oracle: the same question with the join variable moved into an
//! `(= ?v ?w)` filter. Each case runs with the joined attributes unindexed and indexed.

use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, QueryResult, TxItem};

fn build(indexed: bool) -> Conn {
    let spec = |name: &str| {
        let s = AttrSpec::new(name);
        if indexed { s.indexed() } else { s }
    };
    let conn = Conn::open_mem(vec![spec(":l/x"), spec(":r/x"), spec(":l/y"), spec(":r/y"), spec(":e/x")]).unwrap();
    // Left and right values include duplicates, 0.0 vs -0.0, NaN (equal to nothing), and a non-shared value.
    let left = [
        (Value::I64(1), 10),
        (Value::I64(1), 11),
        (Value::F64(0.0), 12),
        (Value::F64(f64::NAN), 13),
        (Value::F64(1.5), 14),
        (Value::I64(9), 10),
    ];
    let right = [
        (Value::I64(1), 10),
        (Value::F64(-0.0), 11),
        (Value::F64(f64::NAN), 12),
        (Value::F64(1.5), 13),
        (Value::F64(1.5), 10),
        (Value::I64(7), 14),
    ];
    let mut items = Vec::new();
    for (x, y) in left {
        items.push(TxItem::Assert(None, vec![(":l/x".into(), x), (":l/y".into(), Value::I64(y))]));
    }
    for (x, y) in right {
        items.push(TxItem::Assert(None, vec![(":r/x".into(), x), (":r/y".into(), Value::I64(y))]));
    }
    conn.transact(&items).unwrap();
    conn
}

/// Sorted rows of a relation query, so results compare independently of evaluation order.
fn rows(conn: &Conn, q: &str) -> Vec<Vec<Value>> {
    let rtxn = conn.read_txn().unwrap();
    match conn.q_str(&rtxn, q, &[]).unwrap() {
        QueryResult::Rel(mut r) => {
            r.sort_by_key(|row| format!("{row:?}"));
            r
        }
        other => panic!("expected a relation for {q}, got {other:?}"),
    }
}

fn check_against_oracle(indexed: bool, join: &str, oracle: &str) -> Vec<Vec<Value>> {
    let conn = build(indexed);
    let got = rows(&conn, join);
    let want = rows(&conn, oracle);
    assert_eq!(got, want, "join {join} (indexed: {indexed})");
    got
}

#[test]
fn shared_value_join_matches_oracle_including_zero_and_nan() {
    for indexed in [false, true] {
        let got = check_against_oracle(
            indexed,
            "[:find ?a ?b :where [?a :l/x ?v] [?b :r/x ?v]]",
            "[:find ?a ?b :where [?a :l/x ?v] [?b :r/x ?w] [(= ?v ?w)]]",
        );
        // 1 matches 1 twice on the left and once on the right (2 pairs); 0.0 matches -0.0 (1); 1.5 matches
        // 1.5 twice (2); NaN matches nothing.
        assert_eq!(got.len(), 5, "indexed: {indexed}");
    }
}

#[test]
fn three_clause_join_matches_oracle() {
    for indexed in [false, true] {
        check_against_oracle(
            indexed,
            "[:find ?a ?b ?c :where [?a :l/x ?v] [?b :r/x ?v] [?b :r/y ?w] [?c :l/y ?w]]",
            "[:find ?a ?b ?c :where [?a :l/x ?v] [?b :r/x ?u] [(= ?v ?u)] [?b :r/y ?w] [?c :l/y ?w]]",
        );
    }
}

#[test]
fn join_with_no_shared_variable_is_a_cross_product() {
    for indexed in [false, true] {
        let conn = build(indexed);
        // 6 left entities times 6 right entities, no shared variable.
        assert_eq!(rows(&conn, "[:find ?a ?b :where [?a :l/x ?v] [?b :r/x ?w]]").len(), 36, "indexed: {indexed}");
    }
}

#[test]
fn bound_entity_point_lookup_matches_either_sign_of_zero() {
    // The right side holds -0.0 once (entity 8). A literal 0.0 point lookup on that entity must still match it.
    for indexed in [false, true] {
        let conn = build(indexed);
        let got = rows(&conn, "[:find ?v :where [?a :r/x ?v] [?a :r/x 0.0]]");
        assert_eq!(got.len(), 1, "indexed: {indexed}");
    }
}

#[test]
fn join_against_an_empty_attribute_is_empty_on_either_side() {
    for indexed in [false, true] {
        let conn = build(indexed);
        assert!(rows(&conn, "[:find ?a ?b :where [?a :e/x ?v] [?b :r/x ?v]]").is_empty(), "indexed: {indexed}");
        assert!(rows(&conn, "[:find ?a ?b :where [?a :l/x ?v] [?b :e/x ?v]]").is_empty(), "indexed: {indexed}");
    }
}
