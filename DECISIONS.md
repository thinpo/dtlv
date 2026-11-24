# DECISIONS.md

- 2026-11-24: Initialized local Git repository in `/Users/tieying/elle/g1` because worktrees, integration branches, and draft PRs require Git.
- 2026-11-24: Standardized `TEST_CMD` on `clojure -M:test -e "(require 'clojure.test 'refdata.comparison-test) (let [res (clojure.test/run-tests 'refdata.comparison-test)] (System/exit (if (and (zero? (:fail res)) (zero? (:error res))) 0 1)))"` until a dedicated test-runner alias is added to `deps.edn`.
- 2026-11-24: Noted that `refdata.comparison-test` must run with host socket/LMDB permissions (`BypassSandbox: true` in Antigravity) because XTDB v2 binds a local pgwire socket and Datalevin uses LMDB native env creation.
- 2026-11-24: Authored `NEON.md`, `DECISIONS.md`, and `SPEC.md` from the current Gemini 4 Hydrogen (Medium) session; ready for Gemini 4 Neon (High/XHigh) in T3 Code to execute and review slices.
