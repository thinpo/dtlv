use dtlv_datalog::{idoc_value, pattern_predicates, AttrSpec, Conn, PathSeg, TxItem};
use serde_json::json;

fn key(k: &str) -> PathSeg {
    PathSeg::Key(k.into())
}

fn count(conn: &Conn, pattern: serde_json::Value) -> usize {
    let rtxn = conn.read_txn().unwrap();
    conn.idoc_match(&rtxn, Some(":doc/edn"), &pattern_predicates(&pattern).unwrap()).unwrap().len()
}

fn conn(indexed: Option<Vec<Vec<PathSeg>>>, excluded: Vec<Vec<PathSeg>>) -> Conn {
    let mut spec = AttrSpec::new(":doc/edn").idoc().idoc_excluded_paths(excluded);
    if let Some(i) = indexed {
        spec = spec.idoc_indexed_paths(i);
    }
    let conn = Conn::open_mem(vec![spec]).unwrap();
    conn.transact(&[TxItem::Assert(
        None,
        vec![(
            ":doc/edn".into(),
            idoc_value(r#"{"status":"active","profile":{"age":30,"raw":"x"},"tags":["a","b"]}"#).unwrap(),
        )],
    )])
    .unwrap();
    conn
}

#[test]
fn only_indexed_paths_are_searchable() {
    let c = conn(Some(vec![vec![key("status")], vec![key("profile")]]), vec![]);
    assert_eq!(count(&c, json!({"status": "active"})), 1);
    assert_eq!(count(&c, json!({"profile": {"age": 30}})), 1);
    assert_eq!(count(&c, json!({"tags": "a"})), 0, "tags is not in the indexed paths");
}

#[test]
fn excluded_paths_win_over_indexed_ones() {
    let c = conn(None, vec![vec![key("profile"), key("raw")]]);
    assert_eq!(count(&c, json!({"status": "active"})), 1);
    assert_eq!(count(&c, json!({"profile": {"raw": "x"}})), 0);
    assert_eq!(count(&c, json!({"profile": {"age": 30}})), 1);
}

#[test]
fn selectors_without_positions_cover_array_elements() {
    let c = conn(Some(vec![vec![key("tags")]]), vec![]);
    assert_eq!(count(&c, json!({"tags": "b"})), 1);
    assert_eq!(count(&c, json!({"status": "active"})), 0);
}
