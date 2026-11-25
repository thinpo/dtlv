//! Magic sets on prepared queries: a prepared query with a bound `:in` input must match the unprepared
//! `q_str_inputs` result, and must be demand-driven rather than computing the full closure.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, QueryInput, QueryResult, TxItem};

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

#[test]
fn prepared_reach_matches_q_str_inputs() {
    let conn = graph(&[(1, 2), (2, 3), (2, 4), (4, 5), (6, 7), (7, 6)]);
    let rtxn = conn.read_txn().unwrap();
    let query = "[:find ?a ?b :in $ ?a :where (reach ?a ?b)]";
    let prepared = conn.prepare_query(query, RULES).unwrap();
    for src in [1, 2, 6, 99] {
        let input = [QueryInput::Scalar(Value::I64(src))];
        let via_prepared = pairs(prepared.execute_inputs(&rtxn, &input).unwrap());
        let via_q = pairs(conn.q_str_inputs(&rtxn, query, &input, RULES).unwrap());
        assert_eq!(via_prepared, via_q, "source {src}");
    }
}

#[test]
fn prepared_demand_driven_is_much_cheaper() {
    // 300 nodes in a chain: from node 290 only 9 nodes are reachable, while the full closure has ~45k pairs.
    let edges: Vec<(i64, i64)> = (0..299).map(|i| (i, i + 1)).collect();
    let conn = graph(&edges);
    let rtxn = conn.read_txn().unwrap();

    let prepared = conn.prepare_query("[:find ?a ?b :in $ ?a :where (reach ?a ?b)]", RULES).unwrap();
    let t = Instant::now();
    let demand = pairs(prepared.execute(&rtxn, &[Value::I64(290)]).unwrap());
    let demand_time: Duration = t.elapsed();
    assert_eq!(demand.len(), 9);

    let t = Instant::now();
    let full = pairs(conn.q_str_with_rules(&rtxn, "[:find ?a ?b :where (reach ?a ?b)]", &[], RULES).unwrap());
    let full_time: Duration = t.elapsed();
    assert!(full.len() > 9);

    println!("prepared demand-driven (source 290): {demand_time:?}");
    println!("full closure: {full_time:?}");
    assert!(demand_time * 5 < full_time, "prepared {demand_time:?} not 5x faster than full {full_time:?}");
}
