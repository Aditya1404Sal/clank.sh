---
title: "Workspace cohesion: close the verification gaps, finish the crate split, consolidate the docs"
date: 2026-09-11
author: agent
---

# Workspace cohesion

Design for closing the defects in `dev-docs/issues/closed/workspace-cohesion.md`. Written as intent
after a ten-agent audit of the workspace on 2026-09-11 and a brainstorm with Aditya the same day;
the decisions below were made or confirmed in that session. Facts about clank refer to
`main-rc-1` at `de5f908`; facts about the Golem monorepo refer to `clank-connect-patch` rebased
onto upstream `f5a3d29b9`. The implementation plan
(`dev-docs/plans/done/workspace-cohesion.md`) is the single source of truth for sequencing and
acceptance; this document records intent and decisions.

## Overview

Five phases, ordered so each rests on a verified base:

```
P0  Merge main            inherit 8 bug fixes + the session/tests split + the resilience tier
     │                     (must precede any restructuring: main deleted the file rc-1 edited)
     ▼
P1  Close the gaps        CI actually runs the tests · the embedder defect · one decoder ·
     │                     the stale-artifact guard · reproducible dependencies
     ▼
P2  Finish the crate      extract clank-native · extract grease-pkg · consolidate HTTP ·
     │  split              decompose the two god-functions · delete dead code
     ▼
P3  Consolidate docs      one command reference · architecture out of module headers ·
     │                     a generated fork inventory with a drift gate
     ▼
P4  Error taxonomy        Result<String, String> → typed errors at the seams   (separable)
```

P4 is deliberately last and explicitly separable: it is the largest change and the only one that
touches every seam signature. If it is deferred, P0–P3 still stand alone as a complete piece of
work.

## Guiding principle

Every defect in the issue shares one property: it lets something be wrong silently. So the bar for
"done" throughout is not "the code is nicer" but **"this class of error now announces itself."**
Where a choice exists between a convention and a mechanism, the mechanism wins — a comment that
says "keep these in sync" is what already failed for the fork inventory, the `AGENTS.md` links,
and the `WASM_CHANGES.md` fork count.

## Decisions

### D1 — Merge `main` before any restructuring

`main` is 46 commits ahead at the merge-base `8dfbf19`, with 84 shared source files. The decisive
constraint: `8c290a9` on `main` **deleted** `crates/clank-core/src/session/tests.rs` (splitting it
into eleven modules under `session/tests/`), while `main-rc-1` has changed that same file by 1,675
lines. That is a delete/modify conflict whose resolution is to redistribute this branch's test
changes across `main`'s eleven modules. Restructuring first would compound it; every later phase
is easier once it is done.

The merge also brings four "reported success for work that did not succeed" fixes (`f948a3c`),
four durable-agent-wedging panic fixes (`8808547`), and a resilience conformance tier — none of
which have regression coverage on this branch.

*Rejected: cherry-picking only the bug fixes.* It leaves the divergence to grow and does not
deliver the test split, which is the thing that makes `session/`'s tests navigable.

*Rejected: rebasing `main-rc-1` onto `main`.* 63 commits replayed across 84 conflicting files, in
a branch that is already pushed. A merge commit is honest about what happened.

### D2 — CI runs the tests that exist

Add `cargo test --workspace -- --test-threads=1` to the push-triggered CI. The single-thread flag
is not optional: Brush resets `SIGPIPE` to `SIG_DFL` for POSIX pipe semantics, and under libtest's
default thread pool one test's broken-pipe write can kill the whole process with signal 13.

`scripts/golem-e2e.sh` moves to the same nightly/`workflow_dispatch` trigger the golem conformance
tier already uses, rather than to every push — it needs a server, a deploy and tens of minutes.
That is a deliberate compromise: nightly is not as good as per-push, but the current state is
*never*, and the ~150 assertions it uniquely owns (`ask`, `grease`, `mcp`, cluster, greeter wRPC)
have no other home.

*Rejected: migrating the e2e's assertions into the conformance corpus wholesale.* About two-thirds
of the corpus already mirrors some slice of the e2e, and the overlap is not waste — the corpus's
purpose is cross-target diffing (the same assertion on native and golem, where a *divergence* is
the signal). But the e2e's live-network, stateful and cross-deploy assertions do not fit the
corpus's fresh-instance-per-scenario, offline-by-default model without significant harness work.
Migrating the deterministic offline subset is worthwhile and is scheduled, but it is not a
prerequisite for running what exists.

### D3 — The log sink defaults to the correct implementation, and is nameable

Three changes, together:

1. **Every `EmbeddedShell` constructor installs `DurableLogSink`** — `new()`, `with_setup()` and
   `with_default_golem_providers()`. `clank-embed` targets Golem agents; the replay-safe sink is
   always the right default there, and the append sink never is.
2. **`DurableLogSink` becomes `pub`**, so an embedder combining it with a custom provider mix can
   name it.
3. **The `with_setup` doc example becomes a real doctest** rather than an `ignore`d fence, so an
   example that cannot compile fails the build.

`with_durable_log_sink()` is kept as a deprecated alias for one release rather than removed, since
`greeter-agent` and any external embedder call it.

*Rejected: making `log_sink` an `Option` to match the other four seams.* It looks like consistency
but fixes nothing — the defect is that the default is wrong on wasm, not that a default exists,
and `None` would mean losing `/var/log` entirely rather than corrupting it.

*Rejected: a runtime warning when the append sink is used on wasm.* A warning in the durable log
about the durable log being wrong is not a mechanism anyone will see.

### D4 — One invoke-JSON decoder for the shell scripts, sharing the Rust one's dual-shape contract

`scripts/lib/golem-json.sh` gains `eval_json()` and `run_line()`, handling **both** wire shapes
(released `result_json` with named fields, dev-SDK `resultJson` with a positional
schema-value-tree) exactly as `clank-conformance`'s `decode_invoke` does. `golem-e2e.sh`,
`golem-probe.sh` and `clank-repl.sh` source it and delete their copies.

This is worth stating plainly because it has already cost real time twice: both mismatch modes
fail *silently* — the wrong key greps nothing, the wrong shape finds no path — so a completely
healthy agent reads back as blank. That produced a documented "4 passed, 288 failed" false
catastrophe once, cost a debugging detour on 2026-09-11 when `golem-probe.sh` was found carrying
it, and `clank-repl.sh` is broken by it right now.

`golem-native-testing/probe.sh` keeps its independent python approach — it reads the top-level
`"result"` Debug string rather than the structured record, which is a different mechanism that
does not share the failure mode.

*Rejected: a small Rust helper binary the scripts shell out to.* Correct, and it would give one
implementation across all five sites, but it puts a `cargo build` in the path of every script
invocation including the debugging ones, which is exactly when you want zero ceremony.

### D5 — Freshness is checked, not remembered

`scripts/lib/golem-json.sh`'s companion `assert_fresh_artifact()` compares the built component's
mtime against the newest source mtime under `crates/` and `utilities/`. When the artifact is
stale it runs the targeted remedy — `cargo clean -p clank-core -p clank-agent --target
wasm32-wasip2` — and says so, rather than proceeding.

`golem build` has a documented up-to-date blind spot: it tracks the `clank-agent` component
directory, not its path dependencies, so editing `clank-core` can leave the deployed wasm stale.
This is written down in four markdown files and was still rediscovered the hard way on 2026-09-11,
producing a confident 80/272 result against a binary that predated every edit under test. Four
documents did not prevent it; a check will.

### D6 — Dependencies are declared once, and pinned reproducibly

All shared dependencies move to `[workspace.dependencies]`, and every member references them as
`{ workspace = true }` — the single most mechanical convention in the Golem monorepo, where all
326 entries including internal crates follow it without exception. This is also the fix for the
`golem-rust` path being an absolute, machine-local string hardcoded in three manifests: one entry,
one place to change.

`golemcloud/wit-bindgen` moves from a branch pin to a rev pin, matching what the audit's P2-7
finding already established for the other two forks and what `deny.toml` exists to enforce.

`clank-embed` gains the `[lints] workspace = true` stanza every other member has.

### D7 — `clank-core` becomes genuinely target-agnostic

Extract `crates/clank-native`, mirroring `clank-embed`: `native.rs` (the reedline TUI and provider
wiring), `ai/anthropic_native.rs`, `mcp/http_native.rs`, `golem/rest_native.rs`,
`golem/config_native.rs`. `clank-cli` then depends on `clank-native`; `clank-core` keeps only the
engine and the seam definitions, which is what its module doc already claims it is.

The result is symmetric — one platform crate per target, each implementing the same five seams —
so understanding a seam means reading the trait plus one implementation, not three crates.

Two smaller extractions of the same shape: **`grease-pkg`** (1,186 self-contained lines plus the
`manifest` slice it needs), which cuts `grease-tool` from ~38 transitive dependencies to about
six; and the five duplicated HTTP client call sites, whose shared parts — client construction and
the identical Content-Length-then-body-cap guard — move into `whttp`, the crate created to hold
exactly that.

*Rejected: splitting `Session` into sub-structs.* Tempting at 21 fields and 99 methods, but the
coupling is real rather than sloppy: `grease` is read from all five session submodules and `mcp`
from five of six files, because a grease package can *be* an MCP server, a prompt or a Golem
agent. A naive split relocates the coupling into `Arc<Mutex<_>>` handles and makes it worse. The
two genuinely separable clusters (`ask_provider`+`repl`+`next_ask_stdin`,
`agent_invoker`+`golem_cluster`+`pending_invocations`) are noted but not scheduled — the
god-*functions* are the acute problem, not the god-object.

### D8 — Comments are not trimmed; architecture is promoted out of them

The audit measured 22.3% comment-to-code, 96.4% of a size-weighted sample load-bearing, two
instances of narration codebase-wide, and zero debt markers in 37,335 lines. Total safely
trimmable is roughly twenty lines. A trimming pass is therefore not scheduled.

The finding that *is* actionable: the module-level `//!` headers are currently the only
architecture documentation that exists. "Wall C" is threaded by name across eight files, each
carrying a partial explanation. So `docs/architecture/` gains one canonical page each for Wall C,
replay-safety, the wasip2 constraint table, and the resolution surface; the module headers keep a
one-line statement and a pointer. Files get shorter, the explanation becomes single-sourced, and
nothing is lost.

The comparison that motivated the original request is worth recording: clank runs ~18.3% comments
per non-blank line against Golem's ~6.4%, but the gap is composition, not padding — Golem's
563,000 lines include vast tracts of layered CRUD and generated clients that need no comment,
while clank is almost entirely code fighting a platform with no processes, pipes, threads or
`dup2`. clank's own driest files (`awk.rs` 4.4%, `find.rs` 8.3%) sit at Golem's median precisely
because POSIX-specified behaviour explains itself.

### D9 — Documentation is single-sourced, and drift is detected

`README.md` keeps its first ~310 lines (pitch, philosophy, glossary, architecture) and hands the
command surface to `docs/USAGE.md`, ending the largest duplication in the repo — two ~900-line
surfaces hand-synced on every behaviour change.

The fork inventory becomes **generated** from `Cargo.toml` and `Cargo.lock` into
`docs/FORKS.md`, with a `check-forks` CI step that regenerates and fails on `git diff
--exit-code`. This is the Golem monorepo's own pattern (`check-openapi`, `check-docs-skills`,
`check-wit`) and it is the direct answer to a hand-written fork document drifting into asserting
"exactly two forks" when there are three.

Dead links are fixed, the `docs/audit/*` trilogy gets a banner noting it predates the
`clank-shell`→`clank-core` rename rather than being rewritten, and the three stalled `dev-docs`
triads are promoted to `closed/`/`approved/`/`done/` — with a dated addendum on
`clank-golem-agent`'s design noting its `wstd::block_on` premise was superseded, written *before*
promotion so the frozen record does not carry a false claim forward.

### D10 — Error taxonomy follows Golem's shape, at the seams only

The five seams currently return `Result<String, String>`. P4 introduces `thiserror` domain enums
at those boundaries plus a `SafeDisplay`-equivalent for what reaches a user, following the Golem
monorepo's layering (`anyhow` for propagation in 419 files, `thiserror` for taxonomies in 82,
joined by `SafeDisplay`/`IsRetriableError`/`ApiErrorDetails`).

Scope is deliberately the seams, not the whole crate. Converting every internal `Result<_, String>`
in `clank-core` is a much larger change with a much worse risk-to-value ratio, and nothing in the
issue points at it.

### D11 — What this design deliberately does not do

- **Match Golem's lint posture.** clank is already stricter: pedantic, `unwrap_used`,
  `expect_used`, `missing_docs`, `unsafe_code`, `cargo-audit` and `cargo-deny`. Golem has no
  `[workspace.lints]`, no `deny.toml` and no `#![deny]` anywhere. The only lint action is closing
  the `clank-embed` adoption gap.
- **Split large files for being large.** Golem's median file is 258 lines with a p90 of 1,384 and
  a maximum of 10,935; its CLI carries four files over 2,700. `session/mod.rs` at 2,636 would be
  unremarkable there. The case for decomposing `eval_line_inner` (379 lines) and `run_command`
  (114) is that they are unreadable classification ladders, not that the file is big.
- **Change any shell behaviour.** Every phase is behaviour-preserving, verified by the existing
  suites. The one intentional exception is D3, which changes a default that is currently wrong.

## Verification

Each phase closes on the full gate: `cargo fmt --check`, clippy `-D warnings` on both targets,
`cargo test --workspace -- --test-threads=1`, the conformance corpus on both tiers, and
`scripts/golem-e2e.sh`. Structural phases (P2) additionally require the e2e assertion count to be
unchanged — a restructure that quietly drops coverage is the failure mode to guard against.

Because P0 changes the test file layout and P2 moves code between crates, the gate is run per
ticket rather than per phase.

## Open questions

- **Does the `main` merge alter any behaviour this branch depends on?** `main` and `main-rc-1`
  both touched 84 source files; the merge is expected to be behaviour-preserving for rc-1's SDK
  work, but `native.rs` changed by ~550 lines on each side and is the most likely place for a
  genuine semantic conflict.
- **Should the deterministic offline subset of `golem-e2e.sh` migrate into the conformance
  corpus?** Deferred pending the cross-target value being demonstrated on a first batch.
