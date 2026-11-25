use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, DatalogError, QueryResult, TxItem};

fn conn() -> Conn {
    let conn = Conn::open_mem(vec![AttrSpec::new(":x/name")]).unwrap();
    let items: Vec<TxItem> = ["100%", "100x", "a_b", "a!b", "ab"]
        .iter()
        .map(|n| TxItem::Assert(None, vec![(":x/name".into(), Value::Str((*n).into()))]))
        .collect();
    conn.transact(&items).unwrap();
    conn
}

fn names(conn: &Conn, q: &str) -> Vec<String> {
    let rtxn = conn.read_txn().unwrap();
    let mut out: Vec<String> = match conn.q_str(&rtxn, q, &[]).unwrap() {
        QueryResult::Coll(v) => v.into_iter().map(|x| x.as_str().unwrap().to_owned()).collect(),
        other => panic!("expected Coll, got {other:?}"),
    };
    out.sort();
    out
}

#[test]
fn default_escape_is_bang() {
    let c = conn();
    // `!%` is a literal percent sign, so only "100%" matches.
    assert_eq!(names(&c, r#"[:find [?n ...] :where [?e :x/name ?n] [(like ?n "100!%")]]"#), vec!["100%"]);
    // `!!` is a literal bang: only "a!b" matches.
    assert_eq!(names(&c, r#"[:find [?n ...] :where [?e :x/name ?n] [(like ?n "a!!b")]]"#), vec!["a!b"]);
    // Unescaped `_` is still one character.
    assert_eq!(names(&c, r#"[:find [?n ...] :where [?e :x/name ?n] [(like ?n "a_b")]]"#), vec!["a!b", "a_b"]);
}

#[test]
fn custom_escape_option_and_not_like() {
    let c = conn();
    assert_eq!(
        names(&c, r#"[:find [?n ...] :where [?e :x/name ?n] [(like ?n "100|%" {:escape "|"})]]"#),
        vec!["100%"]
    );
    // With `|` as the escape, `!` is an ordinary character.
    assert_eq!(
        names(&c, r#"[:find [?n ...] :where [?e :x/name ?n] [(not-like ?n "a!b" {:escape "|"})]]"#),
        vec!["100%", "100x", "a_b", "ab"]
    );
}

#[test]
fn invalid_escape_in_a_constant_pattern_is_rejected() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    let err = c.q_str(&rtxn, r#"[:find [?n ...] :where [?e :x/name ?n] [(like ?n "a!b")]]"#, &[]).unwrap_err();
    assert!(matches!(err, DatalogError::Parse(_)), "got {err:?}");
}
