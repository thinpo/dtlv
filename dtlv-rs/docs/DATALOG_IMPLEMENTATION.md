# `dtlv-rs` Datalog Implementation Guide

This document describes the architecture, storage layouts, transaction semantics, query parser, and execution engine of `dtlv-rs` ([dtlv-rs/Cargo.toml](../Cargo.toml)).

---

## 1. Crate Architecture

`dtlv-rs` is structured as a layered Cargo workspace:

1. **`dtlv-codec`** ([crates/dtlv-codec/src/lib.rs](../crates/dtlv-codec/src/lib.rs)): Order-preserving binary serialization for `Value` and composite `EAV`/`AVE`/`VAE` index keys.
2. **`dtlv-kv`** ([crates/dtlv-kv/src/lib.rs](../crates/dtlv-kv/src/lib.rs)): Zero-copy LMDB wrapper built on `heed` (`RoTxn` / `RwTxn`, named DBIs, range/prefix scans).
3. **`dtlv-search`** ([crates/dtlv-search/src/lib.rs](../crates/dtlv-search/src/lib.rs)): LMDB-backed BM25 `FullTextIndex` and `VectorIndex` (`Cosine`, `Euclidean`, `DotProduct`).
4. **`dtlv-datalog`** ([crates/dtlv-datalog/src/lib.rs](../crates/dtlv-datalog/src/lib.rs), [parser.rs](../crates/dtlv-datalog/src/parser.rs)): Schema catalog, ACID transaction processor, `pull` engine, EDN/Datalog parser, rule evaluator, and prepared query/pull handles.
5. **`dtlv-bench`** ([crates/dtlv-bench/src/main.rs](../crates/dtlv-bench/src/main.rs)): Trading Reference Data benchmark harness.

---

## 2. Binary Encoding & LMDB Index Layout

### 2.1 `Value` Type Tags (`dtlv-codec`)
Every value is prefixed with a 1-byte type tag so heterogeneous values are self-describing and type-grouped in B+tree order:

| Tag | Variant | Encoding |
| --- | --- | --- |
| `0x01` | `Bool(bool)` | `0` or `1` |
| `0x02` | `I64(i64)` | Sign-bit flipped big-endian (`(v as u64) ^ (1 << 63)`) |
| `0x03` | `F64(f64)` | IEEE-754 sortable bits (negative: `!bits`; non-negative: `bits ^ (1 << 63)`) |
| `0x04` | `Instant(i64)` | Epoch milliseconds, sign-bit flipped big-endian |
| `0x05` | `Ref(u64)` | 8-byte big-endian `EntityId` |
| `0x06` | `Keyword(String)` | UTF-8 bytes |
| `0x07` | `Str(String)` | UTF-8 bytes |
| `0x08` | `Bytes(Vec<u8>)` | Raw bytes |
| `0x09` | `Uuid([u8; 16])` | 16 raw bytes |
| `0x0a` | `Tuple(Vec<Value>)` | Concatenation of byte-stuffed encoded elements |
| `0x0b` | `Doc(String)` | UTF-8 JSON/EDN document (`:db.type/idoc`) |

### 2.2 Escaped Bytes & Index Keys
Variable-length values in composite keys use `encode_escaped_bytes` (`0x00` → `0x00 0x01`, terminated by `0x00 0x00`), which preserves lexicographical sort order while preventing prefix collisions (e.g., `"AA"` vs `"AAPL"`).

Each `Conn` opens the following LMDB DBIs (512 MiB default map size, 16 max DBIs):

- **`eav`**: `[eid: 8B BE][aid: 4B BE][encoded_value...]` → `[]`
  Supports $O(\log N)$ entity attribute reads (`get_attr`, `get_attr_many`) and entity-wide prefix scans (`pull_pattern`, `RetractEntity`).
- **`ave`**: `[aid: 4B BE][escaped(encoded_value)][eid: 8B BE]` → `[]`
  Populated when `AttrSpec::index` is `true` (including `:db.unique/identity` and `:db.unique/value`). Supports $O(\log N)$ point lookups (`lookup_entity_by_av` / `entid`), `count_datoms`, `seek_datoms`, and bounded `index_range`.
- **`vae`**: `[target_eid: 8B BE][aid: 4B BE][src_eid: 8B BE]` → `[]`
  Populated for `Value::Ref` values to support $O(\log N)$ reverse pull (`:_attr`).
- **`meta`**: Stores `__next_eid` (`u64`), `__next_aid` (`u32`), and `attr:<name>` → `aid` mappings for schema persistence across restarts.
- **`fts_postings`**, **`fts_doc_lens`**, **`fts_doc_terms`**, **`fts_meta`**: BM25 inverted index (`k1 = 1.2`, `b = 0.75`).
- **`vec_idx`**: LMDB-persisted entity embeddings (`eid: 8B BE` → `Vec<f32>` little-endian).

---

## 3. Schema & Transaction Processing (`Conn::transact`)

### 3.1 Schema (`AttrSpec`)
Attributes are registered on `Conn::open(path, schema)` and persisted in `meta`:
- `Cardinality::One` (default) or `Cardinality::Many`
- `Unique::Identity` (upserts existing entity) or `Unique::Value` (returns `DatalogError::UniqueViolation` on conflict)
- `index: bool`, `fulltext: bool`, `is_component: bool`

### 3.2 Atomic `RwTxn` Operations (`TxItem`)
All items in `Conn::transact(&[TxItem])` execute inside a single LMDB `RwTxn`:
- **`Assert(Option<EntityId>, Vec<(String, Value)>)`**: Resolves entity by explicit ID, `:db.unique/identity` lookup in `ave`, or allocates `__next_eid`.
- **`AssertRef(EntityRef, Vec<(String, TxVal)>)`**: Resolves `TempId(String)` (shared within the transaction batch) and `LookupRef(attr, val)` for both entity IDs and `Ref` target values.
- **`assert_datom`**: Enforces `Unique::Value`, retracts prior values if `Cardinality::One`, updates `eav`/`ave`/`vae`, and updates `FullTextIndex` (`remove_doc` + `index_doc`) when `fulltext` is enabled.
- **`Retract(eid, attr, val)`**: Deletes from `eav`, `ave`, `vae`, and removes `fts` postings if `fulltext`.
- **`RetractEntity(eid)`**: Recursively retracts all datoms of `eid` (`retract_entity_recursive`), cleans up `fts`, and cascades into child entities referenced via `is_component` attributes.
- **`Cas(eid, attr, expected, new_val)`**: Verifies current value in `eav` equals `expected` (else returns `DatalogError::CasFailed` and aborts `RwTxn`) before asserting `new_val`.

---

## 4. EDN / Datalog Parser (`parser.rs`)

`parse_query` / `parse_query_template` and `parse_rules` support both **vector** (`[:find ... :where ...]`) and **map** (`{:find ... :where ... :order-by ... :limit ... :offset ... :keys ...}`) EDN syntax:
- **Literals**: Strings, keywords, integers, floats, booleans, and `#inst "YYYY-MM-DDTHH:MM:SSZ"` (converted to epoch ms via `parse_iso8601_utc_ms`).
- **`:find` forms**:
  - Relation (`?a ?b`), Scalar (`?a .`), Collection (`[?a ...]`), Tuple (`[?a ?b]`)
  - Aggregates: `(count ?v)`, `(sum ?v)`, `(min ?v)`, `(max ?v)`, `(avg ?v)`
  - Inline pull: `[(pull ?e [:attr1 :attr2]) ...]`
- **`:in` bindings**: `$` (default DB), `%` (rules), and `?var` inputs.
- **`:where` clauses**:
  - Datoms `[?e :attr ?v]` (with `_` wildcards)
  - Predicates: `<=`, `<`, `>=`, `>`, `=`, `!=`/`not=`, `like`
  - Full-text: `[(fulltext $ ?q) [[?e _ _]]]` or `[(fulltext $ ?q) ?e]`
  - Disjunction/Negation: `or`, `or-join`, `and`, `not`, `not-join`
  - Rule calls: `(rule-name ?arg1 ...)`
- **Modifiers**: `:order-by [[?var :asc|:desc]]` (or column index), `:limit`, `:offset`, and `:keys`/`:strs`/`:syms`.

---

## 5. Query Execution, Rules & Prepared Handles

### 5.1 Recursive Rules (`eval_all_rules`)
Before evaluating `:where` clauses, `eval_all_rules` computes the fixed-point relation for all rules in `rule_map`, iterating until no new deduplicated tuples are produced. This supports arbitrary transitive closure (e.g., corporate parent-subsidiary trees and multi-hop M&A lineage).

### 5.2 Clause Evaluation (`eval_clauses`)
Clauses are evaluated left-to-right over a vector of variable binding maps (`Vec<HashMap<String, Value>>`):
- **`Clause::Datom`**:
  1. Both `e` and `v` bound → $O(\log N)$ point lookup in `eav`.
  2. `e` bound, `v` unbound → prefix scan in `eav` over `(e, a)`.
  3. `e` unbound, `v` bound, `spec.index == true` → prefix scan in `ave` over `(a, v)`.
  4. Fallback → `eav` scan filtered by `aid`.
- **`Clause::Pred`**:
  - `CmpOp::Eq` performs **unification**: if one side is an unbound variable and the other is bound, it binds the variable.
  - `CmpOp::Like` runs UTF-8 char-aware dynamic programming wildcard matching (`%` and `_`).
  - `cmp_vals` / `cmp_vars` compare `I64`/`Instant` (including cross-type), `F64`, `Str`/`Keyword`, and `Ref`.
- **`Clause::Or` / `Clause::Not`**: `Or` unions branch environments; `Not` filters out environments where the inner sub-clauses match.

### 5.3 Post-Processing & Prepared Execution
`finalize_query_result` applies:
1. Grouped aggregation (`aggregate_envs`) if any `FindElem::Agg` is present.
2. Otherwise: `:order-by` sorting → `:offset` / `:limit` slicing → `(pull ...)` expansion or `:find` projection → `:keys` map wrapping or `Rel`/`Scalar`/`Coll`/`Tuple` shaping.

`Conn::prepare_query` (`PreparedQuery`) and `Conn::prepare_pull` (`PreparedPull`) parse the query/rules and selector ASTs once at startup, eliminating parsing overhead on intraday hot paths.
