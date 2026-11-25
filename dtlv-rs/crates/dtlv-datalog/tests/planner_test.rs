use dtlv_codec::Value;
use dtlv_datalog::{Access, AttrSpec, Conn, QueryResult, TxItem};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

/// 5,000 people named by number; three of them are "rare" (indexed attribute).
fn conn() -> Conn {
    let conn = Conn::open_mem(vec![AttrSpec::new(":p/name"), AttrSpec::new(":p/kind").indexed()]).unwrap();
    let mut items: Vec<TxItem> = (0..5000)
        .map(|i| TxItem::Assert(None, vec![(":p/name".into(), s(&format!("n{i}")))]))
        .collect();
    for i in 0..3 {
        items.push(TxItem::Assert(None, vec![(":p/name".into(), s(&format!("rare{i}"))), (":p/kind".into(), s("rare"))]));
    }
    conn.transact(&items).unwrap();
    conn
}

const UNSELECTIVE_FIRST: &str = r#"[:find ?n :where [?e :p/name ?n] [?e :p/kind "rare"]]"#;

#[test]
fn the_selective_clause_is_planned_first() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    let plan = c.explain(&rtxn, UNSELECTIVE_FIRST, &[]).unwrap();
    assert_eq!(plan.len(), 2);
    assert_eq!(plan[0].access, Access::AveRange, "the indexed value lookup should run first: {plan:?}");
    assert_eq!(plan[0].estimated_rows, 3);
    assert_eq!(plan[1].access, Access::EavPrefix, "with the entity bound and the name free, the entity prefix is read");
}

#[test]
fn analyze_shows_small_intermediate_results_when_the_plan_is_good() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    let steps = c.analyze(&rtxn, UNSELECTIVE_FIRST, &[]).unwrap();
    // Without reordering, the first clause would bind all 5,003 entities; planned first, it binds 3.
    assert_eq!(steps[0].rows, 3, "{steps:?}");
    assert_eq!(steps.last().unwrap().rows, 3);
    let mut names: Vec<String> = match c.q_str(&rtxn, UNSELECTIVE_FIRST, &[]).unwrap() {
        QueryResult::Rel(rows) => rows.into_iter().map(|r| r[0].as_str().unwrap().to_owned()).collect(),
        other => panic!("{other:?}"),
    };
    names.sort();
    assert_eq!(names, vec!["rare0", "rare1", "rare2"]);
}

#[test]
fn planning_never_changes_results() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    // The same conjunction written in both orders must give the same answer.
    let a = c.q_str(&rtxn, r#"[:find [?n ...] :where [?e :p/name ?n] [?e :p/kind "rare"]]"#, &[]).unwrap();
    let b = c.q_str(&rtxn, r#"[:find [?n ...] :where [?e :p/kind "rare"] [?e :p/name ?n]]"#, &[]).unwrap();
    let sorted = |r: QueryResult| match r {
        QueryResult::Coll(mut v) => {
            v.sort_by_key(|x| format!("{x:?}"));
            v
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(sorted(a), sorted(b));
}

#[test]
fn constant_entity_patterns_are_planned_before_scans() {
    let c = conn();
    let rtxn = c.read_txn().unwrap();
    let plan = c.explain(&rtxn, r#"[:find ?n :where [?e :p/name ?n] [1 :p/name ?m]]"#, &[]).unwrap();
    assert_eq!(plan[0].access, Access::EavPrefix, "{plan:?}");
    assert_eq!(plan[1].access, Access::EavScan, "{plan:?}");
}
