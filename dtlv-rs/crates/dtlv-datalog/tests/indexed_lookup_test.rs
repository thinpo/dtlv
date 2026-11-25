//! Indexed lookups per binding: bindings that share a bound key share one index scan per clause.

use std::collections::{BTreeSet, HashMap};

use dtlv_codec::Value;
use dtlv_datalog::{AttrSpec, Conn, QueryResult, TxItem};

#[test]
fn bindings_sharing_an_indexed_key_join_correctly() {
    // 2,000 outer entities in 20 groups (attribute not indexed, scanned once). 2,000 inner entities with an
    // indexed group attribute, 100 per group. Each outer binding probes its group: 2,000 x 100 = 200,000 pairs.
    let conn = Conn::open_mem(vec![AttrSpec::new(":o/g"), AttrSpec::new(":i/g").indexed()]).unwrap();
    let mut items = Vec::new();
    for n in 0..2_000i64 {
        items.push(TxItem::Assert(None, vec![(":o/g".into(), Value::I64(n % 20))]));
        items.push(TxItem::Assert(None, vec![(":i/g".into(), Value::I64(n % 20))]));
    }
    let eids = conn.transact(&items).unwrap();
    let mut outer_group = HashMap::new();
    let mut inner_group = HashMap::new();
    for n in 0..2_000usize {
        let g = (n % 20) as i64;
        outer_group.insert(eids[2 * n], g);
        inner_group.insert(eids[2 * n + 1], g);
    }

    let rtxn = conn.read_txn().unwrap();
    let r = conn
        .q_str(&rtxn, "[:find ?o ?i :where [?o :o/g ?g] [?i :i/g ?g]]", &[])
        .unwrap();
    let QueryResult::Rel(rows) = r else { panic!("expected Rel, got {r:?}") };
    assert_eq!(rows.len(), 200_000);
    let mut outer_counts: HashMap<u64, usize> = HashMap::new();
    for row in &rows {
        let o = row[0].as_ref().unwrap();
        let i = row[1].as_ref().unwrap();
        assert_eq!(outer_group[&o], inner_group[&i], "mismatched group join for ({o}, {i})");
        *outer_counts.entry(o).or_default() += 1;
    }
    assert_eq!(outer_counts.len(), 2_000);
    assert!(outer_counts.values().all(|&c| c == 100));
}

#[test]
fn entity_bound_lookups_memoize_per_entity() {
    // Many bindings across multiple distinct entities: each entity maps strictly to its own values.
    let conn = Conn::open_mem(vec![AttrSpec::new(":e/v").many(), AttrSpec::new(":o/e")]).unwrap();
    let e1 = conn
        .transact(&[TxItem::Assert(None, vec![(":e/v".into(), Value::I64(1)), (":e/v".into(), Value::I64(2))])])
        .unwrap()[0];
    let e2 = conn
        .transact(&[TxItem::Assert(None, vec![(":e/v".into(), Value::I64(3)), (":e/v".into(), Value::I64(4))])])
        .unwrap()[0];
    let e_empty = conn.transact(&[TxItem::Assert(None, vec![])]).unwrap()[0];

    let mut items = Vec::new();
    for _ in 0..250 {
        items.push(TxItem::Assert(None, vec![(":o/e".into(), Value::Ref(e1))]));
        items.push(TxItem::Assert(None, vec![(":o/e".into(), Value::Ref(e2))]));
    }
    for _ in 0..10 {
        items.push(TxItem::Assert(None, vec![(":o/e".into(), Value::Ref(e_empty))]));
    }
    let outer_eids = conn.transact(&items).unwrap();

    let mut expected: BTreeSet<(u64, i64)> = BTreeSet::new();
    for i in 0..250 {
        let o1 = outer_eids[2 * i];
        let o2 = outer_eids[2 * i + 1];
        expected.insert((o1, 1));
        expected.insert((o1, 2));
        expected.insert((o2, 3));
        expected.insert((o2, 4));
    }

    let rtxn = conn.read_txn().unwrap();
    let r = conn.q_str(&rtxn, "[:find ?o ?v :where [?o :o/e ?x] [?x :e/v ?v]]", &[]).unwrap();
    let QueryResult::Rel(rows) = r else { panic!("expected Rel, got {r:?}") };
    let actual: BTreeSet<(u64, i64)> = rows
        .into_iter()
        .map(|row| (row[0].as_ref().unwrap(), row[1].as_i64().unwrap()))
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn indexed_memoization_maps_each_bound_value_to_its_exact_entities() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":o/g"), AttrSpec::new(":i/g").indexed()]).unwrap();
    let o1 = conn.transact(&[TxItem::Assert(None, vec![(":o/g".into(), Value::I64(1))])]).unwrap()[0];
    let o2 = conn.transact(&[TxItem::Assert(None, vec![(":o/g".into(), Value::I64(2))])]).unwrap()[0];
    let o3 = conn.transact(&[TxItem::Assert(None, vec![(":o/g".into(), Value::I64(1))])]).unwrap()[0];

    let i1 = conn.transact(&[TxItem::Assert(None, vec![(":i/g".into(), Value::I64(1))])]).unwrap()[0];
    let i2 = conn.transact(&[TxItem::Assert(None, vec![(":i/g".into(), Value::I64(2))])]).unwrap()[0];
    let _i3 = conn.transact(&[TxItem::Assert(None, vec![(":i/g".into(), Value::I64(3))])]).unwrap()[0];

    let rtxn = conn.read_txn().unwrap();
    let r = conn.q_str(&rtxn, "[:find ?o ?i :where [?o :o/g ?g] [?i :i/g ?g]]", &[]).unwrap();
    let QueryResult::Rel(rows) = r else { panic!("expected Rel, got {r:?}") };
    let actual: BTreeSet<(u64, u64)> = rows
        .into_iter()
        .map(|row| (row[0].as_ref().unwrap(), row[1].as_ref().unwrap()))
        .collect();
    let expected: BTreeSet<(u64, u64)> = [(o1, i1), (o3, i1), (o2, i2)].into_iter().collect();
    assert_eq!(actual, expected);
}

#[test]
fn indexed_memoization_treats_equal_differently_encoded_zeros_identically() {
    let conn = Conn::open_mem(vec![
        AttrSpec::new(":o/z"),
        AttrSpec::new(":i/z").indexed(),
        AttrSpec::new(":o/tz"),
        AttrSpec::new(":i/tz").indexed(),
    ])
    .unwrap();
    let o_neg = conn.transact(&[TxItem::Assert(None, vec![(":o/z".into(), Value::F64(-0.0))])]).unwrap()[0];
    let o_pos = conn.transact(&[TxItem::Assert(None, vec![(":o/z".into(), Value::F64(0.0))])]).unwrap()[0];
    let o_one = conn.transact(&[TxItem::Assert(None, vec![(":o/z".into(), Value::F64(1.0))])]).unwrap()[0];
    // NaN must never match another NaN across indexed join.
    conn.transact(&[TxItem::Assert(None, vec![(":o/z".into(), Value::F64(f64::NAN))])]).unwrap();

    let i_pos = conn.transact(&[TxItem::Assert(None, vec![(":i/z".into(), Value::F64(0.0))])]).unwrap()[0];
    let i_one = conn.transact(&[TxItem::Assert(None, vec![(":i/z".into(), Value::F64(1.0))])]).unwrap()[0];
    conn.transact(&[TxItem::Assert(None, vec![(":i/z".into(), Value::F64(f64::NAN))])]).unwrap();

    let rtxn = conn.read_txn().unwrap();
    let r = conn.q_str(&rtxn, "[:find ?o ?i :where [?o :o/z ?z] [?i :i/z ?z]]", &[]).unwrap();
    let QueryResult::Rel(rows) = r else { panic!("expected Rel, got {r:?}") };
    let actual: BTreeSet<(u64, u64)> = rows
        .into_iter()
        .map(|row| (row[0].as_ref().unwrap(), row[1].as_ref().unwrap()))
        .collect();
    let expected: BTreeSet<(u64, u64)> = [(o_neg, i_pos), (o_pos, i_pos), (o_one, i_one)].into_iter().collect();
    assert_eq!(actual, expected);

    // Tuple values with differently encoded zeros (-0.0 vs 0.0) share the same memoized AVE scan too.
    drop(rtxn);
    let t_neg = Value::Tuple(vec![Value::F64(-0.0), Value::I64(7)]);
    let t_pos = Value::Tuple(vec![Value::F64(0.0), Value::I64(7)]);
    let ot_neg = conn.transact(&[TxItem::Assert(None, vec![(":o/tz".into(), t_neg)])]).unwrap()[0];
    let ot_pos = conn.transact(&[TxItem::Assert(None, vec![(":o/tz".into(), t_pos.clone())])]).unwrap()[0];
    let it_pos = conn.transact(&[TxItem::Assert(None, vec![(":i/tz".into(), t_pos)])]).unwrap()[0];

    let rtxn = conn.read_txn().unwrap();
    let tr = conn.q_str(&rtxn, "[:find ?o ?i :where [?o :o/tz ?t] [?i :i/tz ?t]]", &[]).unwrap();
    let QueryResult::Rel(trows) = tr else { panic!("expected Rel, got {tr:?}") };
    let t_actual: BTreeSet<(u64, u64)> = trows
        .into_iter()
        .map(|row| (row[0].as_ref().unwrap(), row[1].as_ref().unwrap()))
        .collect();
    let t_expected: BTreeSet<(u64, u64)> = [(ot_neg, it_pos), (ot_pos, it_pos)].into_iter().collect();
    assert_eq!(t_actual, t_expected);
}
