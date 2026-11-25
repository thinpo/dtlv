use std::time::{Duration, Instant};

use dtlv_codec::Value;
use dtlv_datalog::{idoc_value, pattern_predicates, AttrSpec, Conn, TxItem};
use serde_json::json;
use tempfile::tempdir;

fn schema() -> Vec<AttrSpec> {
    vec![AttrSpec::new(":a/doc").idoc().idoc_async(), AttrSpec::new(":s/doc").idoc()]
}

fn doc(status: &str) -> Value {
    idoc_value(&json!({"status": status}).to_string()).unwrap()
}

fn count(conn: &Conn, attr: &str, status: &str) -> usize {
    let rtxn = conn.read_txn().unwrap();
    let preds = pattern_predicates(&json!({"status": status})).unwrap();
    conn.idoc_match(&rtxn, Some(attr), &preds).unwrap().len()
}

#[test]
fn documents_are_visible_to_queries_only_after_their_job_runs() {
    let conn = Conn::open_mem(schema()).unwrap();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":a/doc".into(), doc("on"))])]).unwrap()[0];
    // The source value is there immediately...
    let rtxn = conn.read_txn().unwrap();
    assert_eq!(conn.get_attr(&rtxn, e, ":a/doc").unwrap(), Some(doc("on")));
    drop(rtxn);
    // ...but the index has not seen it yet.
    assert_eq!(count(&conn, ":a/doc", "on"), 0);
    assert_eq!(conn.idoc_jobs_pending().unwrap(), 1);
    assert_eq!(conn.process_idoc_jobs().unwrap(), 1);
    assert_eq!(count(&conn, ":a/doc", "on"), 1);
    assert_eq!(conn.idoc_jobs_pending().unwrap(), 0);
}

#[test]
fn a_replacement_is_visible_only_after_processing_and_the_old_document_leaves_the_index() {
    let conn = Conn::open_mem(schema()).unwrap();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":a/doc".into(), doc("old"))])]).unwrap()[0];
    conn.process_idoc_jobs().unwrap();
    conn.transact(&[TxItem::Assert(Some(e), vec![(":a/doc".into(), doc("new"))])]).unwrap();
    // Stale until processed: the index still shows the old document.
    assert_eq!(count(&conn, ":a/doc", "old"), 1);
    assert_eq!(count(&conn, ":a/doc", "new"), 0);
    conn.process_idoc_jobs().unwrap();
    assert_eq!(count(&conn, ":a/doc", "old"), 0);
    assert_eq!(count(&conn, ":a/doc", "new"), 1);
}

#[test]
fn pending_jobs_survive_a_restart() {
    let dir = tempdir().unwrap();
    {
        let conn = Conn::open(dir.path().join("db"), schema()).unwrap();
        conn.transact(&[TxItem::Assert(None, vec![(":a/doc".into(), doc("kept"))])]).unwrap();
        assert_eq!(conn.idoc_jobs_pending().unwrap(), 1);
    }
    let conn = Conn::open(dir.path().join("db"), schema()).unwrap();
    assert_eq!(conn.idoc_jobs_pending().unwrap(), 1);
    conn.process_idoc_jobs().unwrap();
    assert_eq!(count(&conn, ":a/doc", "kept"), 1);
}

#[test]
fn retracting_before_processing_leaves_no_index_entries() {
    let conn = Conn::open_mem(schema()).unwrap();
    let e = conn.transact(&[TxItem::Assert(None, vec![(":a/doc".into(), doc("gone"))])]).unwrap()[0];
    conn.transact(&[TxItem::RetractEntity(e)]).unwrap();
    assert_eq!(conn.idoc_jobs_pending().unwrap(), 2);
    conn.process_idoc_jobs().unwrap();
    assert_eq!(count(&conn, ":a/doc", "gone"), 0);
}

#[test]
fn synchronous_attributes_are_still_indexed_immediately() {
    let conn = Conn::open_mem(schema()).unwrap();
    conn.transact(&[TxItem::Assert(None, vec![(":s/doc".into(), doc("now"))])]).unwrap();
    assert_eq!(count(&conn, ":s/doc", "now"), 1);
    assert_eq!(conn.idoc_jobs_pending().unwrap(), 0);
}

#[test]
fn processing_thousands_of_jobs_stays_fast() {
    let conn = Conn::open_mem(schema()).unwrap();
    let items: Vec<TxItem> = (0..2_000).map(|i| TxItem::Assert(None, vec![(":a/doc".into(), doc(if i % 2 == 0 { "even" } else { "odd" }))])).collect();
    conn.transact(&items).unwrap();
    let t = Instant::now();
    assert_eq!(conn.process_idoc_jobs().unwrap(), 2_000);
    let elapsed = t.elapsed();
    println!("process 2k idoc jobs: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(60), "took {elapsed:?}");
    assert_eq!(count(&conn, ":a/doc", "even"), 1_000);
}
