use std::collections::BTreeMap;

use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, QueryInput, QueryResult, TxItem};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn conn() -> Conn {
    let c = Conn::open_mem(vec![AttrSpec::new(":p/name").unique_identity(), AttrSpec::new(":p/code").noindex()]).unwrap();
    c.transact(&[
        TxItem::Assert(None, vec![(":p/name".into(), s("ann")), (":p/code".into(), s("x1"))]),
        TxItem::Assert(None, vec![(":p/name".into(), s("bo")), (":p/code".into(), s("x2"))]),
    ])
    .unwrap();
    c
}

#[test]
fn literal_value_is_fine_when_the_entity_is_already_bound() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    // ?e comes from an indexed lookup; reading the noindex attribute on that entity needs no value index.
    let r = c.q_str(&rtxn, r#"[:find ?code :where [?e :p/name "ann"] [?e :p/code ?code]]"#, &[]).unwrap();
    assert_eq!(r, QueryResult::Rel(vec![vec![s("x1")]]));
    // Checking a value on a bound entity is an entity lookup too.
    let r = c.q_str(&rtxn, r#"[:find ?e . :where [?e :p/name "ann"] [?e :p/code "x1"]]"#, &[]).unwrap();
    assert!(matches!(r, QueryResult::Scalar(Some(_))), "got {r:?}");
}

#[test]
fn selecting_entities_by_a_noindex_value_is_rejected_even_when_the_value_comes_from_inputs() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    let err = c
        .q_str_inputs(&rtxn, "[:find ?e :in ?v :where [?e :p/code ?v]]", &[QueryInput::Scalar(s("x1"))], "[]")
        .unwrap_err();
    assert!(matches!(err, DatalogError::Type(_)), "got {err:?}");
    let err = c.q_str(&rtxn, r#"[:find ?e :where [?e :p/code "x1"]]"#, &[]).unwrap_err();
    assert!(matches!(err, DatalogError::Type(_)), "got {err:?}");
    // The value comes from an earlier clause, not a literal: selecting ?e by it is still a value search.
    let err = c
        .q_str(&rtxn, r#"[:find ?e :where [?f :p/name "ann"] [?f :p/code ?v] [?e :p/code ?v]]"#, &[])
        .unwrap_err();
    assert!(matches!(err, DatalogError::Type(_)), "got {err:?}");
}

#[test]
fn keys_shape_aggregate_results_as_maps() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    let r = c.q_str(&rtxn, "{:find [(count ?e)] :keys [total] :where [[?e :p/name ?n]]}", &[]).unwrap();
    let mut want = BTreeMap::new();
    want.insert("total".to_string(), Value::I64(2));
    assert_eq!(r, QueryResult::Maps(vec![want]));
}
