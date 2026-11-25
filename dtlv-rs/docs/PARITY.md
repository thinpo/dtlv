# `dtlv-rs` parity with Datalevin (embedded engine scope)

Scope: the embedded engine (Datalog, KV, search, vector, idoc, WAL). Server, HA, MCP, pod, CLI, JVM/Python/JS bindings, and LLM/embedding providers are out of scope for this crate.

Source of truth: `datalevin/src` (Clojure) and `datalevin/src/rust` (codecs).

## Status

| Area | Item | Status |
|---|---|---|
| Transactions | assert, retract, retractEntity, cas, tempids, lookup refs, component cascade | done |
| Transactions | unique identity/value, cardinality | done |
| Transactions | tuple attributes (`tuple_of`, maintained at commit), `RetractAttribute`, named transaction functions (`TxItem::Call`, `register_tx_fn`) | done |
| Transactions | `with` simulation, `:db/ensure`, descriptor UDFs | todo |
| Schema | `update_schema`: add, delete, rename, index/fulltext upgrade with backfill | done |
| Schema | in-place change of cardinality/unique, tuple attrs added after open | todo |
| Query | `:find` forms, aggregates, `:order-by`, `:limit`, `:offset`, `:keys`/`:strs`/`:syms`, pull in find | done |
| Query | predicates `< <= > >= = != like not-like` | done |
| Query | built-ins `get-else get-some missing? ground tuple untuple in not-in inc dec + - * / quot rem mod` | done |
| Query | `:in` scalar, collection, tuple, relation bindings (`QueryInput`, `q_str_inputs`, `execute_inputs`) | done |
| Query | `or-join` / `not-join` join-variable scoping | done |
| Query | `or`, `not`, recursive rules, pull, `fulltext` clause | done (rules are naive fixpoint) |
| Query | cost-based optimizer, `explain`, `analyze`, magic-set rules | todo |
| Query | nested queries, UDFs, `get-some-else`, `like` escape option | todo |
| Entity API | `rseek_datoms`, `search_datoms`, `fulltext_datoms`, `cardinality`, `max_eid` | done |
| Entity API | lazy `entity` / `touch` (pull covers reads) | todo |
| Listeners | `listen`/`unlisten` with a `TxReport` of added and retracted datoms, delivered after commit | done |
| Listeners | `listen-db!` (per-database listeners) | todo |
| Bulk load | `load_datoms`: one transaction, all-or-nothing | done |
| Bulk load | streaming / batched fill, `init-db` schema bootstrap | todo |
| KV | get/put/delete/range/prefix/iter on LMDB, SQLite, redb, memory | done |
| KV | list (dupsort) DBIs (`ListDb`), rank, by-rank, range count, first-n, clear | done |
| KV | drop/copy/stat/list-dbis, sample, buffers; list ops not yet exposed via `Conn` | todo |
| Durability | WAL, txlog, snapshots, GC, watermarks, durability profiles, tx-group, async tx | todo |
| Search | BM25 with Datalevin's default analyzer (stopwords on, positions kept across gaps) | done |
| Search | English Snowball (Porter2) stemming, `FtsConfig` (fixed before indexing) | done |
| Search | boolean `and` / `or` / `not` and positional `phrase` queries (`Query`, `Conn::fulltext_query`) | done |
| Search | fuzzy and prefix terms, custom analyzers, EDN query syntax in Datalog `fulltext` clause, doc-filter, paging | todo |
| Vector | exact top-k over 3 metrics | done |
| Vector | ANN index (usearch HNSW), quantization, checkpoints, `vec-neighbors` in query | todo |
| Idoc | document values, `idoc-match`, `idoc-get`, patch, path index | todo (only the `Doc` tag exists) |
| Codecs | nippy, bits, zstd dictionary compression, CSV, Hu-Tucker keys, roaring | todo (in `datalevin/src/rust`) |

## Behavioral notes

- Scalar `:in` inputs are substituted at parse time. Other shapes are seeded into the evaluation environment.
- Integer `/` returns `I64` only when exact, otherwise `F64`. Integer overflow and division by zero are `DatalogError::Function` errors.
- `or-join` branches keep only outer and join variables. `not-join` clauses see only join variables from the outer scope.
- Explicit entity ids advance the fresh-id counter, so later allocations cannot collide with them.
- `get-else` and `missing?` on an unknown attribute return an error, rather than treating the attribute as absent.
