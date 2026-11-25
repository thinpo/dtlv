use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, Durability, EntityRef, QueryResult, TxItem, TxView, TxVal};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn schema() -> Vec<AttrSpec> {
    vec![AttrSpec::new(":acct/owner"), AttrSpec::new(":acct/balance"), AttrSpec::new(":acct/tags").many()]
}

fn non_negative(v: &TxView<'_, '_>, args: &[Value]) -> Result<bool, DatalogError> {
    match args.first() {
        Some(Value::Ref(eid)) => Ok(matches!(v.get_attr(*eid, ":acct/balance")?, Some(Value::I64(b)) if b >= 0)),
        _ => Ok(false),
    }
}

fn has_two_tags(v: &TxView<'_, '_>, args: &[Value]) -> Result<bool, DatalogError> {
    match args.first() {
        Some(Value::Ref(eid)) => Ok(v.get_all(*eid, ":acct/tags")?.len() == 2),
        _ => Ok(false),
    }
}

fn conn() -> Conn {
    let mut c = Conn::open_mem(schema()).unwrap();
    c.register_ensure_fn("non-negative", non_negative);
    c.register_ensure_fn("two-tags", has_two_tags);
    c
}

fn open_account(owner: &str, balance: i64) -> Vec<TxItem> {
    vec![
        TxItem::AssertRef(
            EntityRef::TempId("a".into()),
            vec![
                (":acct/owner".into(), TxVal::Val(s(owner))),
                (":acct/balance".into(), TxVal::Val(Value::I64(balance))),
            ],
        ),
        TxItem::Ensure("non-negative".into(), vec![TxVal::TempRef("a".into())]),
    ]
}

fn rows(conn: &Conn, q: &str) -> Vec<Vec<Value>> {
    let rtxn = conn.read_txn().unwrap();
    match conn.q_str(&rtxn, q, &[]).unwrap() {
        QueryResult::Rel(r) => r,
        other => panic!("expected Rel, got {other:?}"),
    }
}

fn owner_count(conn: &Conn, owner: &str) -> usize {
    rows(conn, &format!("[:find ?e :where [?e :acct/owner \"{owner}\"]]")).len()
}

fn eid_of(conn: &Conn, owner: &str) -> Value {
    rows(conn, &format!("[:find ?e :where [?e :acct/owner \"{owner}\"]]"))[0][0].clone()
}

fn balance_of(conn: &Conn, owner: &str) -> Option<Value> {
    rows(conn, &format!("[:find ?b :where [?e :acct/owner \"{owner}\"] [?e :acct/balance ?b]]")).into_iter().next().map(|r| r[0].clone())
}

#[test]
fn ensure_passes_and_commits() {
    let c = conn();
    c.transact(&open_account("alice", 10)).unwrap();
    assert_eq!(owner_count(&c, "alice"), 1);
}

#[test]
fn ensure_failure_aborts_creation() {
    let c = conn();
    let err = c.transact(&open_account("bob", -5)).unwrap_err();
    assert!(matches!(err, DatalogError::EnsureFailed { ref pred, .. } if pred == "non-negative"), "{err:?}");
    assert_eq!(owner_count(&c, "bob"), 0);
}

#[test]
fn ensure_sees_transaction_writes() {
    let c = conn();
    c.transact(&open_account("carol", 10)).unwrap();
    let Value::Ref(eid) = eid_of(&c, "carol") else { panic!("expected ref") };
    // Set the balance to -1 and ensure in the same transaction: must fail, balance stays 10.
    let set = |bal: i64| {
        vec![
            TxItem::Assert(Some(eid), vec![(":acct/balance".into(), Value::I64(bal))]),
            TxItem::Ensure("non-negative".into(), vec![TxVal::Val(Value::Ref(eid))]),
        ]
    };
    assert!(matches!(c.transact(&set(-1)), Err(DatalogError::EnsureFailed { .. })));
    assert_eq!(balance_of(&c, "carol"), Some(Value::I64(10)));
    c.transact(&set(20)).unwrap();
    assert_eq!(balance_of(&c, "carol"), Some(Value::I64(20)));
}

#[test]
fn unknown_predicate_commits_nothing() {
    let c = conn();
    let items = vec![
        TxItem::AssertRef(EntityRef::TempId("a".into()), vec![(":acct/owner".into(), TxVal::Val(s("dan")))]),
        TxItem::Ensure("no-such-pred".into(), vec![TxVal::TempRef("a".into())]),
    ];
    let err = c.transact(&items).unwrap_err();
    assert!(matches!(err, DatalogError::UnknownEnsureFn(ref n) if n == "no-such-pred"), "{err:?}");
    assert_eq!(owner_count(&c, "dan"), 0);
}

#[test]
fn unresolved_tempid_commits_nothing() {
    let c = conn();
    let items = vec![
        TxItem::AssertRef(EntityRef::TempId("a".into()), vec![(":acct/owner".into(), TxVal::Val(s("erin")))]),
        TxItem::Ensure("non-negative".into(), vec![TxVal::TempRef("missing".into())]),
    ];
    assert!(c.transact(&items).is_err());
    assert_eq!(owner_count(&c, "erin"), 0);
}

#[test]
fn tx_fn_can_return_ensure() {
    let mut c = conn();
    c.register_tx_fn("open", |_, args| match args.first() {
        Some(Value::I64(b)) => Ok(open_account("fay", *b)),
        _ => Ok(vec![]),
    });
    let call = |b: i64| vec![TxItem::Call("open".into(), vec![Value::I64(b)])];
    assert!(matches!(c.transact(&call(-5)), Err(DatalogError::EnsureFailed { .. })));
    assert_eq!(owner_count(&c, "fay"), 0);
    c.transact(&call(5)).unwrap();
    assert_eq!(owner_count(&c, "fay"), 1);
}

#[test]
fn get_all_sees_both_tags_inside_ensure() {
    let c = conn();
    let items = vec![
        TxItem::AssertRef(
            EntityRef::TempId("a".into()),
            vec![
                (":acct/owner".into(), TxVal::Val(s("gina"))),
                (":acct/tags".into(), TxVal::Val(s("x"))),
                (":acct/tags".into(), TxVal::Val(s("y"))),
            ],
        ),
        TxItem::Ensure("two-tags".into(), vec![TxVal::TempRef("a".into())]),
    ];
    c.transact(&items).unwrap();
    assert_eq!(owner_count(&c, "gina"), 1);
}

#[test]
fn committed_ensure_tx_reopens_without_predicate() {
    let db = tempfile::tempdir().unwrap();
    let path = db.path().join("db");
    {
        let c = Conn::open_wal(&path, db.path(), schema(), Durability::Strict).unwrap();
        let mut c = c;
        c.register_ensure_fn("non-negative", non_negative);
        c.transact(&open_account("hal", 7)).unwrap();
    }
    // Reopen without registering "non-negative": replay must not need it.
    let c = Conn::open_wal(&path, db.path(), schema(), Durability::Strict).unwrap();
    assert_eq!(owner_count(&c, "hal"), 1);
}
