use dtlv_datalog::{idoc_value_markdown, normalize_header, pattern_predicates, AttrSpec, Conn, DatalogError, TxItem};
use serde_json::json;

fn parsed(md: &str) -> serde_json::Value {
    match idoc_value_markdown(md).unwrap() {
        dtlv_codec::Value::Doc(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn documented_example_nests_headers_and_stores_text_under_the_leaf() {
    let md = "# User Profile\n## Getting Started!\nName: Alice\nAge: 30";
    assert_eq!(parsed(md), json!({"user-profile": {"getting-started": "Name: Alice\nAge: 30"}}));
}

#[test]
fn returning_to_a_higher_level_starts_a_sibling_section() {
    let md = "# One\n## A\nx\n# Two\n## B\ny";
    assert_eq!(parsed(md), json!({"one": {"a": "x"}, "two": {"b": "y"}}));
}

#[test]
fn headers_normalize_like_datalevin() {
    assert_eq!(normalize_header("1. Getting Started!").unwrap(), "getting-started");
    assert_eq!(normalize_header("  Hello,   World  ").unwrap(), "hello-world");
    assert_eq!(normalize_header("Déjà vu").unwrap(), "déjà-vu");
    assert!(normalize_header("!!!").is_err());
}

#[test]
fn blank_sections_store_nothing() {
    assert_eq!(parsed("# Empty\n\n# Filled\nthing"), json!({"filled": "thing"}));
}

#[test]
fn content_before_the_first_header_is_rejected() {
    assert!(matches!(idoc_value_markdown("stray text\n# A\nx"), Err(DatalogError::Type(_))));
}

#[test]
fn a_section_with_text_and_subheaders_is_rejected() {
    assert!(matches!(idoc_value_markdown("# A\ntext\n## B\nx"), Err(DatalogError::Type(_))));
}

#[test]
fn a_repeated_key_under_one_parent_is_rejected() {
    assert!(matches!(idoc_value_markdown("# A\n## Dup\nx\n## dup\ny"), Err(DatalogError::Type(_))));
}

#[test]
fn markdown_documents_index_and_query_like_other_idoc_values() {
    let conn = Conn::open_mem(vec![AttrSpec::new(":doc/md").idoc()]).unwrap();
    let doc = idoc_value_markdown("# User Profile\n## Getting Started!\nName: Alice").unwrap();
    conn.transact(&[TxItem::Assert(None, vec![(":doc/md".into(), doc)])]).unwrap();
    let rtxn = conn.read_txn().unwrap();
    let preds = pattern_predicates(&json!({"user-profile": {"getting-started": "Name: Alice"}})).unwrap();
    let hits = conn.idoc_match(&rtxn, Some(":doc/md"), &preds).unwrap();
    assert_eq!(hits.len(), 1);
}
