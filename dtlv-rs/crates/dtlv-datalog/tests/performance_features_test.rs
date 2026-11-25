//! Coarse guards for features outside the core paths: phrase search, idoc patching, and WAL recovery.
//! Bounds are generous for debug builds; timings print with `--nocapture`.

use std::time::{Duration, Instant};

use dtlv_codec::Value;
use dtlv_datalog::{idoc_value, AttrSpec, Conn, Durability, FtsQuery, FulltextConfig, PatchOp, PathSeg, TxItem};
use serde_json::json;

fn report(label: &str, elapsed: Duration, bound: Duration) {
    println!("{label}: {elapsed:?} (bound {bound:?})");
    assert!(elapsed < bound, "{label} took {elapsed:?}, over {bound:?}");
}

#[test]
fn phrase_queries_over_thousands_of_documents() {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":d/t").fulltext()]).unwrap();
    conn.set_fulltext_config(FulltextConfig { index_positions: true, ..FulltextConfig::default() }).unwrap();
    let items: Vec<TxItem> = (0..5_000)
        .map(|i| {
            let text = if i % 50 == 0 { "the quick brown fox jumps" } else { "a slow green turtle walks" };
            TxItem::Assert(None, vec![(":d/t".into(), Value::Str(format!("{text} {i}")))])
        })
        .collect();
    conn.transact(&items).unwrap();
    let rtxn = conn.read_txn().unwrap();
    let t = Instant::now();
    let hits = conn.fulltext_query(&rtxn, &FtsQuery::Phrase("quick brown fox".into()), 10_000).unwrap();
    report("phrase query over 5k documents", t.elapsed(), Duration::from_secs(5));
    assert_eq!(hits.len(), 100);
}

#[test]
fn patches_on_a_large_document_stay_fast() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":d/doc").idoc()]).unwrap();
    let big: serde_json::Value = json!({"items": (0..2_000).map(|i| json!({"n": i})).collect::<Vec<_>>(), "status": "a"});
    let e = conn
        .transact(&[TxItem::Assert(None, vec![(":d/doc".into(), idoc_value(&big.to_string()).unwrap())])])
        .unwrap()[0];
    let t = Instant::now();
    for i in 0..50 {
        conn.transact(&[TxItem::PatchIdoc(
            e,
            ":d/doc".into(),
            vec![PatchOp::Set(vec![PathSeg::Key("status".into())], json!(format!("s{i}")))],
        )])
        .unwrap();
    }
    report("50 patches on a 2k-element document", t.elapsed(), Duration::from_secs(60));
}

#[test]
fn wal_replay_after_a_crash_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    {
        let conn = Conn::open_wal(&db, dir.path(), vec![AttrSpec::new(":w/n")], Durability::Relaxed).unwrap();
        let items: Vec<TxItem> = (0..2_000).map(|i| TxItem::Assert(None, vec![(":w/n".into(), Value::I64(i))])).collect();
        conn.transact(&items).unwrap();
    }
    // Simulate losing the store: reopen with a fresh database directory, so every record must be replayed.
    let fresh = dir.path().join("fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    std::fs::copy(dir.path().join("wal.log"), fresh.join("wal.log")).unwrap();
    let t = Instant::now();
    let conn = Conn::open_wal(fresh.join("db"), &fresh, vec![AttrSpec::new(":w/n")], Durability::Relaxed).unwrap();
    report("replay of 2k logged transactions into an empty store", t.elapsed(), Duration::from_secs(120));
    let rtxn = conn.read_txn().unwrap();
    assert_eq!(conn.search_datoms(&rtxn, None, Some(":w/n"), None).unwrap().len(), 2_000);
}
