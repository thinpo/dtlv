use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, QueryResult, TxItem};

fn k(x: &str) -> Value {
    Value::Str(x.into())
}

fn conn() -> Conn {
    let conn = Conn::open_mem(vec![
        AttrSpec::new(":left/key"),
        AttrSpec::new(":right/key"),
        AttrSpec::new(":p/k"),
        AttrSpec::new(":p/n"),
        AttrSpec::new(":x/k"),
    ])
    .unwrap();
    let mut items = Vec::new();
    for key in ["k1", "k1", "k2"] {
        items.push(TxItem::Assert(None, vec![(":left/key".into(), k(key))]));
    }
    for key in ["k1", "k1", "k1", "k3"] {
        items.push(TxItem::Assert(None, vec![(":right/key".into(), k(key))]));
    }
    for (kk, n) in [("a", "one"), ("a", "two"), ("b", "three")] {
        items.push(TxItem::Assert(None, vec![(":p/k".into(), k(kk)), (":p/n".into(), k(n))]));
    }
    items.push(TxItem::Assert(None, vec![(":x/k".into(), k("a"))]));
    items.push(TxItem::Assert(None, vec![(":x/k".into(), k("b"))]));
    conn.transact(&items).unwrap();
    conn
}

#[test]
fn derived_relations_join_and_aggregate_like_the_documented_example() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    let q = r#"[:find (sum ?w) .
                :where
                [(q [:find ?key (count ?l) :where [?l :left/key ?key]] $) [[?key ?wl]]]
                [(q [:find ?key (count ?r) :where [?r :right/key ?key]] $) [[?key ?wr]]]
                [(* ?wl ?wr) ?w]]"#;
    // Only k1 appears on both sides: 2 left rows times 3 right rows.
    assert_eq!(c.q_str(&rtxn, q, &[]).unwrap(), QueryResult::Scalar(Some(Value::I64(6))));
}

#[test]
fn correlated_nested_queries_run_per_outer_binding() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    let q = r#"[:find ?k ?n :where
                [?x :x/k ?k]
                [(q [:find [?n ...] :in ?kk :where [?e :p/k ?kk] [?e :p/n ?n]] ?k) [?n ...]]]"#;
    let mut rows = match c.q_str(&rtxn, q, &[]).unwrap() {
        QueryResult::Rel(r) => r,
        other => panic!("{other:?}"),
    };
    rows.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(
        rows,
        vec![vec![k("a"), k("one")], vec![k("a"), k("two")], vec![k("b"), k("three")]]
    );
}

#[test]
fn nested_column_count_must_match_the_outputs() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    let q = r#"[:find ?a :where [(q [:find ?key (count ?l) :where [?l :left/key ?key]] $) [[?a]]]]"#;
    let err = c.q_str(&rtxn, q, &[]).unwrap_err();
    assert!(matches!(err, DatalogError::Parse(_)), "got {err:?}");
}
