use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, QueryResult, TxItem};

const RULES: &str = r#"[
    [(main ?a ?z) [?e :x/id ?a] [?e :x/n ?z] (helper ?w)]
    [(helper ?w) [?h :h/v ?w]]
]"#;

#[test]
fn a_constant_bound_rule_can_call_an_all_free_helper() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":x/id"), AttrSpec::new(":x/n"), AttrSpec::new(":h/v")]).unwrap();
    conn.transact(&[
        TxItem::Assert(None, vec![(":x/id".into(), Value::I64(1)), (":x/n".into(), Value::I64(10))]),
        TxItem::Assert(None, vec![(":h/v".into(), Value::I64(99))]),
    ])
    .unwrap();
    let rtxn = conn.read_txn().unwrap();
    let r = conn.q_str_with_rules(&rtxn, "[:find ?z :where (main 1 ?z)]", &[], RULES);
    assert_eq!(r.unwrap(), QueryResult::Rel(vec![vec![Value::I64(10)]]));
}

#[test]
fn hash_probe_treats_negative_and_positive_zero_as_equal() {
    // The scan path compares with Value equality, where -0.0 == 0.0. The probe must agree.
    let conn = Conn::open_mem(vec![AttrSpec::new(":l/x"), AttrSpec::new(":r/x")]).unwrap();
    conn.transact(&[
        TxItem::Assert(None, vec![(":l/x".into(), Value::F64(-0.0))]),
        TxItem::Assert(None, vec![(":r/x".into(), Value::F64(0.0))]),
    ])
    .unwrap();
    let rtxn = conn.read_txn().unwrap();
    let r = conn.q_str(&rtxn, "[:find (count ?v) . :where [?a :l/x ?v] [?b :r/x ?v]]", &[]).unwrap();
    assert_eq!(r, QueryResult::Scalar(Some(Value::I64(1))));
}

#[test]
fn hash_probe_never_matches_nan() {
    // NaN is not equal to itself under Value equality, so a join on NaN has no matches.
    let conn = Conn::open_mem(vec![AttrSpec::new(":l/x"), AttrSpec::new(":r/x")]).unwrap();
    conn.transact(&[
        TxItem::Assert(None, vec![(":l/x".into(), Value::F64(f64::NAN))]),
        TxItem::Assert(None, vec![(":r/x".into(), Value::F64(f64::NAN))]),
    ])
    .unwrap();
    let rtxn = conn.read_txn().unwrap();
    let r = conn.q_str(&rtxn, "[:find ?v :where [?a :l/x ?v] [?b :r/x ?v]]", &[]).unwrap();
    assert_eq!(r, QueryResult::Rel(vec![]));
}
