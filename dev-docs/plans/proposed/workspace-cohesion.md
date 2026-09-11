---
title: "Workspace Cohesion - Implementation Plan"
date: 2026-09-11
author: agent
issue: dev-docs/issues/open/workspace-cohesion.md
design: dev-docs/designs/proposed/workspace-cohesion.md
---

# Workspace Cohesion - Implementation Plan

## Table of Contents

- [Targets and phases](#targets-and-phases)
- [The gate](#the-gate)
- [Executor tiers](#executor-tiers)
- [Tickets](#tickets)
  1. [Merge `main` into `main-rc-1`](#1-merge-main-into-main-rc-1)
  2. [CI runs the tests that exist](#2-ci-runs-the-tests-that-exist)
  3. [Close the embedder log-sink hole](#3-close-the-embedder-log-sink-hole)
  4. [One invoke-JSON decoder, one freshness guard](#4-one-invoke-json-decoder-one-freshness-guard)
  5. [Reproducible dependencies](#5-reproducible-dependencies)
  6. [Fix the `printf` width panic in the coreutils fork](#6-fix-the-printf-width-panic-in-the-coreutils-fork)
  7. [Extract `clank-native`](#7-extract-clank-native)
  8. [Extract `grease-pkg`](#8-extract-grease-pkg)
  9. [Consolidate HTTP into `whttp`](#9-consolidate-http-into-whttp)
  10. [Decompose the classification ladders; delete dead code](#10-decompose-the-classification-ladders-delete-dead-code)
  11. [Single-source the command reference; extract architecture docs](#11-single-source-the-command-reference-extract-architecture-docs)
  12. [Generate the fork inventory; fix documentation drift](#12-generate-the-fork-inventory-fix-documentation-drift)
  13. [Typed errors at the seams](#13-typed-errors-at-the-seams)

## Targets and phases

| Phase | Tickets | Theme | Blocking? |
|---|---|---|---|
| **P0** | 1 | Merge `main` | Blocks everything |
| **P1** | 2–6 | Close the silent-failure gaps | Blocks P2 |
| **P2** | 7–10 | Finish the crate split | — |
| **P3** | 11–12 | Consolidate documentation | Independent of P2 |
| **P4** | 13 | Error taxonomy | Separable; may be deferred |

P3 does not depend on P2 and can run in parallel with it. P4 is explicitly droppable — P0–P3 stand
alone as a complete piece of work.

## The gate

Every ticket closes on all of:

```bash
cargo fmt --check
cargo clippy --all-targets -p clank-core -p clank-cli -p clank-embed \
  -p whttp -p wcurl -p waget -p clank-conformance -- -D warnings
cargo clippy --target wasm32-wasip2 -p clank-agent -p greeter-agent -- -D warnings
cargo test --workspace -- --test-threads=1
cargo test -p clank-conformance --test native
```

`--test-threads=1` is load-bearing, not caution: Brush resets `SIGPIPE` to `SIG_DFL`, and under
libtest's thread pool one test's broken-pipe write kills the process with signal 13.

Tickets touching the agent additionally require `scripts/golem-e2e.sh --takeover` green, and
**the assertion count must not fall** — a restructure that quietly drops coverage is the failure
mode to guard against. Record the count in the ticket's deviations note.

## Executor tiers

| Tier | Meaning | Tickets |
|---|---|---|
| **mechanical** | Moves, renames, manifest edits, doc edits. Safe for a cheap model behind the gate. | 5, 11, 12 (partly), and the file-move steps of 7, 8 |
| **assisted** | Mechanical core, judgement at the edges. Cheap model drafts, reviewed before the gate. | 2, 4, 9, 12 |
| **judgement** | Semantic change to load-bearing logic. Not for a cheap model. | 1, 3, 6, 10, 13, and the seam-rewiring of 7 |

Ticket 1 in particular must not be delegated to a cheap model: it is a delete/modify conflict over
a 5,511-line test file whose resolution requires understanding what each test asserts.

---

## Tickets

### 1. Merge `main` into `main-rc-1`

**Executor:** judgement · **Phase:** P0 · **Blocks:** everything

`main` is 46 commits ahead at merge-base `8dfbf19`; 84 source files overlap. It carries four
silent-success fixes (`f948a3c`), four durable-agent-wedging panic fixes (`8808547`), a resilience
conformance tier, and the `session/tests.rs` split (`8c290a9`).

**Files — the conflict surface, by combined churn:**

| File | main | rc-1 | Nature |
|---|---:|---:|---|
| `crates/clank-core/src/session/tests.rs` | 4,568 | 1,675 | **delete/modify** — main split it into `session/tests/{agent,ask,authz,eval,grease,http,logging,mcp,prompt,resolution,secrets}.rs` |
| `crates/clank-core/src/session/mod.rs` | 1,105 | 425 | both edited |
| `crates/clank-core/src/native.rs` | 561 | 553 | both edited heavily — most likely genuine semantic conflict |
| `crates/clank-core/src/session/ask.rs` | 515 | 400 | both edited |
| `crates/clank-core/src/session/grease.rs` | 713 | 164 | main-dominant |
| `crates/clank-core/src/tools/coreutils.rs` | 366 | 348 | both edited |
| `utilities/whttp/src/lib.rs` | 416 | 215 | rc-1 side is the wasi-fetch migration |
| `crates/clank-agent/src/ask_provider.rs` | 136 | 178 | **rename/modify** — rc-1 moved it to `clank-embed` |
| `Cargo.lock` | — | — | regenerate, never hand-merge |

`reedline-fork/` and `crossterm-fork/` show as touched on both sides but are **byte-identical
between the branches** — verified, they merge trivially.

**Steps:**

1. Branch a backup: `git branch main-rc-1-premerge-20260911`.
2. `git merge main`. Expect ~84 conflicts.
3. Resolve `session/tests.rs` **by taking `main`'s deletion** — i.e. accept the eleven-module
   split — then redistribute rc-1's 1,675 lines of test changes into the matching module by
   concern. Use `git show $(git merge-base main main-rc-1):crates/clank-core/src/session/tests.rs`
   as the base to diff rc-1's changes against.
4. Resolve `ask_provider.rs` by applying `main`'s edits to the file at its **new** location,
   `crates/clank-embed/src/ask_provider.rs`.
5. Resolve the rest by hand. `native.rs` is the one to read most carefully.
6. Regenerate `Cargo.lock` (`cargo check --workspace`), never hand-merge it.
7. Preserve the lock ordering rule while merging tests: **grease before mcp**, everywhere. The
   reverse deadlocks the suite AB-BA. `main` documents it at the top of `session/tests/mod.rs`.

**Tests:** the full gate, plus `scripts/golem-e2e.sh --takeover`. Both resilience scenarios from
`main` must run and pass on both conformance tiers.

**Done when:** `git merge-base --is-ancestor main HEAD` succeeds; the eleven `session/tests/`
modules exist and every test that was in rc-1's `tests.rs` has a home; the gate is green; the e2e
assertion count is ≥ the pre-merge count on both branches.

---

### 2. CI runs the tests that exist

**Executor:** assisted · **Phase:** P1

554 `#[test]` functions and a 1,552-line e2e currently have no automated trigger.

**Files:** `.github/workflows/conformance.yml`

**Steps:**

1. Add to the push-triggered `native` job, after the existing conformance steps:
   ```yaml
   - name: Workspace unit tests
     run: cargo test --workspace -- --test-threads=1
   ```
2. Add a new job on the same nightly `cron`/`workflow_dispatch` trigger the `golem` job uses,
   running `scripts/golem-e2e.sh --takeover`. Give it `timeout-minutes: 60`.
3. Do **not** put the e2e on push — it needs a server, a deploy and tens of minutes.

**Tests:** push the branch and confirm both jobs run and pass. Deliberately break one `clank-core`
test locally, confirm the workspace job would fail, revert.

**Done when:** a red `clank-core` test turns CI red; the e2e runs on the nightly schedule.

**DONE 2026-09-11 (`a4899c2`).** Deviations:

- **Half of this ticket arrived with the merge.** `main`'s `469211b` had already added the
  library-crate test step. The premise ("554 tests never run in CI") was true of `main-rc-1` when
  the audit ran and false by the time the ticket was executed. **Re-check an audit finding against
  the tree after a merge, not against the audit.**
- **A gap no audit found: 8 tests ran nowhere at all.** `clank-embed` has 13 tests, 8 behind the
  non-default `providers` feature. Neither `-p clank-embed` nor `cargo test --workspace` enables a
  non-default feature, so they executed under no invocation anyone runs — and they cover the wRPC
  encoding the merge had just rewritten. Own CI step now. **Worth a sweep: any other crate with a
  non-default feature gating tests is invisible the same way.**
- **`--test-threads=1` was documented-as-required and not honoured by CI.** Added to every test
  step; it costs nothing (the suites run in under a second) and removes a latent SIGPIPE flake.
- **The new `e2e` job cannot pass on this branch** — CI pins golem 1.5.1, the manifest is 1.6.0.
  Stated in the job comment with the local workaround rather than left as a silently-red job.
  Aditya's call whether to gate both it and the existing `golem` job off until a 1.6 CLI ships.
- **Finishing `GOLEM_BIN` was not in the ticket and should have been.** A half-parameterised script
  works only while the right binary happens to be on PATH, then fails as "server did not become
  ready in 60s" — pointing at the server, not the missing binary. Cost a full e2e run to find.
  All four harnesses now route every invocation through `$GOLEM`; the sweep also caught
  `clank-repl.sh` starting its server without `-Y`.

---

### 3. Close the embedder log-sink hole

**Executor:** judgement · **Phase:** P1

Four seams degrade to an honest error; `LogSink` degrades to a silently-wrong append that
duplicates `/var/log` under replay. The correct sink is `pub(crate)`, so no embedder can install
it, and the documented example demonstrating exactly that is in an `ignore`d fence and does not
compile.

**Files:** `crates/clank-embed/src/shell.rs`, `crates/clank-embed/src/log_sink.rs`,
`crates/clank-embed/src/lib.rs`

**Steps:**

1. `log_sink.rs`: `pub(crate) struct DurableLogSink` → `pub`; same for `new()`.
2. `lib.rs`: add `pub use log_sink::DurableLogSink;` to the existing re-export block.
3. `shell.rs`: install `DurableLogSink` in `new()` and `with_setup()`, not only in
   `with_durable_log_sink()` and `with_default_golem_providers()`. `clank-embed` targets Golem
   agents; the replay-safe sink is always correct there.
4. Mark `with_durable_log_sink()` `#[deprecated(note = "every constructor now installs the durable
   sink")]` — keep it, `greeter-agent` calls it.
5. Convert the `with_setup` doc example from ```` ```ignore ```` to a real doctest. It currently
   names `clank_embed::log_sink::DurableLogSink::new()`, which step 1 makes valid.

**Tests:** a new `clank-embed` test asserting `EmbeddedShell::new()`'s session carries a sink that
is *not* `DefaultLogSink` (expose a `#[cfg(test)]` accessor or assert via behaviour — two writes
to the same log path must not duplicate). `cargo test --doc -p clank-embed` must compile the
example.

**Done when:** every constructor installs the replay-safe sink; `DurableLogSink` is nameable
externally; the doctest compiles and runs.

---

### 4. One invoke-JSON decoder, one freshness guard

**Executor:** assisted · **Phase:** P1

The decoder exists five times; only the Rust one is robust. `clank-repl.sh` is broken by the
released-shape-only version right now. Separately, nothing checks that the deployed wasm is newer
than its sources.

**Files:** new `scripts/lib/golem-json.sh`; `scripts/golem-e2e.sh`, `scripts/golem-probe.sh`,
`scripts/clank-repl.sh`

**Steps:**

1. Create `scripts/lib/golem-json.sh` exporting `eval_json()` and `run_line()`. Handle **both**
   shapes, trying named first then positional, mirroring
   `crates/clank-conformance/src/backend/golem.rs::decode_invoke`:
   - named: `.result_json.value.{stdout,stderr,exit_code}`
   - positional: `.resultJson.value.value.fields[0..3].value`, option via `.inner`, list via
     `.inner.value.elements[].value`
2. Add `assert_fresh_artifact()` to the same file: compare `golem-temp/agents/clank_agent_debug.wasm`
   mtime against the newest `.rs`/`.toml` under `crates/` and `utilities/`; when stale, run
   `cargo clean -p clank-core -p clank-agent --target wasm32-wasip2` and report it.
3. Source the library from all three scripts; delete their local copies.
4. Call `assert_fresh_artifact` immediately before `golem build` in `golem-e2e.sh`,
   `golem-probe.sh`, `conformance-golem.sh` and `clank-repl.sh --deploy`.
5. Fix `golem-e2e.sh`'s stale header comment: it claims "Exits non-zero on the first failed
   assertion"; it actually aggregates and exits non-zero at the end.

**Tests:** run `golem-probe.sh 'echo hi'` and confirm real output. Run `clank-repl.sh` and confirm
it is functional again. Touch a `clank-core` source file, run the e2e, and confirm the freshness
guard fires and rebuilds.

**Done when:** three scripts share one decoder; `clank-repl.sh` works; a stale artifact is
detected and remedied rather than deployed.

**DONE 2026-09-11 (`c99cba6`).** Deviations from the plan as written:

- **The library also serves `conformance-golem.sh`** (four harnesses, not three). That script has
  no decoder of its own — the Rust backend decodes — but it deploys, so it needs the freshness
  check. The ticket missed it.
- **The remedy is deletion, not `cargo clean`.** The plan prescribed
  `cargo clean -p clank-core -p clank-agent --target wasm32-wasip2`; that removes 1,357 files /
  1.5 GB and forces a long rebuild. Deleting the staged artifacts is sufficient and far cheaper:
  it forces `golem build` to re-invoke cargo, and cargo then rebuilds exactly what changed —
  cargo tracks path deps correctly, the gap is only whether it is invoked at all. **Both** the
  `golem-temp/` copy and cargo's `target/` copy must go; removing only the former lets `golem build`
  re-stage the same stale wasm without running cargo.
- **Shell-portability details worth keeping:** enumerate with `find`, not a glob (unmatched globs
  behave differently across shells, and a missing `golem-temp/` must be a silent no-op rather than
  an error), and use `find -newer` rather than `stat(1)`, whose mtime flag differs between BSD and
  GNU.
- **`golem-e2e.sh`'s header was also wrong** and is corrected: it claimed to exit on the first
  failed assertion; it aggregates and exits at the end.

Verified both branches of the guard — a touch on `crates/clank-core/src/lib.rs` (a path dep
`golem build` does not track) is detected and cleared; a tree with nothing to check is silent.
Full e2e with both changes wired: **274 passed, 0 failed.**

---

### 5. Reproducible dependencies

**Executor:** mechanical · **Phase:** P1

`golem-rust` is an absolute machine-local path in three manifests; `wit-bindgen` is branch-pinned;
`clank-embed` has no lints stanza.

**Files:** root `Cargo.toml`; `crates/clank-agent/Cargo.toml`, `crates/clank-embed/Cargo.toml`,
`fixtures/greeter-agent/Cargo.toml`; every member manifest

**Steps:**

1. Add `golem-rust` to `[workspace.dependencies]` once. **Keep the path ABSOLUTE.** This looks
   like the obvious thing to fix and is not: `~/Desktop/clank-spike` is a git worktree, and mixing
   relative and absolute path-dep forms across a worktree has previously produced a cargo
   "package collision in the lockfile". The defect this ticket fixes is the path being repeated in
   three manifests, not its absolute-ness. Change the three consumers to `{ workspace = true }`,
   preserving each one's feature set (`export_golem_agentic` on the two leaf agents only).
   If making it relocatable is wanted later, that is a separate change requiring a lockfile
   verification on a fresh clone.
2. Move every other shared dependency (`serde`, `serde_json`, `http`, `async-trait`, `tokio`,
   `wasi-fetch`, `wasip3`, …) into `[workspace.dependencies]`; members reference
   `{ workspace = true }`.
3. Rev-pin `golemcloud/wit-bindgen` in `[patch.crates-io]` — replace `branch = "..."` with the
   resolved `rev` from `Cargo.lock` (currently `4407232`).
4. Add `[lints]\nworkspace = true` to `crates/clank-embed/Cargo.toml`.
5. Fix `clank-embed`'s `wasi-fetch`/`http` deps to sit under
   `[target.'cfg(target_arch = "wasm32")'.dependencies]`, matching every other HTTP split in the
   workspace. They are currently gated only by the `providers` feature.
6. Update `rustfmt.toml`'s header comment, which still claims the workspace lints are not enabled.

**Tests:** the full gate. Expect new clippy findings in `clank-embed` from step 4 — fix them; that
is the point of the ticket.

**Done when:** `grep -rn "/Users/" --include=Cargo.toml .` returns at most one hit; `cargo deny
check` passes with the rev pin; `clank-embed` compiles clean under the workspace lints.

---

### 6. Fix the `printf` width panic in the coreutils fork

**Executor:** judgement · **Phase:** P1 · **Repo:** `Aditya1404Sal/coreutils` (external)

Rust ≥1.88 stores `core::fmt` widths as `u16`. `printf '%150000s' ''` panics in `write_padded`
(`Argument::from_usize`) and, on a durable agent, permanently wedges the instance. Reachable from
any user script. Bisected: 65,000 works, 70,000 traps. Only the e2e's call site is worked around;
the fork is unpatched.

**Steps:**

1. In the fork, replace the `fmt`-width padding path with an explicit loop or
   `iter::repeat_n(b' ', n)` write that carries no `core::fmt` width.
2. Add a regression test at width 100,000.
3. Tag a rev; bump the `[patch.crates-io]` pin in clank's root `Cargo.toml`.
4. Revert `scripts/golem-e2e.sh`'s three-conversion workaround to the original single
   `printf '%150000s' ''` — it is now the regression test.

**Tests:** `printf '%150000s' '' | wc -c` returns 150000 on both targets; the agent answers
normally afterwards (the wedge signature is "Previous invocation failed" on every later call).

**Done when:** the single-conversion form is back in the e2e and passes; the fork carries the fix
at a pinned rev.

---

### 7. Extract `clank-native`

**Executor:** mechanical (moves) + judgement (rewiring) · **Phase:** P2

The wasm platform layer lives in `clank-embed`; the native one was folded back into `clank-core`,
making the crate boundary asymmetric for no principled reason.

**Files:** new `crates/clank-native/`; move `crates/clank-core/src/native.rs`,
`ai/anthropic_native.rs`, `mcp/http_native.rs`, `golem/rest_native.rs`, `golem/config_native.rs`;
edit `crates/clank-core/src/{lib,ai/mod,mcp/mod,golem/mod}.rs`; `crates/clank-cli/Cargo.toml`

**Steps:**

1. Create `crates/clank-native` with `[lints] workspace = true` and the native-only dependencies
   currently in `clank-core` (`reqwest`, `reedline`, `crossterm`, `nu-ansi-term`, `libc`,
   `tempfile`).
2. Move the five files. Keep module paths stable inside the new crate
   (`clank_native::{run, anthropic, mcp_http, rest, config}`).
3. Delete the `#[cfg(not(target_arch = "wasm32"))]` module declarations they leave behind in
   `clank-core`.
4. Repoint `clank-cli` to depend on `clank-native`; its `main.rs` calls `clank_native::run()`.
5. Mirror `clank-embed`'s shape: `clank-native` gains an `inject_native_providers(&mut Session)`
   as its single wiring entry point, matching `with_default_golem_providers`.
6. Remove `reqwest` and the TUI stack from `clank-core`'s manifest entirely.

**Tests:** the full gate. `cargo tree -p clank-core --target wasm32-wasip2` must not mention
`reqwest`, `reedline` or `crossterm`; `cargo tree -p clank-core` (native) must not either.

**Done when:** `clank-core` has no target-specific platform code and no native-only dependency;
`clank-native` and `clank-embed` are symmetric.

---

### 8. Extract `grease-pkg`

**Executor:** mechanical · **Phase:** P2

`grease-tool` pulls all ~38 of `clank-core`'s dependencies to use one self-contained module.

**Files:** new `crates/grease-pkg/`; move `crates/clank-core/src/grease/pkg.rs` and the
`ParamSpec`/`ParamType` slice of `manifest.rs`; edit `crates/clank-core/src/grease/mod.rs`,
`dev-tools/grease-tool/Cargo.toml`

**Steps:**

1. Create `crates/grease-pkg` depending only on `serde`, `serde_json`, `sha2`, `ed25519-dalek`,
   `base64`.
2. Move `grease/pkg.rs` in whole. Move `ParamSpec`/`ParamType` from `manifest.rs` — they are the
   only intra-crate dependency `pkg.rs` has.
3. `clank-core` re-exports them (`pub use grease_pkg::...`) so no existing path breaks.
4. `grease-tool` depends on `grease-pkg` directly and drops its `clank-core` dependency.

**Tests:** the full gate. `cargo tree -p grease-tool | wc -l` must drop substantially — record the
before and after in the deviations note.

**Done when:** `grease-tool` no longer depends on `clank-core`; the signing and
transparency-log tests still pass from their new home.

---

### 9. Consolidate HTTP into `whttp`

**Executor:** assisted · **Phase:** P2

`whttp` was created to end HTTP duplication; there are five independent client call sites, only
two of which use it.

**Files:** `utilities/whttp/src/lib.rs`; `crates/clank-native/src/{anthropic,mcp_http,rest}.rs`
(post-ticket-7); `crates/clank-embed/src/{mcp_http,ask_provider}.rs`

**Steps:**

1. Add to `whttp`: a `client()` constructor per target (the rustls `reqwest` builder natively, the
   `wasi_fetch::Client` on wasm), and `enforce_body_cap(headers, bytes, max) -> Result<(), Error>`
   implementing the Content-Length-pre-check-then-post-hoc-cap guard.
2. Replace the three native `reqwest::Client::builder()...` idioms with `whttp::client()`.
3. Replace the duplicated Content-Length guard in `clank-embed/src/mcp_http.rs` and
   `ask_provider.rs` with `whttp::enforce_body_cap`.
4. Keep the header pre-validation at each call site — the messages are usefully specific
   (`wasi-fetch` silently drops unparseable headers, and MCP's session-id case reads differently
   from `ask`'s API-key case).

**Tests:** the full gate plus `scripts/golem-e2e.sh` (exercises `curl`, `wget`, and — gated — MCP
and `ask`). Add a `whttp` unit test for `enforce_body_cap` covering: declared-over-cap rejected,
actual-over-cap rejected, absent Content-Length falling through to the post-hoc check.

**Done when:** no `reqwest::Client::builder` or `wasi_fetch::Client::new` outside `whttp`; one
body-cap implementation.

---

### 10. Decompose the classification ladders; delete dead code

**Executor:** judgement · **Phase:** P2

`eval_line_inner` is 379 lines and `run_command` 114 — two sequential first-match-wins ladders
that are the least readable code in the crate. Separately, several things are dead or misfiled.

**Files:** `crates/clank-core/src/session/mod.rs`; `crates/clank-core/src/wasm.rs` (delete);
`crates/clank-core/src/runtime/process.rs` (delete); `crates/clank-core/src/builtins/helpshim.rs`
(move); `crates/clank-core/src/{ai/ask.rs, mcp/state.rs}`

**Steps:**

1. Extract `eval_line_inner`'s ladder into a `classify_line(&self, line) -> LineRoute` returning an
   enum of the dispatch targets, leaving `eval_line_inner` as guard-clauses plus a `match`. Do
   **not** reorder the ladder — first-match-wins order is behaviour.
2. Same for `run_command`'s ladder.
3. Delete `wasm.rs` and the `repl-driver` feature. It is dead — **verified 2026-09-11**: it is
   `default` in `clank-core`, but `clank-cli`, `clank-embed`, `clank-conformance` and
   `grease-tool` all pass `default-features = false`, so no crate in the workspace enables it.
   (Two audit agents disagreed on this; the manifests settle it.) It is also latently broken: it
   discards `pending_prompt`, so a confirm-gated command would print its question and then wedge
   the session if the driver were ever revived as-is.
4. Delete the `ClankProcess` trait (`runtime/process.rs`) — zero implementors, self-documented as
   scaffolding.
5. Move `builtins/helpshim.rs` to a top-level `helpshim.rs` beside `registry.rs`; it is consumed
   from four concern directories and is generic Brush plumbing, not a builtin.
6. Unify the three `--help` mechanisms (`typecmd::help_for`, `session::pkg_help_for`,
   `helpshim::WithHelp`) behind one entry point. Preserve behaviour exactly — both intercept
   paths already skip a leading `sudo`, and that is load-bearing.
7. Break the `ai ↔ mcp` cycle: move the `McpState → Vec<AskTool>` conversion out of
   `mcp/state.rs` into `ai/`, so the dependency runs one way.

**Tests:** the full gate plus `scripts/golem-e2e.sh`. The e2e's help and resolution sections
(§2i, plus the `--help` assertions) are the specific regression guard for step 6.

**Done when:** no function in `session/` exceeds ~120 lines; `repl-driver` and `ClankProcess` are
gone; `--help` has one implementation; `cargo tree`-level cycle between `ai` and `mcp` is gone.

---

### 11. Single-source the command reference; extract architecture docs

**Executor:** mechanical · **Phase:** P3

`README.md` (1,065 lines) and `docs/USAGE.md` (901) independently document the same surface. The
module-level `//!` headers are the only architecture documentation that exists.

**Files:** `README.md`, `docs/USAGE.md`; new `docs/architecture/{wall-c,replay-safety,wasip2-constraints,resolution-surface}.md`;
the module headers of `builtins/http.rs`, `builtins/interceptstub.rs`, `session/mod.rs`,
`ai/ask.rs`, `logging.rs`, `clank-embed/src/log_sink.rs`, `tools/coreutils.rs`

**Steps:**

1. Move `README.md` lines ~310–1065 into `docs/USAGE.md`, merging section-by-section where USAGE
   already covers the topic. README ends with a pointer.
2. Write `docs/architecture/wall-c.md` — the canonical explanation: `SimpleCommand::execute` is
   synchronous by signature on both targets; a WASI-HTTP future polled by the nested tokio runtime
   is never woken because nothing performs the component-model wait; therefore async work is
   awaited at the Session layer and is top-level-only. Gather the eight partial explanations
   currently spread across the codebase.
3. Write `docs/architecture/replay-safety.md` — `std::fs` append duplicates under oplog replay
   because the filesystem is re-run guest code, not a restored snapshot; whole-file `fs::write` is
   idempotent. Cite the log sink and the grease store.
4. Write `docs/architecture/wasip2-constraints.md` — the table of missing primitives (process
   spawn, `pipe(2)`, threads, `dup2`, blocking stdin, `stat(2)`, `nix`) and what each forces.
5. Write `docs/architecture/resolution-surface.md` — the virtual `/bin`, `/proc`, `/mnt/mcp`
   namespaces and the `which`/`type` split.
6. Replace each module header's long explanation with a one-line statement plus a pointer. **Keep
   every trap comment in place** — the `SILENTLY DROPS`, `replay-unsafe`, `deadlock` and
   named-historical-bug comments stay exactly where they are.

**Tests:** `cargo doc --no-deps` clean. Grep that no `//!` header lost a load-bearing fact:
`grep -rn "SILENTLY DROPS\|replay-unsafe\|load-bearing\|deadlock" crates/ utilities/` must return
the same count before and after.

**Done when:** the command surface is documented once; the four architecture pages exist and are
linked from `docs/ONBOARDING.md`.

---

### 12. Generate the fork inventory; fix documentation drift

**Executor:** assisted · **Phase:** P3

`docs/WASM_CHANGES.md` asserts two forks where there are three, and drifts every time a pin moves.
Several documents point at files that never existed.

**Files:** new `dev-tools/fork-inventory/`; new `docs/FORKS.md` (generated);
`.github/workflows/conformance.yml`; `AGENTS.md`, `DEV_SDK_CHANGES.md`, `docs/WASM_CHANGES.md`,
`docs/audit/*.md`, `dev-docs/**`

**Steps:**

1. Write `dev-tools/fork-inventory` — a std-only Rust binary (mirroring Golem's zero-dependency
   `dev-tools` convention) that reads `Cargo.toml` and `Cargo.lock` and emits `docs/FORKS.md`:
   every `[patch.crates-io]` entry and every `git+` source in the lock, with repo, pin kind
   (rev/branch), resolved rev, and the crates it replaces. Vendored path-forks
   (`reedline-fork`, `crossterm-fork`) listed separately.
2. Add a `check-forks` CI step: regenerate and `git diff --exit-code`.
3. Cut the fork inventory out of `docs/WASM_CHANGES.md`; it keeps the *rationale* per fork and
   links to the generated table for the facts. Fix its header (still says branch
   `clank-golem-agent`).
4. Fix `AGENTS.md`'s two dead design links (they name files absent from all of git history —
   repoint to `dual-target-shell-loop.md` or remove). Add one hand-authored line above the
   `<!-- golem-managed -->` marker noting clank's HTTP transport is `wasi-fetch`/`reqwest` via
   `whttp`, not `wstd`, since the managed block says otherwise and the CLI rewrites it.
5. Fix `DEV_SDK_CHANGES.md`'s two links to `WASM_CHANGES.md` (wrong path), or move the file into
   `docs/` beside its self-declared sibling.
6. Banner `docs/audit/{AUDIT,RESTRUCTURE,VERIFY}.md`: "paths predate the 2026-07-23
   `clank-shell`→`clank-core` rename." Note on `RESTRUCTURE.md` that its recommendation shipped.
7. Add a dated addendum to `dev-docs/designs/proposed/clank-golem-agent.md` noting the
   `wstd::block_on` premise was superseded on 2026-09-11 — **then** promote the three stalled
   triads (`clank-golem-agent`, `dual-target-shell-loop`) to `closed/`/`approved/`/`done/`.
8. Resolve `dev-docs/plans/approved/`: either document the stage in `AGENTS.md`'s table or delete
   the directory. Ask Aditya — it is a workflow decision, not a guess.
9. Add a "template only — fictional, not project history" banner to the eight
   `dev-docs/**/example.md` files; their running example (`wstd` vs `reqwest` HTTP abstraction)
   now shadows real history closely enough to mislead a grep.
10. Close `dev-docs/issues/open/shell-owned-virtual-filesystem.md` — it has no design or plan,
    violating the one-of-each rule, and its gap was resolved by later work.

**Tests:** `check-forks` fails when a pin is changed without regenerating. Every markdown link
resolves (a link-check script or manual pass over the ~47 files).

**Done when:** the fork inventory is generated and gated; no dead links; `dev-docs` reflects
reality.

---

### 13. Typed errors at the seams

**Executor:** judgement · **Phase:** P4 · **Separable**

The five seams return `Result<String, String>`. Golem's layering — `anyhow` for propagation,
`thiserror` for domain taxonomies, joined by `SafeDisplay` — is the house pattern to follow.

**Files:** `crates/clank-core/src/{ai/ask.rs, mcp/client.rs, golem/agent.rs, golem/cluster.rs,
logging.rs}`; both platform crates' implementations; every call site in `session/`

**Steps:**

1. Define one `thiserror` enum per seam (`AskError`, `McpError`, `AgentInvokeError`,
   `ClusterError`) with variants for the failure modes each already distinguishes in prose:
   not-configured, transport, protocol/deserialize, capped-body, host-unsupported.
2. Add a `SafeDisplay`-equivalent trait in `clank-core` with `to_safe_string()`, and implement it
   per error type. This is the only sanctioned path from an error to user-visible text — it is
   what keeps a raw transport error out of a shell's stderr.
3. Convert the seam signatures and both implementations per target.
4. Convert the ~17 "not configured" call sites in `session/` to construct the typed variant. The
   exit-code contract (4 for not-configured) must not change; the three divergent "no model
   provider configured" strings collapse to one.
5. Leave `clank-core`'s internal `Result<_, String>` usage alone — out of scope.

**Tests:** the full gate plus the e2e. The e2e's exit-code assertions are the regression guard:
every "not configured" path must still exit 4, and `mcp_session_close`'s deliberate exit-0 must
stay 0.

**Done when:** no seam trait mentions `String` as its error type; user-visible text flows through
`to_safe_string()`; exit codes are unchanged.

---

## Deviations noted during implementation

### Ticket 1 — merged 2026-09-11 (`dadfb15`), follow-up fix `ec1a870`

**The ticket badly overstated `session/tests.rs`.** It budgeted the bulk of the work for
"redistributing rc-1's 1,675 lines of test changes across main's eleven modules." Measuring first
showed 186 test functions on both sides with none added or removed, only two rc-1 commits touching
the file (one a repo-wide `cargo fmt` sweep), and the other's six-line `set_context_cap` change
already present in main's split. The correct action was `git rm`. **Lesson for the remaining
tickets: measure the delta before planning to port it.**

**The ticket understated everything else.** It was scoped as conflict resolution; it was an
architectural re-convergence. main had built five typed-error modules, a `config` module, multi-
provider LLM support, `panicreport`, `session/env`/`session/streams`, and a resilience conformance
tier. Consequence for the plan: **a large part of ticket 13 is already delivered** — re-scope it to
the seams main did not convert rather than all five.

**`ask` now differs by target, which the design did not anticipate.** main's multi-provider routing
needs `golem-ai-llm` (hard-pins `golem-rust = "=2.1.0"`, cannot link here), but main's *native*
dispatcher is pure reqwest — so native `ask` gained openai/grok/openrouter/ollama while the agent
keeps direct-HTTP Anthropic. Aditya approved "Anthropic-only"; the constraint turned out to bind
only the agent side. The follow-up commit adds the provider-prefix split and an honest rejection on
the agent, because `model` accepts every provider clank knows and the transport speaks one.

**Parallel conflict resolution worked and should be reused.** Five sonnet agents took 22 of the 43
files under one written policy. Three caught defects invisible from a conflict hunk in isolation: an
E0592 duplicate definition, an E0255 collision, and — the valuable one — rc-1's `if let Code(want)`
in `matcher.rs`, which would have made `exit nonzero` a **silent no-op**, quietly neutering the
resilience tier the merge was partly for. All three required knowing what the *unconflicted* code
around them expected. This is the argument for `assisted`, not `mechanical`, on merge-shaped work.

**Ticket 4 is now the highest-priority item in P1.** The `golem build` staleness blind spot bit
THREE times in one session: it fabricated an 80/272 failure against a binary that never contained
the code under test, and later reported a correct fix as failing because the fix was never deployed.
Both times the tally looked authoritative. It is documented in four markdown files and still bit
three times — a comment is not a mechanism. Raise ticket 4 above tickets 2, 3, 5 and 6.

**One assertion in the e2e was pinning a security hole**, which is worth watching for elsewhere:
`nested curl errors honestly` asserted that `echo x | xargs curl` slips past the authz gate. main had
fixed exactly that bypass (`xargs` re-enters via `run_string`, never passing back through the gate —
`echo /path | xargs rm` deleted the file at exit 0 while bare `rm` paused). A test can encode a bug
as expected behaviour; when a merge "breaks" a test, check which side is right before restoring it.
