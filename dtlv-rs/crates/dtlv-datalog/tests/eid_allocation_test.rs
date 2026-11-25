use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, TxItem};

#[test]
fn explicit_eid_does_not_collide_with_later_allocation() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":n/v")]).unwrap();
    conn.transact(&[TxItem::Assert(Some(1), vec![(":n/v".into(), Value::I64(10))])]).unwrap();
    let new = conn.transact(&[TxItem::Assert(None, vec![(":n/v".into(), Value::I64(20))])]).unwrap()[0];
    assert_ne!(new, 1, "fresh entity reused explicit eid 1");
}
