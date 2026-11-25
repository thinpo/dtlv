use std::collections::BTreeMap;

use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, TxItem};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn conn() -> Conn {
    let conn = Conn::open_mem(vec![
        AttrSpec::new(":p/name").unique_identity(),
        AttrSpec::new(":p/age"),
        AttrSpec::new(":p/tag").many(),
    ])
    .unwrap();
    conn.transact(&[TxItem::Assert(
        None,
        vec![
            (":p/name".into(), s("Ann")),
            (":p/age".into(), Value::I64(30)),
            (":p/tag".into(), s("x")),
            (":p/tag".into(), s("y")),
        ],
    )])
    .unwrap();
    conn
}

#[test]
fn get_reads_single_and_many_valued_attributes() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    let e = c.lookup_entity_by_av(&rtxn, ":p/name", &s("Ann")).unwrap().unwrap();
    let ent = c.entity(&rtxn, e);
    assert_eq!(ent.eid(), e);
    assert_eq!(ent.get(":p/age").unwrap(), Some(Value::I64(30)));
    assert_eq!(ent.get(":p/tag").unwrap(), Some(Value::Tuple(vec![s("x"), s("y")])));
    assert!(ent.get(":p/undeclared").is_err(), "undeclared attributes are an error on get");
    assert_eq!(c.entity(&rtxn, 999).get(":p/age").unwrap(), None);
}

#[test]
fn touch_materializes_every_attribute() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    let ent = c.entity_by_lookup(&rtxn, ":p/name", &s("Ann")).unwrap().unwrap();
    let all: BTreeMap<String, Value> = ent.touch().unwrap();
    assert_eq!(all.get(":p/name"), Some(&s("Ann")));
    assert_eq!(all.get(":p/age"), Some(&Value::I64(30)));
    assert_eq!(all.get(":p/tag"), Some(&Value::Tuple(vec![s("x"), s("y")])));
    assert_eq!(all.len(), 3, "name, age, and tag are the only attributes");
}

#[test]
fn lookup_for_unknown_value_is_none() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    assert!(c.entity_by_lookup(&rtxn, ":p/name", &s("Nobody")).unwrap().is_none());
}
