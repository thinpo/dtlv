use dtlv_datalog::{idoc_value, pattern_predicates, AttrSpec, Conn, PathSeg, TxItem};
use serde_json::json;

fn key(k: &str) -> PathSeg {
    PathSeg::Key(k.into())
}

fn count(conn: &Conn, path: Vec<PathSeg>, val: &str) -> usize {
    let rtxn = conn.read_txn().unwrap();
    let preds = vec![(path, dtlv_codec::Value::Str(val.into()))];
    conn.idoc_match(&rtxn, Some(":d/doc"), &preds).unwrap().len()
}

fn conn() -> Conn {
    let conn = Conn::open_mem(vec![AttrSpec::new(":d/doc").idoc()]).unwrap();
    let doc = json!({"a": [
        {"b": [{"c": "x"}]},
        {"b": [{"c": "y"}, {"c": "z"}]}
    ]});
    conn.transact(&[TxItem::Assert(None, vec![(":d/doc".into(), idoc_value(&doc.to_string()).unwrap())])]).unwrap();
    conn
}

#[test]
fn named_position_at_one_level_and_free_at_another() {
    let c = conn();
    // Second element of `a`, any element of its `b`: finds "y" and "z", not "x".
    assert_eq!(count(&c, vec![key("a"), PathSeg::Index(1), key("b"), key("c")], "y"), 1);
    assert_eq!(count(&c, vec![key("a"), PathSeg::Index(1), key("b"), key("c")], "z"), 1);
    assert_eq!(count(&c, vec![key("a"), PathSeg::Index(1), key("b"), key("c")], "x"), 0);
}

#[test]
fn fully_named_and_fully_free_paths_still_match() {
    let c = conn();
    assert_eq!(count(&c, vec![key("a"), PathSeg::Index(0), key("b"), PathSeg::Index(0), key("c")], "x"), 1);
    assert_eq!(count(&c, vec![key("a"), key("b"), key("c")], "y"), 1);
    assert_eq!(count(&c, vec![key("a"), PathSeg::Index(0), key("b"), key("c")], "y"), 0);
}

#[test]
fn json_patterns_match_free_positions() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    let preds = pattern_predicates(&json!({"a": {"b": {"c": "z"}}})).unwrap();
    assert_eq!(c.idoc_match(&rtxn, Some(":d/doc"), &preds).unwrap().len(), 1);
}

#[test]
fn deep_documents_fall_back_to_the_extremes_without_blowing_up() {
    // Seven array levels: the index keeps only the all-named and all-free forms for this leaf.
    let mut v = json!("deep");
    for _ in 0..7 {
        v = json!([v]);
    }
    let conn = Conn::open_mem(vec![AttrSpec::new(":d/doc").idoc()]).unwrap();
    conn.transact(&[TxItem::Assert(None, vec![(":d/doc".into(), idoc_value(&json!({"deep": v}).to_string()).unwrap())])])
        .unwrap();
    let mut path = vec![key("deep")];
    path.extend(std::iter::repeat(PathSeg::Index(0)).take(7));
    assert_eq!(count(&conn, path, "deep"), 1);
}
