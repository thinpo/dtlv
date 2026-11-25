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
| Query | built-ins `get-else get-some get-some-else missing? ground tuple untuple in not-in inc dec + - * / quot rem mod` | done (nil is the keyword `:nil`) |
| Query | `:in` scalar, collection, tuple, relation bindings (`QueryInput`, `q_str_inputs`, `execute_inputs`) | done |
| Query | `or-join` / `not-join` join-variable scoping | done |
| Query | `or`, `not`, recursive rules, pull, `fulltext` clause | done (rules are semi-naive fixpoint (transitive closure over a 200-node chain: 4 s debug, was 522 s)) |
| Query | cost-based ordering of datom clauses (greedy, index-aware), `explain` (access path and estimate per clause), `analyze` (rows and time per clause); an unconstrained attribute scan runs once per clause instead of once per binding | done |
| Query | magic-set rewriting for rule calls with constant arguments (adorned rules and magic predicates; positive conjunctive bodies; other rules fall back to full evaluation). Applies to `q_str` and friends, not prepared queries. 300-node chain: 4 ms demand-driven vs 8 s full | done (limits noted) |
| Query | hash probe for datom patterns whose value is bound by an earlier clause but not indexed (index built once per clause). Other join shapes (both sides free, indexed lookups per binding) remain nested-loop | partial |
| Query | `like` / `not-like` with Datalevin's `!` default escape and `{:escape "c"}` option | done |
| Query | nested queries `[(q [:find ...] $ args...) [[?a ?b]]]` (positional binding, correlated per outer binding, constant ones run once; nested queries see the outer query's rules) | done |
| Query | user functions in clauses, registered per connection with `register_query_fn` (resolved at evaluation) | done |
| Entity API | `rseek_datoms`, `search_datoms`, `fulltext_datoms`, `cardinality`, `max_eid` | done |
| Entity API | read-only `entity` views: `get`, `touch`, `entity_by_lookup` (cardinality-many as tuples) | done |
| Entity API | transactable entities: `assoc` and `dissoc` build transaction items | done |
| Listeners | `listen`/`unlisten` with a `TxReport` of added and retracted datoms, delivered after commit | done |
| Listeners | `listen-db!` (per-database listeners) | todo |
| Bulk load | `load_datoms`: one transaction, all-or-nothing | done |
| Bulk load | streaming / batched fill, `init-db` schema bootstrap | todo |
| KV | get/put/delete/range/prefix/iter on LMDB, SQLite, redb, memory | done |
| KV | list (dupsort) DBIs (`ListDb`), rank, by-rank, range count, first-n, clear | done |
| KV | list-dbis (all backends), drop-dbi, copy-to (LMDB and SQLite) | done |
| KV | LMDB drop empties the DBI but cannot remove its name (pinned `heed`); copy on redb and memory unsupported | known limit |
| KV | sample, buffers, stat beyond entry count, list ops exposed through `Conn` | todo |
| Durability | Datalog write-ahead log (`Conn::open_wal`): framed and checksummed, replay of unapplied records, torn-tail recovery, `checkpoint`, strict and relaxed profiles | done |
| Durability | KV-level log (`DurableKv`): batches logged before commit, replay of unapplied batches, torn-tail recovery, checkpoint (log truncation), snapshots. Tested on LMDB and SQLite; redb and memory share the code path but are untested | done |
| Durability | segmented txlog, automatic checkpoints and retention, watermarks, env flags for no-sync writes, tx-group, async tx | todo |
| Durability | a failed store commit after a log append is marked aborted, and replay skips it (replay path tested; the live failure path is not) | done |
| Search | BM25 with Datalevin's default analyzer (stopwords on, positions kept across gaps) | done |
| Search | English Snowball (Porter2) stemming, `FtsConfig` (fixed before indexing) | done |
| Search | boolean `and` / `or` / `not` and positional `phrase` queries (`Query`, `Conn::fulltext_query`) | done |
| Search | `fulltext` clause accepts `:and`/`:or`/`:not`, `{:phrase}`, `{:term}`, and bound string variables | done |
| Search | fuzzy and prefix terms, custom analyzers, doc-filter, paging | todo |
| Vector | exact top-k over 3 metrics (used below 512 vectors) | done |
| Vector | in-memory HNSW for indexes above 512 vectors, rebuilt after writes (version-checked) | done |
| Vector | incremental graph maintenance (log replay, tombstones, rebuild fallback) | done |
| Vector | persisted graph, quantization, checkpoints, `vec-neighbors` in query | todo |
| Idoc | JSON documents, path index (concrete, collapsed, wildcard forms), `idoc_match` (conjunctive), `idoc_get` | done (JSON, sync indexing) |
| Idoc | `idoc-match` clause in Datalog queries (`[(idoc-match $ :attr {...}) [[?e ?a ?v]]]`, literal or bound pattern values) | done |
| Idoc | `:db.fn/patchIdoc` (set, unset, conj, merge, assoc, dissoc, inc, dec) on cardinality-one attributes, atomic with the transaction and logged to the WAL | done |
| Idoc | `:db.fn/patchIdoc` on cardinality-many attributes (the old document identifies the target) | done |
| Idoc | EDN input (`idoc_value_edn`; keyword values, lists, and sets are rejected) | done |
| Idoc | `indexed-paths` and `excluded-paths` per attribute (excluded wins; selectors without positions cover array elements) | done |
| Idoc | named domains: `Conn::set_idoc_domain` shares selectors across attributes that name the domain (set before indexing; not persisted) | done |
| Idoc | `:db/noindex` (values kept on the entity; value-constrained queries rejected; unique attributes stay indexed) | done |
| Idoc | Markdown input (`idoc_value_markdown`): headers nest, sections store text, original's errors reproduced; inline markdown is read as plain text (the original uses a Markdown library) | done |
| Idoc | mixed named and free array positions in one path (each array level is indexed both ways; leaves under more than six array levels keep only the two extremes) | done |
| Idoc | async indexing: `idoc_async` attributes record index jobs in the writing transaction; `process_idoc_jobs` applies them in order; jobs survive restart; `idoc_jobs_pending`. Processing is explicit (no background worker thread) | done |
| Idoc | array values in patterns (containment: the document array at the path must contain every listed element; object elements match across elements, so one array element need not satisfy all fields) | done |
| Codecs | nippy, bits, zstd dictionary compression, CSV, Hu-Tucker keys, roaring | todo (in `datalevin/src/rust`) |

## Behavioral notes

- Scalar `:in` inputs are substituted at parse time. Other shapes are seeded into the evaluation environment.
- Integer `/` returns `I64` only when exact, otherwise `F64`. Integer overflow and division by zero are `DatalogError::Function` errors.
- `or-join` branches keep only outer and join variables. `not-join` clauses see only join variables from the outer scope.
- Explicit entity ids advance the fresh-id counter, so later allocations cannot collide with them.
- `get-else` and `missing?` on an unknown attribute return an error, rather than treating the attribute as absent.
