use std::cell::RefCell;
use std::rc::Rc;

use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, TxDatum, TxItem, TxReport};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

#[test]
fn listener_sees_added_and_retracted_datoms_after_commit() {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":p/name"), AttrSpec::new(":p/tag").many()]).unwrap();
    let seen: Rc<RefCell<Vec<TxReport>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = seen.clone();
    conn.listen("log", move |r| sink.borrow_mut().push(r.clone()));

    let e = conn.transact(&[TxItem::Assert(None, vec![(":p/name".into(), s("Ann"))])]).unwrap()[0];
    // Replacing a cardinality-one value reports the retraction of the old one and the addition of the new.
    conn.transact(&[TxItem::Assert(Some(e), vec![(":p/name".into(), s("Bob"))])]).unwrap();

    let reports = seen.borrow();
    assert_eq!(reports.len(), 2);
    assert_eq!(
        reports[0].tx_data,
        vec![TxDatum { eid: e, attr: ":p/name".into(), value: s("Ann"), added: true }]
    );
    assert_eq!(
        reports[1].tx_data,
        vec![
            TxDatum { eid: e, attr: ":p/name".into(), value: s("Ann"), added: false },
            TxDatum { eid: e, attr: ":p/name".into(), value: s("Bob"), added: true },
        ]
    );
}

#[test]
fn listeners_are_not_called_for_empty_or_failed_transactions_and_can_be_removed() {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":p/name").unique_value()]).unwrap();
    let calls = Rc::new(RefCell::new(0));
    let c = calls.clone();
    conn.listen("count", move |_| *c.borrow_mut() += 1);

    conn.transact(&[]).unwrap();
    assert!(conn.transact(&[TxItem::Assert(None, vec![(":p/name".into(), s("x"))])]).is_ok());
    // A unique violation aborts the transaction, so the listener is not called for it.
    let err = conn.transact(&[TxItem::Assert(None, vec![(":p/name".into(), s("x"))])]);
    assert!(matches!(err, Err(DatalogError::UniqueViolation { .. })));
    assert_eq!(*calls.borrow(), 1);

    assert!(conn.unlisten("count"));
    assert!(!conn.unlisten("count"));
    conn.transact(&[TxItem::Assert(None, vec![(":p/name".into(), s("y"))])]).unwrap();
    assert_eq!(*calls.borrow(), 1);
}

#[test]
fn bulk_load_is_one_transaction_and_later_ids_do_not_collide() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":p/name"), AttrSpec::new(":p/age")]).unwrap();
    conn.load_datoms(&[
        (10, ":p/name".into(), s("Ann")),
        (10, ":p/age".into(), Value::I64(30)),
        (20, ":p/name".into(), s("Bob")),
    ])
    .unwrap();
    let fresh = conn.transact(&[TxItem::Assert(None, vec![(":p/name".into(), s("New"))])]).unwrap()[0];
    assert!(fresh > 20, "fresh id {fresh} should be above loaded ids");
    let rtxn = conn.read_txn().unwrap();
    assert_eq!(conn.get_attr(&rtxn, 10, ":p/age").unwrap(), Some(Value::I64(30)));
    drop(rtxn);

    // A failing row rolls back the whole load.
    let err = conn.load_datoms(&[(30, ":p/name".into(), s("Cy")), (31, ":p/unknown".into(), s("?"))]);
    assert!(err.is_err());
    let rtxn = conn.read_txn().unwrap();
    assert_eq!(conn.lookup_entity_by_av(&rtxn, ":p/name", &s("Cy")).unwrap(), None);
}
