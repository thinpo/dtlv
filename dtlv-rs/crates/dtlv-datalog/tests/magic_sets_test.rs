//! Magic-set rewriting: results must equal full evaluation, and demand-driven queries must do less work.
//! Oracle: the same question written with the constant in a filter, which skips the rewrite.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, QueryResult, TxItem};

const RULES: &str = r#"[
    [(reach ?a ?b) [?e :e/id ?a] [?e :e/to ?b]]
    [(reach ?a ?c) [?e :e/id ?a] [?e :e/to ?b] (reach ?b ?c)]
]"#;

/// Edges given as (from, to) pairs, stored as entities with a unique id and a multi-valued target.
fn graph(edges: &[(i64, i64)]) -> Conn {
    let conn = Conn::open_mem(vec![AttrSpec::new(":e/to").many(), AttrSpec::new(":e/id").unique_identity()]).unwrap();
    let mut nodes: BTreeSet<i64> = BTreeSet::new();
    for &(a, b) in edges {
        nodes.insert(a);
        nodes.insert(b);
    }
    let mut items = Vec::new();
    for &n in &nodes {
        let targets: Vec<(String, Value)> =
            edges.iter().filter(|(a, _)| *a == n).map(|(_, b)| (":e/to".to_string(), Value::I64(*b))).collect();
        let mut attrs = vec![(":e/id".to_string(), Value::I64(n))];
        attrs.extend(targets);
        items.push(TxItem::Assert(None, attrs));
    }
    conn.transact(&items).unwrap();
    conn
}

fn pairs(r: QueryResult) -> BTreeSet<(i64, i64)> {
    match r {
        QueryResult::Rel(rows) => rows
            .into_iter()
            .map(|row| match (&row[0], &row[1]) {
                (Value::I64(a), Value::I64(b)) => (*a, *b),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn singles(r: QueryResult) -> BTreeSet<i64> {
    match r {
        QueryResult::Coll(v) => v
            .into_iter()
            .map(|x| match x {
                Value::I64(n) => n,
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// Descendants of `src` by the oracle: all reach pairs, filtered in the query.
fn oracle_from(conn: &Conn, src: i64) -> BTreeSet<i64> {
    let rtxn = conn.read_txn().unwrap();
    let q = format!("[:find [?b ...] :where (reach ?a ?b) [(= ?a {src})]]");
    singles(conn.q_str_with_rules(&rtxn, &q, &[], RULES).unwrap())
}

fn magic_from(conn: &Conn, src: i64) -> BTreeSet<i64> {
    let rtxn = conn.read_txn().unwrap();
    let q = format!("[:find [?b ...] :where (reach {src} ?b)]");
    singles(conn.q_str_with_rules(&rtxn, &q, &[], RULES).unwrap())
}

#[test]
fn bound_first_argument_matches_full_evaluation_on_a_branching_graph() {
    let conn = graph(&[(1, 2), (2, 3), (3, 4), (1, 5), (5, 6), (6, 1), (7, 8)]);
    for src in [1, 2, 5, 7, 8, 99] {
        assert_eq!(magic_from(&conn, src), oracle_from(&conn, src), "source {src}");
    }
}

#[test]
fn bound_second_argument_uses_a_different_adornment() {
    let conn = graph(&[(1, 2), (2, 3), (3, 4), (5, 4)]);
    let rtxn = conn.read_txn().unwrap();
    // Who reaches 4? Adornment fb, not bf.
    let magic = singles(
        conn.q_str_with_rules(&rtxn, "[:find [?a ...] :where (reach ?a 4)]", &[], RULES).unwrap(),
    );
    let oracle = singles(
        conn.q_str_with_rules(&rtxn, "[:find [?a ...] :where (reach ?a ?b) [(= ?b 4)]]", &[], RULES).unwrap(),
    );
    assert_eq!(magic, oracle);
    assert_eq!(magic, BTreeSet::from([1, 2, 3, 5]));
}

#[test]
fn both_arguments_bound_is_a_membership_test() {
    let conn = graph(&[(1, 2), (2, 3), (3, 4)]);
    let rtxn = conn.read_txn().unwrap();
    let yes = conn.q_str_with_rules(&rtxn, "[:find ?x . :where (reach 1 4) [(ground 1) ?x]]", &[], RULES).unwrap();
    let no = conn.q_str_with_rules(&rtxn, "[:find ?x . :where (reach 4 1) [(ground 1) ?x]]", &[], RULES).unwrap();
    assert_eq!(yes, QueryResult::Scalar(Some(Value::I64(1))));
    assert_eq!(no, QueryResult::Scalar(None));
}

#[test]
fn rules_with_negation_fall_back_and_stay_correct() {
    let conn = graph(&[(1, 2), (2, 3), (3, 1), (2, 9)]);
    let rules = r#"[
        [(open ?a ?b) [?e :e/id ?a] [?e :e/to ?b] (not [?b :e/to 9])]
        [(open ?a ?c) [?e :e/id ?a] [?e :e/to ?b] (not [?b :e/to 9]) (open ?b ?c)]
    ]"#;
    let rtxn = conn.read_txn().unwrap();
    let magic = singles(conn.q_str_with_rules(&rtxn, "[:find [?b ...] :where (open 1 ?b)]", &[], rules).unwrap());
    let oracle = singles(
        conn.q_str_with_rules(&rtxn, "[:find [?b ...] :where (open ?a ?b) [(= ?a 1)]]", &[], rules).unwrap(),
    );
    assert_eq!(magic, oracle);
}

#[test]
fn a_query_with_no_constant_is_unchanged() {
    let conn = graph(&[(1, 2), (2, 3)]);
    let rtxn = conn.read_txn().unwrap();
    let r = pairs(conn.q_str_with_rules(&rtxn, "[:find ?a ?b :where (reach ?a ?b)]", &[], RULES).unwrap());
    assert_eq!(r, BTreeSet::from([(1, 2), (1, 3), (2, 3)]));
}

#[test]
fn demand_driven_query_is_much_cheaper_than_the_full_closure() {
    // 300 nodes in a chain. Asking for the descendants of the last node needs no work at all; the full closure
    // computes every pair.
    let edges: Vec<(i64, i64)> = (0..300).map(|i| (i, i + 1)).collect();
    let conn = graph(&edges);
    let rtxn = conn.read_txn().unwrap();
    let t = Instant::now();
    let demand = singles(conn.q_str_with_rules(&rtxn, "[:find [?b ...] :where (reach 290 ?b)]", &[], RULES).unwrap());
    let demand_time = t.elapsed();
    assert_eq!(demand.len(), 10, "290 reaches 291..=300");
    let t = Instant::now();
    let full = pairs(conn.q_str_with_rules(&rtxn, "[:find ?a ?b :where (reach ?a ?b)]", &[], RULES).unwrap());
    let full_time = t.elapsed();
    println!("demand-driven {demand_time:?} vs full closure {full_time:?} ({} pairs)", full.len());
    assert!(demand_time * 5 < full_time, "demand {demand_time:?} not clearly cheaper than full {full_time:?}");
    assert!(demand_time < Duration::from_secs(60));
}
