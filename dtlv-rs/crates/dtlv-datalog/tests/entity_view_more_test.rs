use std::collections::BTreeMap;

use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, TxItem};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn conn() -> Conn {
    Conn::open_mem(vec![
        AttrSpec::new(":p/name").unique_identity(),
        AttrSpec::new(":p/secret").noindex(),
        AttrSpec::new(":p/tag").many(),
        AttrSpec::new(":p/score").indexed(),
    ])
    .unwrap()
}

#[test]
fn touch_covers_many_attributes_and_noindex_values() {
    let c = conn();
    let e = c
        .transact(&[TxItem::Assert(
            None,
            vec![
                (":p/name".into(), s("ann")),
                (":p/secret".into(), s("hidden")),
                (":p/tag".into(), s("a")),
                (":p/tag".into(), s("b")),
                (":p/score".into(), Value::I64(9)),
            ],
        )])
        .unwrap()[0];
    let rtxn = c.read_txn().unwrap();
    let all: BTreeMap<String, Value> = c.entity(&rtxn, e).touch().unwrap();
    assert_eq!(all.get(":p/secret"), Some(&s("hidden")), "noindex values are still entity data");
    assert_eq!(all.get(":p/tag"), Some(&Value::Tuple(vec![s("a"), s("b")])));
    assert_eq!(all.get(":p/score"), Some(&Value::I64(9)));
    assert_eq!(all.len(), 4, "name, secret, tag, and score");
}

#[test]
fn entity_reads_for_a_missing_id_are_empty() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    let ent = c.entity(&rtxn, 12345);
    assert_eq!(ent.get(":p/name").unwrap(), None);
    assert_eq!(ent.get(":p/tag").unwrap(), None);
    assert!(ent.touch().unwrap().is_empty());
}

#[test]
fn assoc_and_dissoc_respect_cardinality() {
    let c = conn();
    let e = c.transact(&[TxItem::Assert(None, vec![(":p/name".into(), s("bo")), (":p/tag".into(), s("x"))])]).unwrap()[0];
    let rtxn = c.read_txn().unwrap();
    let add_tag = c.entity(&rtxn, e).assoc(":p/tag", s("y"));
    let replace_score = c.entity(&rtxn, e).assoc(":p/score", Value::I64(3));
    drop(rtxn);
    c.transact(&[add_tag, replace_score]).unwrap();
    let rtxn = c.read_txn().unwrap();
    let ent = c.entity(&rtxn, e);
    assert_eq!(ent.get(":p/tag").unwrap(), Some(Value::Tuple(vec![s("x"), s("y")])), "many adds");
    assert_eq!(ent.get(":p/score").unwrap(), Some(Value::I64(3)));
    drop(rtxn);

    let rtxn = c.read_txn().unwrap();
    let drop_tags = c.entity(&rtxn, e).dissoc(":p/tag");
    drop(rtxn);
    c.transact(&[drop_tags]).unwrap();
    let rtxn = c.read_txn().unwrap();
    assert_eq!(c.entity(&rtxn, e).get(":p/tag").unwrap(), None);
    assert_eq!(c.entity(&rtxn, e).get(":p/name").unwrap(), Some(s("bo")), "other attributes stay");
}

#[test]
fn lookup_by_unique_attribute_and_by_missing_value() {
    let c = conn();
    let e = c.transact(&[TxItem::Assert(None, vec![(":p/name".into(), s("cy"))])]).unwrap()[0];
    let rtxn = c.read_txn().unwrap();
    assert_eq!(c.entity_by_lookup(&rtxn, ":p/name", &s("cy")).unwrap().map(|x| x.eid()), Some(e));
    assert!(c.entity_by_lookup(&rtxn, ":p/name", &s("nobody")).unwrap().is_none());
}
