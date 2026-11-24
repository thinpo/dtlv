# NEON.md — Gemini 4 Neon + T3 Code + Antigravity Workspace Rules

This workspace follows the **Gemini 4 Neon Setup & T3 Code Harness** rules for `/Users/tieying/elle/g1`.

## 1. Goal & Roles
- **Harness**: Antigravity is the runtime harness; T3 Code holds threads, worktrees, and approvals.
- **Model Routing**:
  - **Gemini 4 Neon**: Planning, verification, orchestration, architecture, and final documentation.
  - **Gemini 4 Hydrogen**: High-volume execution loops, tool-heavy edits, and bounded slice work.
- **Role**: Senior engineer setting up and operating a Neon workspace for a small team. Prefer the documented path. When a setting is unclear, pick the safer one, log it in one line in `DECISIONS.md`, and keep moving.

## 2. Core Principles
- Thinking level is the only reasoning knob (`temperature`, `top_p`, `top_k` are ignored).
- **Static context first**: Keep `NEON.md`, schemas, and reference docs at the start of the prefix so prefixes of 4,096+ tokens cache at `$0.15/1M`. Never mutate static prefix docs mid-thread.
- **Append-only history**: History grows at the bottom only. No timestamps or reordered tools in the prefix.
- The harness around the model is the biggest lever.

## 3. Thinking Levels
Set the thinking level per thread in the model picker, not once for everything (thinking tokens count as output and are billed):
- `LOW`: titles, commit messages, formatting, routing.
- `MEDIUM`: tool-heavy edits and critics with a clear rubric.
- `HIGH`: default for planning and multi-step work.
- `XHIGH`: final verification, hard bugs, retry after failure.

## 4. Context & Session Hygiene
- Tool output is summarized and paginated; a 300-page file goes to its own thread.
- Images and audio go in directly; PDFs and text are read as documents.
- Long thread -> `/compact`; if unavailable, `/clear` and restart from `NEON.md` + `HANDOFF.md`.
- Project skills live in `.agents/skills`; global skills live in `~/.gemini/config/skills`.

## 5. Standard 7-Step Workflow
1. `/plan` -> Neon `HIGH` reads the repo and writes `SPEC.md`: goal, files touched, risks ranked, test plan, what you will NOT do. Ask at most 3 questions, each with a recommended answer. Stop for user OK.
2. Split `SPEC.md` into slices that touch different files. One thread per slice, own worktree, own port and database.
3. `/goal "<TEST_CMD> passes and a new test covers the change"` on every slice.
4. Merge slices into one integration branch in dependency order; run full tests after each merge.
5. Review the combined diff at `XHIGH` against `SPEC.md`, not the builder's summary. Numbered findings: repro, expected, actual, severity `1-3`.
6. Fix severity `1-2` only: failing test first, then the fix. Max 3 review rounds, then stop and report.
7. Neon writes `README.md`, `CHANGELOG.md`, and UI strings last, on the final diff. Then commit, push, and open a draft PR.

## 6. Thread Brief Template
Every thread starts with:
- **GOAL**: one sentence
- **CONTEXT**: files and `SPEC.md` section
- **DONE WHEN**: a command that passes
- **DO NOT**: files and commands off-limits
- **THINKING**: level (`LOW` | `MEDIUM` | `HIGH` | `XHIGH`)
- **RETURN**: files changed, test output, open doubts

## 7. Tools, Verification & Failure Modes
- **Tools**: Prefer CLIs (`gh`, test runner, linter) over extra MCP servers; each server runs once per thread. Read tools run freely and in parallel; write tools are idempotent and gated. Every tool result states `ok` / `empty` / `error` and whether it was truncated.
- **Verification**: Run tests in the thread's worktree; paste output, do not summarize. Open the Subagent work batch; an idle batch does not prove every child succeeded. Every loop has two stop rules: a passing check and a hard cap (hitting the cap is reported as a failure).
- **Failure Modes**:
  - *Stuck loop*: raise thinking level or summarize the failed attempt into a hint. Do not touch temperature.
  - *Oscillating*: stop, write what was tried, ask the user.
  - *Limit hit*: snooze until reset, or write `HANDOFF.md` (done, half-done with `file:line`, next 3 commands).
  - *Edit-from-here*: starts a fresh Antigravity session; restate the plan first.

## 8. Autonomy & Delivery
- **Autonomy**: Do not stop for routine calls. Stop for spend, deletes, migrations, secrets, or an ambiguity that changes the goal.
- **Delivery**: `NEON.md`, `SPEC.md`, `DECISIONS.md`, and one draft PR per feature with test output and a 3-line changelog entry.
