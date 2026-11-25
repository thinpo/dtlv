use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, FtsQuery, TxItem};

const N: u64 = 25;

/// 25 entities whose text all contains "alpha". Entities 1..=5 share identical text (exact ties); the rest
/// repeat "alpha" a varying number of times, so their BM25 scores differ.
fn corpus() -> Conn {
    let conn = Conn::open_mem(vec![AttrSpec::new(":doc/text").fulltext()]).unwrap();
    let items: Vec<TxItem> = (1..=N)
        .map(|e| {
            let text = if e <= 5 { "alpha".to_string() } else { format!("alpha {}", "alpha ".repeat((e % 7) as usize)) };
            TxItem::Assert(Some(e), vec![(":doc/text".into(), Value::Str(text))])
        })
        .collect();
    conn.transact(&items).unwrap();
    conn
}

fn alpha() -> FtsQuery {
    FtsQuery::Text("alpha".into())
}

fn page(conn: &Conn, offset: usize, limit: usize, filter: Option<&dyn Fn(u64) -> bool>) -> Vec<(u64, f64)> {
    let rtxn = conn.read_txn().unwrap();
    conn.fulltext_query_page(&rtxn, &alpha(), offset, limit, filter).unwrap()
}

fn full(conn: &Conn) -> Vec<(u64, f64)> {
    let rtxn = conn.read_txn().unwrap();
    conn.fulltext_query(&rtxn, &alpha(), N as usize).unwrap()
}

fn eids(v: &[(u64, f64)]) -> Vec<u64> {
    v.iter().map(|(e, _)| *e).collect()
}

#[test]
fn pages_concatenate_to_full_result() {
    let conn = corpus();
    let full = full(&conn);
    assert_eq!(full.len(), N as usize);

    let mut joined = page(&conn, 0, 10, None);
    joined.extend(page(&conn, 10, 10, None));
    let third = page(&conn, 20, 10, None);
    assert_eq!(third.len(), 5);
    joined.extend(third);
    assert_eq!(eids(&joined), eids(&full));
    assert_eq!(joined, full);
}

#[test]
fn offset_past_end_and_zero_limit_are_empty() {
    let conn = corpus();
    assert!(page(&conn, 30, 10, None).is_empty());
    assert!(page(&conn, 0, 0, None).is_empty());
}

#[test]
fn filter_keeps_only_matching_entities_before_ranking() {
    let conn = corpus();
    let even = |e: u64| e % 2 == 0;
    let even_fn: &dyn Fn(u64) -> bool = &even;

    // Large limit: every even entity, nothing odd.
    let all_even = page(&conn, 0, 100, Some(even_fn));
    assert_eq!(all_even.len(), (N / 2) as usize);
    assert!(all_even.iter().all(|(e, _)| e % 2 == 0));

    // Small limit: the top 3 even entities, not fewer.
    let top3 = page(&conn, 0, 3, Some(even_fn));
    let expected: Vec<u64> = eids(&full(&conn)).into_iter().filter(|e| e % 2 == 0).take(3).collect();
    assert_eq!(top3.len(), 3);
    assert_eq!(eids(&top3), expected);
}

#[test]
fn tied_scores_return_identical_order_across_calls() {
    let conn = corpus();
    let first = page(&conn, 0, N as usize, None);
    let second = page(&conn, 0, N as usize, None);
    assert_eq!(first, second);

    // Entities 1..=5 tie exactly, so within each run of equal scores the ids must ascend.
    for w in first.windows(2) {
        if w[0].1 == w[1].1 {
            assert!(w[0].0 < w[1].0, "tie not broken by ascending id: {w:?}");
        }
    }
    let score_of_1 = first.iter().find(|(e, _)| *e == 1).unwrap().1;
    let ties: Vec<u64> = first.iter().filter(|(_, s)| *s == score_of_1).map(|(e, _)| *e).collect();
    assert!(ties.len() >= 5, "expected at least 5 tied entities, got {ties:?}");
}
