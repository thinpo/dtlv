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

- **Neither side bound, shared variables with the bindings.** Example: `[?a :l/x ?v] [?b :r/x ?v]` where `?v` is bound
  only by the first clause and the second clause's value is free. Plan: a general hash join. Build a table over the
  clause's rows keyed by the values of the variables the clause shares with the bindings (for a datom: `e`, `v`, or
  both), then probe it once per binding. Equality uses the canonical key from `probe_key` (zero and NaN as already
  handled). Shapes with no shared variable stay a cross product, which is the correct semantics.
- **Indexed lookup per binding.** Example: `[?a :p/grp ?g] [?b :p/grp ?g]` with `:p/grp` indexed. Each binding does its
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

- Vector index: persisted graph (today the graph is rebuilt from the log or vectors after restart), quantization,
  and `vec-neighbors` as a query function.
- Idoc: array values inside patterns (today patterns may not contain arrays).
- Transactions: `with` simulation and `:db/ensure`.
- Schema: changing cardinality or uniqueness of an existing attribute in place.
- Durability: segmented txlog, automatic checkpoints and retention, env flags for no-sync writes, group commit.
- Search: fuzzy and prefix terms, custom analyzers, doc filters and paging.

These are independent of 1.1 and 1.2 and can be taken in any order after them.

## 2. Defects found, not yet fixed

- **Float equality in attribute-value indexes.** `probe_key` fixed the hash probe, but the AVE index still keys floats
  by encoded bytes. A lookup for `0.0` misses a stored `-0.0` (and the reverse) through `lookup_entity_by_av` and
  `index_range`, while scans treat them as equal. Fix: canonicalize the same way in `encode_av_prefix` callers, or
  canonicalize when writing AVE keys. Needs a migration note, since AVE keys change for affected values.
- **Live WAL abort path is untested.** Replay of aborted records is tested; the path where a store commit fails after
  the append is not, because a failing commit cannot be forced on demand. Plan: inject a commit failure in a test
  build (a feature flag or a wrapper around the store).
- **Vector graph after restart** is rebuilt from scratch on the first large search, which is O(n log n) and was about
  49 s debug for 20k vectors. Persisting the graph (see 1.3) removes that cost.

## 3. Order of work

1. Fix the AVE float keys (section 2). Small and it is a correctness bug.
2. Join shapes (1.1), tests first. This is the largest remaining optimizer item.
3. Magic sets on prepared queries (1.2).
4. The live WAL abort test (section 2).
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
