# Changelog

All notable changes to this repository are documented here.

## [Unreleased]

### Added
- Pure-Rust Datalevin engine (`dtlv-rs`) workspace with crates:
  - `dtlv-codec`: value codec, sortable encodings, EAV/AVE/VAE key helpers, escaped bytes.
  - `dtlv-kv`: LMDB-backed KV store with transactions and range scans.
  - `dtlv-search`: BM25 full-text index and vector index.
  - `dtlv-datalog`: Datalog query engine, transactions, pull, rules, aggregates, prepared queries.
  - `dtlv-bench`: reference-data benchmark harness.
- Test suites: `upstream_parity_test`, `refdata_gdoc_cases`, and inline unit tests.
- Documentation: `NEON.md`, `DECISIONS.md`, `SPEC.md`.

### Changed
- Standardized canonical test command on `clojure -M:test ...` (see `SPEC.md`).
- `cmp_vars` now matches `cmp_vals` ordering coverage (mixed I64/Instant, Keyword, Ref).
- Benchmark timing sort is NaN-safe.

### Fixed
- Potential panic in benchmark sort when timing samples contain NaN.

### Notes
- Integration tests require host socket/LMDB permissions (XTDB v2 pgwire socket, Datalevin LMDB).
- `.cpcache/` and temporary database directories are intentionally not committed.