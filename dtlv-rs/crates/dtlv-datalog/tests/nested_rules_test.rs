use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, QueryResult, TxItem};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

#[test]
fn nested_queries_see_the_outer_rules() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":par/of"), AttrSpec::new(":par/name")]).unwrap();
    // a -> b -> c, stored as (child :par/of parent-name) facts.
    conn.transact(&[
        TxItem::Assert(None, vec![(":par/name".into(), s("b")), (":par/of".into(), s("a"))]),
        TxItem::Assert(None, vec![(":par/name".into(), s("c")), (":par/of".into(), s("b"))]),
    ])
    .unwrap();
    let rules = r#"[
        [(ancestor ?x ?y) [?c :par/of ?x] [?c :par/name ?y]]
        [(ancestor ?x ?z) [?c :par/of ?x] [?c :par/name ?y] (ancestor ?y ?z)]
    ]"#;
    let rtxn = conn.read_txn().unwrap();
    let q = r#"[:find [?z ...] :where
                [(q [:find [?z ...] :in ?x :where (ancestor ?x ?z)] "a") [?z ...]]]"#;
    let mut got = match conn.q_str_with_rules(&rtxn, q, &[], rules).unwrap() {
        QueryResult::Coll(v) => v,
        other => panic!("{other:?}"),
    };
    got.sort_by_key(|v| format!("{v:?}"));
    assert_eq!(got, vec![s("b"), s("c")]);
}
