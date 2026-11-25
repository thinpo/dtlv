//! Coarse performance guards. Bounds are generous so debug builds pass on ordinary machines; they catch
//! regressions in complexity (a scan where an index should be used), not small constant factors.
//! Timings are printed with `--nocapture`.

use std::time::{Duration, Instant};

use dtlv_codec::Value;
use dtlv_datalog::{idoc_value, pattern_predicates, AttrSpec, Conn, FulltextConfig, QueryResult, TxItem};
use serde_json::json;

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn report(label: &str, elapsed: Duration, bound: Duration) {
    println!("{label}: {:?} (bound {:?})", elapsed, bound);
    assert!(elapsed < bound, "{label} took {elapsed:?}, over {bound:?}");
}

#[test]
fn bulk_transact_and_indexed_lookup_scale() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":p/id").unique_identity(), AttrSpec::new(":p/grp").indexed()]).unwrap();
    let n = 20_000;
    let items: Vec<TxItem> = (0..n)
        .map(|i| TxItem::Assert(None, vec![(":p/id".into(), Value::I64(i)), (":p/grp".into(), Value::I64(i % 100))]))
        .collect();
    let t = Instant::now();
    conn.transact(&items).unwrap();
    report("bulk transact 20k entities", t.elapsed(), Duration::from_secs(60));

    let rtxn = conn.read_txn().unwrap();
    let t = Instant::now();
    for i in 0..1000 {
        assert!(conn.lookup_entity_by_av(&rtxn, ":p/id", &Value::I64(i)).unwrap().is_some());
    }
    report("1000 unique lookups", t.elapsed(), Duration::from_secs(5));

    let t = Instant::now();
    let r = conn.q_str(&rtxn, "[:find (count ?e) . :where [?e :p/grp 7]]", &[]).unwrap();
    report("indexed count query", t.elapsed(), Duration::from_secs(5));
    assert_eq!(r, QueryResult::Scalar(Some(Value::I64(200))));
}

#[test]
fn fulltext_search_scales() {
    let mut conn = Conn::open_mem(vec![AttrSpec::new(":d/text").fulltext()]).unwrap();
    conn.set_fulltext_config(FulltextConfig::default()).unwrap();
    let items: Vec<TxItem> = (0..5_000)
        .map(|i| TxItem::Assert(None, vec![(":d/text".into(), s(&format!("document {i} about topic{} and rust", i % 50)))]))
        .collect();
    let t = Instant::now();
    conn.transact(&items).unwrap();
    report("index 5k documents", t.elapsed(), Duration::from_secs(60));

    let rtxn = conn.read_txn().unwrap();
    let t = Instant::now();
    let hits = conn.fulltext_search(&rtxn, "topic7 rust", 100).unwrap();
    report("ranked full-text query over 5k docs", t.elapsed(), Duration::from_secs(5));
    assert_eq!(hits.len(), 100);
}

#[test]
fn idoc_match_scales() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":d/doc").idoc()]).unwrap();
    let items: Vec<TxItem> = (0..5_000)
        .map(|i| {
            let doc = json!({"status": if i % 10 == 0 { "active" } else { "idle" }, "n": i, "tags": [format!("t{}", i % 20)]});
            TxItem::Assert(None, vec![(":d/doc".into(), idoc_value(&doc.to_string()).unwrap())])
        })
        .collect();
    let t = Instant::now();
    conn.transact(&items).unwrap();
    report("index 5k idoc documents", t.elapsed(), Duration::from_secs(60));

    let rtxn = conn.read_txn().unwrap();
    let preds = pattern_predicates(&json!({"status": "active", "tags": "t0"})).unwrap();
    let t = Instant::now();
    let hits = conn.idoc_match(&rtxn, Some(":d/doc"), &preds).unwrap();
    report("conjunctive idoc match over 5k docs", t.elapsed(), Duration::from_secs(5));
    assert_eq!(hits.len(), 250, "every 20th document is active and tagged t0");
}

#[test]
fn wal_append_throughput_is_reasonable() {
    let dir = tempfile::tempdir().unwrap();
    let conn = Conn::open_wal(dir.path().join("db"), dir.path(), vec![AttrSpec::new(":w/n")], dtlv_datalog::Durability::Relaxed).unwrap();
    let t = Instant::now();
    for i in 0..2_000 {
        conn.transact(&[TxItem::Assert(None, vec![(":w/n".into(), Value::I64(i))])]).unwrap();
    }
    report("2k logged single-row transactions (relaxed)", t.elapsed(), Duration::from_secs(120));
}
