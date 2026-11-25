# `dtlv-rs` implementation plan

Status as of `1b10d96`. Matrix of what exists versus the Clojure original: `PARITY.md`. This file covers what is
still open, how I would build each item, the order, and defects found on the way.

Goal being tracked: scope 1 (the embedded engine: Datalog, KV, search, vector, idoc, WAL) fully implemented, with
correctness and performance tests. Server, HA, MCP, pod, CLI, and bindings are out of scope.

## 1. Open scope-1 items

### 1.1 Join shapes beyond the hash probe (optimizer)

Today a datom clause in `eval_clauses` is one of: entity bound (one EAV lookup or prefix scan per binding); value
bound and indexed (one AVE prefix per binding); value bound and unindexed (hash probe, built once per clause); or
neither bound (attribute scan, shared once per clause, then a nested loop over bindings x rows).

Open shapes:

- **Shared value, unindexed: done.** The hash probe over the attribute's rows (`probe_key`) already covers this, as
  does the per-clause memo for indexed attributes. Correctness against the nested-loop oracle is now tested in
  `join_shapes_test`: duplicates on both sides, zero and NaN, three clauses, no shared variable, empty sides.
- **Shared entity or value, via a general hash join over the clause's own rows.** Not built. The existing shapes
  cover every datom pattern with a bound entity or bound value, so this is only needed if a case shows up that falls
  through to a nested loop (none found).
- **Indexed lookup per binding: partly done.** Index scans are now memoized per clause by their bound key (entity or
  encoded value), so bindings that share a key share one scan. `indexed_lookup_test` checks correctness and a cost
  bound; the memo's own saving is not isolated by those tests (result building dominates at that size).
  Remaining: the general case in the original description below.
- **Indexed lookup per binding (original description).** Example: `[?a :p/grp ?g] [?b :p/grp ?g]` with `:p/grp` indexed. Each binding does its
  own AVE prefix scan, so n bindings cost n index scans, with repeated `?g` values scanned repeatedly. Plan: group
  bindings by their bound key, run one AVE scan per distinct key, and fan the results back out. Bounded by the number
  of distinct keys, not bindings.
- **Entity-bound lookups repeated across bindings.** Same idea for `(Some(eid), None)`: group by entity id.

Tests to write first:
- Correctness against a nested-loop oracle (same question with the join variable moved into a filter) on: duplicate
  keys on both sides, empty sides, no shared variable (cross product), zero and NaN keys, and joins through three
  clauses.
- Cost guards: 20k x 20k joins on an indexed and on an unindexed attribute, with the probe count asserted (not only
  the time): an indexed join with `d` distinct keys should perform `d` index scans, not `n`.

Done when: all shapes above pass, the existing planner, magic-set, and join tests still pass, and `explain` reports
the access path actually used.

### 1.2 Magic sets on prepared queries

**Done (single-env inputs).** `execute_inputs` substitutes a single input env's values into top-level rule calls,
then runs `demand_rules`. Collection/relation inputs (several envs) still evaluate in full. The per-adornment
rewrite cache is not built: rewriting is cheap next to evaluation (2.7 ms prepared vs 6.7 s full closure, debug).
Tests: `prepared_magic_test`. Original plan kept below for reference.

`PreparedQuery` parses the template once. Its inputs are variables at prepare time, so `demand_rules` sees no
constants and never rewrites them. Two parts:

- Make the rewrite take the set of variables bound by the inputs as well as constants. The adornment then marks
  input-bound positions `b`, and the seed rule takes the input values as its parameters instead of literals.
- Cache rewritten rule sets per adornment (the constant or input-bound pattern), so repeated executions with
  different input values reuse the rewritten program. Only the seed changes per execution.

Tests to write first: a prepared `(reach ?src ?b)` executed with several sources returns the same rows as
`q_str_inputs`; the cache is hit on repeated executions (count rewrites); and a demand-driven execution is much
cheaper than the full closure, as the non-prepared guard already checks.

Known limits to keep: rules with `or`, `not`, or nested queries still fall back to full evaluation.

### 1.3 Smaller open items (in `PARITY.md`)

- Vector index: persisted graph done (saved automatically on the first vector write after a rebuild, or with
  `Conn::save_vector_graph`; ~160 KB per 1k dim-8 vectors). `vec-neighbors` query function done
  (`[(vec-neighbors ?q k) [[?e ?dist]]]`; function clauses now accept `[?x ...]` and `[[?a ?b]]` bindings).
  Open: quantization.
- Idoc: array values inside patterns done (containment semantics).
- Transactions: `with` simulation. `:db/ensure` is done (`TxItem::Ensure`, `register_ensure_fn`; runs at end of tx
  on uncommitted state; not logged, so replay never needs the predicate; no EDN tx parser exists to wire it into).
- Schema: changing cardinality or uniqueness in place is done (`update_schema`, validated against existing data;
  tests `schema_change_test`). Attribute specs are not persisted, so callers must pass the new spec to `open`.
- Durability: automatic checkpoints by log size done (`set_wal_checkpoint_bytes`). Open: segmented txlog, env
  flags for no-sync store writes, group commit.
- Search: custom analyzers done (`set_fulltext_analyzer`, not persisted: re-register after open). Paging and entity
  filters are done (`fulltext_query_page`; filter runs before ranking;
  ties by entity id). Prefix and fuzzy terms are done (`[:prefix "s"]`,
  `[:fuzzy "s" d]`, d <= 2; matched against stemmed index terms; fuzzy scans the term dictionary).
- Transactions: `with` needs reads through an uncommitted write txn, which no backend's `ReadTxn` supports; needs
  a design (overlay or per-backend read-through) before it can be specced.

These are independent of 1.1 and 1.2 and can be taken in any order after them.

## 2. Defects found, not yet fixed

- **Float equality in attribute-value indexes: fixed.** AVE keys and idoc path keys now encode zero as `+0.0`
  (`encode_value_indexed`), and the aggregate dedupe key does too. Tests: `float_index_test`.
- **Float equality in EAV point lookups: decided, query side fixed.** Datoms keep the sign of zero as written
  (no key-format change, so existing databases stay valid). Point lookups (`eav_has`) now check both signs of zero,
  so a `0.0` lookup finds an entity that holds `-0.0`. NaN equals nothing in EAV point lookups, AVE scans, and the
  hash probe. Before this, the indexed AVE scan matched NaN to NaN while the hash probe did not, so the same join
  gave different answers by index. Tests: `join_shapes_test`. Writes now store zero as `+0.0`; a legacy `-0.0` is
  replaced on assert and found by retract, so no migration is needed (`zero_write_test`).
- **Live WAL abort path: tested.** A test-only switch makes the next store commit fail after its log record is
  appended. The test checks the failed transaction is invisible, the log holds an abort marker, the connection keeps
  working, and the aborted record is not replayed after restart (`wal_abort_tests` in `lib.rs`).
- **Vector graph after restart** is rebuilt from scratch on the first large search, which is O(n log n) and was about
  49 s debug for 20k vectors. Persisting the graph (see 1.3) removes that cost.

## 3. Order of work

1. EAV zero question (section 2): decided and fixed on the query side. Write-side canonicalization is still open.
2. Join shapes (1.1): oracle tests and the NaN/zero fixes are in. Remaining: the probe-count cost guard
   (asserting `d` index scans for `d` distinct keys needs a counter; not added).
3. Magic sets on prepared queries (1.2).
5. Smaller items in 1.3, by value to users.

Each step is a separate branch, a test-first change, a workspace run, and a commit to `master`, per the standing
instruction for this repo.

## 4. Known limits (accepted, documented)

- Magic-set rewriting declines bodies with `or`, `not`, or nested queries; they evaluate in full.
- Datalevin's Clojure nested queries are not all supported (no UDF `query-apply`, no derived-relation `:having`).
- LMDB drop empties a DBI but cannot remove its name (the pinned `heed` cannot delete named databases). Copy is not
  supported on redb or the in-memory store.
- Idoc async indexing is explicit: `process_idoc_jobs` must be called; there is no worker thread.
- Markdown input reads plain text: inline formatting is not interpreted and setext headers are not recognized.
- Debug-build timings in the guard tests are for regression detection, not benchmarks.
