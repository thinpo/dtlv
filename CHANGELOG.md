# Changelog

All notable changes to this repository are documented here.

## [Unreleased]

### Added
- Pure-Rust Datalevin engine (`dtlv-rs`) workspace with crates:
  - `dtlv-codec`: value codec, sortable encodings, EAV/AVE/VAE key helpers, escaped bytes.
  - `dtlv-kv`: multi-backend KV store supporting LMDB, SQLite (WAL, `WITHOUT ROWID` tables), `redb` (pure Rust), and in-memory `BTreeMap` (`Mem`) with transactions and range scans.
  - `dtlv-search`: BM25 full-text index and vector index.
  - `dtlv-datalog`: Datalog query engine, transactions, pull, rules, aggregates, prepared queries, `open_sqlite`/`open_redb`/`open_mem`, and `save_to_sqlite`/`load_from_sqlite` snapshot round-trip.
  - `dtlv-bench`: 4-backend reference-data benchmark harness (`LMDB`, `SQLite`, `redb`, `Mem`).
- Test suites: `upstream_parity_test`, `refdata_gdoc_cases`, `sqlite_persistence_test`, `review_regressions_test` (dedup, retraction, scan binding, sum, sqlite save), `mem_store_test` (Mem read/write concurrency), and inline unit tests.
- Documentation: `NEON.md`, `DECISIONS.md`, `SPEC.md`, `dtlv-rs/docs/DATALOG_IMPLEMENTATION.md`.

### Changed
- Standardized canonical test command on `clojure -M:test ...` (see `SPEC.md`).
- `cmp_vars` now matches `cmp_vals` ordering coverage (mixed I64/Instant, Keyword, Ref).
- Benchmark timing sort is NaN-safe.
- `TxItem::RetractEntity` now also removes incoming references (eav, ave and vae indexes) from other entities, which previously dangled. Callers that relied on those dangling refs must re-assert them after retracting the target.
- Query deduplication streams over results and stops once `offset + limit` distinct rows are collected, so limited queries no longer format every match.
- Entity terms in value position are treated as refs.
- In-memory store: writers are serialized by a dedicated lock; readers no longer take the data lock for the duration of a write transaction.
- Memory optimizations:
  - Replaced full-DB clone in `MemWriteTxn` with an $O(\text{delta})$ overlay map.
  - Added thread-local scratch buffers in `encode_av_prefix` and `lookup_entity_by_av` to eliminate hot-path `Vec` allocations.
  - Bounded `VectorIndex::search_vec` memory to $O(k)$ using a max-`BinaryHeap`.

### Fixed
- Potential panic in benchmark sort when timing samples contain NaN.
- Datalog query evaluation checks bound-variable consistency in scans, so a variable shared across pattern positions (e.g. `[?x :attr ?x]`) unifies instead of being overwritten.
- Deduplication happens after `:order-by` and before offset/limit application, so `:limit`/`:offset` no longer return duplicate rows, including for ordered queries. Pull results are deduplicated per entity, so `(pull ?e ...)` without `:order-by` returns every match.
- Entity retraction propagates index errors instead of ignoring them.
- `Sum` aggregation preserves `i64` precision; overflow returns `DatalogError::SumOverflow` instead of wrapping.
- `save_to_sqlite` writes to a temp file and atomically replaces the destination, removing stale `-wal`/`-shm` sidecars. A failed save keeps the previous snapshot intact and removes its temp files; the old main file is no longer deleted before the rename.

### Notes
- Integration tests require host socket/LMDB permissions (XTDB v2 pgwire socket, Datalevin LMDB).
- `.cpcache/` and temporary database directories are intentionally not committed.