# SPEC.md — Trading Reference Data Datalog Comparison (`g1`)

## 1. Goal
Establish a reproducible Gemini 4 Neon + T3 Code + Antigravity workspace for `/Users/tieying/elle/g1` (a Clojure `deps.edn` benchmark/comparison suite across Datalevin, Datahike, XTDB v2, Datomic Local, and DataScript) and extend test coverage cleanly across isolated slices.

## 2. Files Touched
### Setup (Completed in Planning)
- `NEON.md` — workspace rules, model routing, thinking levels, caching, workflow, and verification guardrails.
- `DECISIONS.md` — one-line architectural/setup decision log.
- `SPEC.md` — this specification.

### Planned Execution Slices (Disjoint Files)
- **Slice 1 (Build & Git Hygiene)**: `.gitignore`, `deps.edn` (add a `:test-runner` alias or `clojure.test` main entry so `TEST_CMD` is a single short command).
- **Slice 2 (Test Coverage & Benchmark Assertions)**: `test/refdata/comparison_test.clj` (add/verify edge-case coverage, e.g., empty/missing ticker lookup or deterministic result recording).
- **Slice 3 (Final Docs — Neon Last)**: `README.md`, `CHANGELOG.md` (written last against the final combined diff).

## 3. Risks Ranked
1. **High — Sandbox Restrictions on XTDB v2 & Datalevin**: Inside a restricted sandbox, `xtdb-v2-refdata-test` fails with `java.net.SocketException: Operation not permitted` and `datalevin-refdata-test` fails with `datalevin.cpp.Util$DTLVException: Operation not permitted`. *Mitigation*: Run `TEST_CMD` with local socket/LMDB permissions enabled (verified: 5 tests, 33 assertions, 0 failures, 0 errors).
2. **Medium — Concurrent Worktree Temp Dir Collisions**: `refdata.dataset/temp-dir` uses `/tmp/<prefix>-<uuid>`, which is safe across parallel worktrees, though XTDB v2 node ports must not collide if multiple slices run simultaneously. *Mitigation*: One thread/worktree per slice; run full integration tests sequentially after merge.
3. **Low — No Remote Configured Yet**: `git init` created a local repo without an `origin` remote, so `git push` / draft PR creation requires adding a remote first.

## 4. Test Plan
### Canonical `TEST_CMD`
```sh
clojure -M:test -e "(require 'clojure.test 'refdata.comparison-test) (let [res (clojure.test/run-tests 'refdata.comparison-test)] (System/exit (if (and (zero? (:fail res)) (zero? (:error res))) 0 1)))"
```

### Verified Baseline Output
- `Ran 5 tests containing 33 assertions.`
- `0 failures, 0 errors.`
- Engines verified: `datalevin`, `datahike`, `xtdb-v2`, `datomic-local`, `datascript`.

## 5. What We Will NOT Do
- We will **not** modify `src/refdata/dataset.clj` benchmark parameters (`num-securities`, `num-read-warmup`, `num-read-samples`) without an explicit benchmark change request.
- We will **not** commit `.cpcache/` or temporary `/tmp/*` database directories.
- We will **not** push to a remote or open a PR until a Git remote is configured and `XHIGH` diff review passes.
