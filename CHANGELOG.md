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
- Test suites: `upstream_parity_test`, `refdata_gdoc_cases`, `sqlite_persistence_test`, and inline unit tests (18 tests passing).
- Documentation: `NEON.md`, `DECISIONS.md`, `SPEC.md`, `dtlv-rs/docs/DATALOG_IMPLEMENTATION.md`.

### Changed
- Standardized canonical test command on `clojure -M:test ...` (see `SPEC.md`).
- `cmp_vars` now matches `cmp_vals` ordering coverage (mixed I64/Instant, Keyword, Ref).
- Benchmark timing sort is NaN-safe.
- Memory optimizations:
  - Replaced full-DB clone in `MemWriteTxn` with an $O(\text{delta})$ overlay map.
  - Added thread-local scratch buffers in `encode_av_prefix` and `lookup_entity_by_av` to eliminate hot-path `Vec` allocations.
  - Skipped `HashSet` / `format!` deduplication allocations in `finalize_query_result` when `envs_slice.len() <= 1`.
  - Bounded `VectorIndex::search_vec` memory to $O(k)$ using a max-`BinaryHeap`.

### Fixed
- Potential panic in benchmark sort when timing samples contain NaN.

### Notes
- Integration tests require host socket/LMDB permissions (XTDB v2 pgwire socket, Datalevin LMDB).
- `.cpcache/` and temporary database directories are intentionally not committed.