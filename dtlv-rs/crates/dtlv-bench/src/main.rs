use std::time::Instant;
use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, Pattern, Pred, Query, Term, TxItem};
use tempfile::tempdir;

const NUM_SECURITIES: usize = 2000;
const NUM_WARMUP: usize = 500;
const NUM_SAMPLES: usize = 2000;

fn make_schema() -> Vec<AttrSpec> {
    vec![
        AttrSpec::new(":sec/id").unique_identity(),
        AttrSpec::new(":sec/ticker").unique_identity(),
        AttrSpec::new(":sec/isin").indexed(),
        AttrSpec::new(":sec/cusip"),
        AttrSpec::new(":sec/name").fulltext(),
        AttrSpec::new(":sec/exchange"),
        AttrSpec::new(":sec/currency"),
        AttrSpec::new(":sec/lot-size"),
        AttrSpec::new(":sec/tick-size"),
        AttrSpec::new(":sec/split-factor"),
        AttrSpec::new(":sec/status"),
        // Bitemporal version-entity attributes
        AttrSpec::new(":sec-ver/sec-id").indexed(),
        AttrSpec::new(":sec-ver/valid-from"),
        AttrSpec::new(":sec-ver/valid-to"),
        AttrSpec::new(":sec-ver/tx-from"),
        AttrSpec::new(":sec-ver/tx-to"),
        AttrSpec::new(":sec-ver/split-factor"),
    ]
}

fn make_securities(n: usize) -> Vec<TxItem> {
    let exchanges = ["XNAS", "XNYS", "XLON", "XTKS", "XHKG"];
    let currencies = ["USD", "USD", "GBP", "JPY", "HKD"];
    (0..n)
        .map(|i| {
            let ex_idx = i % exchanges.len();
            let ticker = if i == 0 {
                "AAPL".to_string()
            } else {
                format!("T{:05}", i)
            };
            let isin = if i == 0 {
                "US0378331005".to_string()
            } else {
                format!("US{:010}", 1_000_000_000 + i)
            };
            let cusip = if i == 0 {
                "037833100".to_string()
            } else {
                format!("{:09}", 100_000_000 + i)
            };
            let name = if i == 0 {
                "Apple Inc.".to_string()
            } else {
                format!("Global Quant Holdings {:05} Corp", i)
            };
            TxItem::Assert(
                None,
                vec![
                    (":sec/id".into(), Value::Str(format!("SEC-{:05}", i))),
                    (":sec/ticker".into(), Value::Str(ticker)),
                    (":sec/isin".into(), Value::Str(isin)),
                    (":sec/cusip".into(), Value::Str(cusip)),
                    (":sec/name".into(), Value::Str(name)),
                    (":sec/exchange".into(), Value::Str(exchanges[ex_idx].into())),
                    (":sec/currency".into(), Value::Str(currencies[ex_idx].into())),
                    (":sec/lot-size".into(), Value::I64(if i % 10 == 0 { 100 } else { 1 })),
                    (":sec/tick-size".into(), Value::F64(0.01)),
                    (":sec/split-factor".into(), Value::F64(1.0)),
                    (":sec/status".into(), Value::Str("ACTIVE".into())),
                ],
            )
        })
        .collect()
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let idx = ((sorted.len() as f64) * p).floor() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn bench<F: FnMut(&str)>(tickers: &[String], mut f: F) -> (f64, f64, f64) {
    for i in 0..NUM_WARMUP {
        f(&tickers[i % tickers.len()]);
    }
    let mut times_us = Vec::with_capacity(NUM_SAMPLES);
    for i in 0..NUM_SAMPLES {
        let t = &tickers[i % tickers.len()];
        let t0 = Instant::now();
        f(t);
        times_us.push(t0.elapsed().as_nanos() as f64 / 1000.0);
    }
    times_us.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (
        percentile(&times_us, 0.50),
        percentile(&times_us, 0.95),
        percentile(&times_us, 0.99),
    )
}

fn run_suite(label: &str, conn: &Conn, securities: &[TxItem], tickers: &[String]) {
    // 1. SOD batch ingestion
    let t0 = Instant::now();
    let eids = conn.transact(securities).unwrap();
    let ingest_ms = t0.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(eids.len(), NUM_SECURITIES);

    let rtxn = conn.read_txn().unwrap();

    // 2a. Zero-copy direct index read (lookup_entity_by_av + get_attr)
    let (direct_p50, direct_p95, direct_p99) = bench(tickers, |t| {
        let eid = conn
            .lookup_entity_by_av(&rtxn, ":sec/ticker", &Value::Str(t.to_string()))
            .unwrap()
            .unwrap();
        let _isin = conn.get_attr(&rtxn, eid, ":sec/isin").unwrap().unwrap();
        let _lot = conn.get_attr(&rtxn, eid, ":sec/lot-size").unwrap().unwrap();
        let _sf = conn.get_attr(&rtxn, eid, ":sec/split-factor").unwrap().unwrap();
    });

    // 2b. Datalog join query (conn.q)
    let (q_p50, q_p95, q_p99) = bench(tickers, |t| {
        let q = Query {
            find: vec!["?isin".into(), "?lot".into(), "?sf".into()],
            patterns: vec![
                Pattern::Datom(
                    Term::Var("?e".into()),
                    ":sec/ticker".into(),
                    Term::Val(Value::Str(t.to_string())),
                ),
                Pattern::Datom(
                    Term::Var("?e".into()),
                    ":sec/isin".into(),
                    Term::Var("?isin".into()),
                ),
                Pattern::Datom(
                    Term::Var("?e".into()),
                    ":sec/lot-size".into(),
                    Term::Var("?lot".into()),
                ),
                Pattern::Datom(
                    Term::Var("?e".into()),
                    ":sec/split-factor".into(),
                    Term::Var("?sf".into()),
                ),
            ],
            preds: vec![],
        };
        let rows = conn.q(&rtxn, &q).unwrap();
        assert_eq!(rows.len(), 1);
    });

    // 3. Full-text search
    let fts_hits = conn.fulltext_search(&rtxn, "Apple", 5).unwrap();
    assert!(!fts_hits.is_empty());
    assert_eq!(
        conn.get_attr(&rtxn, fts_hits[0].0, ":sec/ticker").unwrap(),
        Some(Value::Str("AAPL".into()))
    );

    // 4. Bitemporal corporate action check
    drop(rtxn);
    let v_eff = 1790812800000_i64; // 2026-10-01
    let v_end = 253402214400000_i64; // 9999-12-31
    let t1 = 1790931600000_i64; // 2026-10-02
    let t2 = 1791018000000_i64; // 2026-10-03
    let v1_ids = conn
        .transact(&[TxItem::Assert(
            None,
            vec![
                (":sec-ver/sec-id".into(), Value::Str("SEC-00000".into())),
                (":sec-ver/valid-from".into(), Value::Instant(v_eff)),
                (":sec-ver/valid-to".into(), Value::Instant(v_end)),
                (":sec-ver/tx-from".into(), Value::Instant(t1)),
                (":sec-ver/tx-to".into(), Value::Instant(v_end)),
                (":sec-ver/split-factor".into(), Value::F64(2.0)),
            ],
        )])
        .unwrap();
    conn.transact(&[
        TxItem::Assert(Some(v1_ids[0]), vec![(":sec-ver/tx-to".into(), Value::Instant(t2))]),
        TxItem::Assert(
            None,
            vec![
                (":sec-ver/sec-id".into(), Value::Str("SEC-00000".into())),
                (":sec-ver/valid-from".into(), Value::Instant(v_eff)),
                (":sec-ver/valid-to".into(), Value::Instant(v_end)),
                (":sec-ver/tx-from".into(), Value::Instant(t2)),
                (":sec-ver/tx-to".into(), Value::Instant(v_end)),
                (":sec-ver/split-factor".into(), Value::F64(4.0)),
            ],
        ),
    ])
    .unwrap();

    let rtxn2 = conn.read_txn().unwrap();
    let bitemp_q = Query {
        find: vec!["?sf".into()],
        patterns: vec![
            Pattern::Datom(Term::Var("?e".into()), ":sec-ver/sec-id".into(), Term::Val(Value::Str("SEC-00000".into()))),
            Pattern::Datom(Term::Var("?e".into()), ":sec-ver/tx-from".into(), Term::Var("?tf".into())),
            Pattern::Datom(Term::Var("?e".into()), ":sec-ver/tx-to".into(), Term::Var("?tt".into())),
            Pattern::Datom(Term::Var("?e".into()), ":sec-ver/split-factor".into(), Term::Var("?sf".into())),
        ],
        preds: vec![
            Pred::Le("?tf".into(), "?tat".into()),
            Pred::Gt("?tt".into(), "?tat".into()),
        ],
    };
    let _ = bitemp_q;
    let all_vers = conn
        .q(
            &rtxn2,
            &Query {
                find: vec!["?tf".into(), "?tt".into(), "?sf".into()],
                patterns: vec![
                    Pattern::Datom(Term::Var("?e".into()), ":sec-ver/sec-id".into(), Term::Val(Value::Str("SEC-00000".into()))),
                    Pattern::Datom(Term::Var("?e".into()), ":sec-ver/tx-from".into(), Term::Var("?tf".into())),
                    Pattern::Datom(Term::Var("?e".into()), ":sec-ver/tx-to".into(), Term::Var("?tt".into())),
                    Pattern::Datom(Term::Var("?e".into()), ":sec-ver/split-factor".into(), Term::Var("?sf".into())),
                ],
                preds: vec![],
            },
        )
        .unwrap();
    assert_eq!(all_vers.len(), 2);

    // 5. Google Doc Sec 5.2: PreparedPull & PreparedQuery hot-path latency
    let pp = conn.prepare_pull(vec![
        dtlv_datalog::PullSelector::Attr(":sec/isin".into()),
        dtlv_datalog::PullSelector::Attr(":sec/lot-size".into()),
        dtlv_datalog::PullSelector::Attr(":sec/split-factor".into()),
    ]);
    let (pp_p50, pp_p95, pp_p99) = bench(tickers, |t| {
        let m = pp
            .execute_by_lookup(&rtxn2, ":sec/ticker", &Value::Str(t.to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(m.len(), 3);
    });

    let pq = conn
        .prepare_query(
            r#"[:find [?isin ?lot ?sf] :in $ ?t :where [?e :sec/ticker ?t] [?e :sec/isin ?isin] [?e :sec/lot-size ?lot] [?e :sec/split-factor ?sf]]"#,
            "[]",
        )
        .unwrap();
    let (pq_p50, pq_p95, pq_p99) = bench(tickers, |t| {
        let res = pq.execute(&rtxn2, &[Value::Str(t.to_string())]).unwrap();
        assert!(matches!(res, dtlv_datalog::QueryResult::Tuple(Some(_))));
    });

    println!("\n================ {} BENCHMARK RESULTS ================", label);
    println!(
        "SOD Batch Load (2,000 securities): {:.1} ms ({:.0} rec/sec)",
        ingest_ms,
        (NUM_SECURITIES as f64) / (ingest_ms / 1000.0)
    );
    println!(
        "Direct Index Read (p50 / p95 / p99): {:.2} us / {:.2} us / {:.2} us",
        direct_p50, direct_p95, direct_p99
    );
    println!(
        "PreparedPull      (p50 / p95 / p99): {:.2} us / {:.2} us / {:.2} us",
        pp_p50, pp_p95, pp_p99
    );
    println!(
        "PreparedQuery EDN (p50 / p95 / p99): {:.2} us / {:.2} us / {:.2} us",
        pq_p50, pq_p95, pq_p99
    );
    println!(
        "Datalog Join Query (p50 / p95 / p99): {:.2} us / {:.2} us / {:.2} us",
        q_p50, q_p95, q_p99
    );
    println!("==================================================================");
}

fn main() {
    let dir = tempdir().unwrap();
    let securities = make_securities(NUM_SECURITIES);
    let tickers: Vec<String> = (0..200)
        .map(|i| if i == 0 { "AAPL".into() } else { format!("T{:05}", i) })
        .collect();

    let lmdb_conn = Conn::open(dir.path().join("lmdb"), make_schema()).unwrap();
    run_suite("dtlv-rs (LMDB)", &lmdb_conn, &securities, &tickers);

    let snap_t0 = Instant::now();
    let rtxn = lmdb_conn.read_txn().unwrap();
    lmdb_conn
        .save_to_sqlite(&rtxn, dir.path().join("snapshot.sqlite"))
        .unwrap();
    let snap_ms = snap_t0.elapsed().as_secs_f64() * 1000.0;
    println!("LMDB -> SQLite Snapshot Export (2,000 securities): {:.1} ms", snap_ms);

    let sqlite_conn = Conn::open_sqlite(dir.path().join("direct.sqlite"), make_schema()).unwrap();
    run_suite("dtlv-rs (SQLite)", &sqlite_conn, &securities, &tickers);

    let redb_conn = Conn::open_redb(dir.path().join("direct.redb"), make_schema()).unwrap();
    run_suite("dtlv-rs (redb)", &redb_conn, &securities, &tickers);

    let mem_conn = Conn::open_mem(make_schema()).unwrap();
    run_suite("dtlv-rs (Mem BTreeMap)", &mem_conn, &securities, &tickers);
}
