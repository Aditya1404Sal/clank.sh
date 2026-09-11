---
title: "The workspace has accumulated defects that make it hard to trust and hard to change"
date: 2026-09-11
author: agent
---

# The workspace has accumulated defects that make it hard to trust and hard to change

## Problem

clank works. The e2e suite is green (272/0 on `main-rc-1` as of 2026-09-11), the architecture is
sound — `clank-core` is a clean DAG, the workspace has exactly two Cargo features and both are
additive, and the lint policy is stricter than the Golem monorepo's. This issue is not about a
codebase in disarray.

It is about a specific, measured set of defects that share one property: **each of them lets
something be wrong without anyone finding out.**

### 1. Verification does not run

`.github/workflows/conformance.yml` is the only workflow. It runs the conformance corpus (28 unit
+ 34 native scenarios) and a hygiene job. It does **not** run `cargo test -p clank-core`, or
`cargo test --workspace`, on any trigger. That is **554 `#[test]` functions** — 466 of them in
`clank-core` — with no automated gate. A pull request can merge with `clank-core` entirely red and
CI reports green.

`scripts/golem-e2e.sh` (1,552 lines, 357 assertion call sites) is called by **no CI job at all**.
The ~150 assertions it uniquely owns — `ask`, `grease`, `mcp`, the `golem` cluster surface, the
greeter-agent wRPC round-trip, none of which have any conformance-corpus equivalent — execute only
when a human types the command.

### 2. The branch is behind, and the gap contains bug fixes

`main-rc-1` and `main` diverged at `8dfbf19`: 63 commits on this side, 46 on the other, 84 shared
source files. The gap is not cosmetic. `main` carries `f948a3c` (four "reported success for work
that did not succeed" bugs), `8808547` (four panics that permanently wedge a durable agent:
UTF-8 byte-slice truncation, wRPC argument arity, `--schedule` epoch overflow), a resilience
conformance tier that caught a real bug on its first run, and `8c290a9` (the split of the
5,511-line `session/tests.rs` into 11 concern modules). Whether those bugs are also live here is
untested, because the regression tests for them do not exist on this branch.

### 3. A crate built to be embedded has a defect embedders cannot fix

`clank-embed` exists so other Golem agents can embed the shell. Four of its five injected seams
degrade to an honest "not configured" error when unfilled. `LogSink` does not: it is a
non-optional `Arc<dyn LogSink>` defaulted to `DefaultLogSink` in both arms of `Session::new`, and
on wasm that default **appends**, which duplicates `/var/log` lines under oplog replay — the exact
hazard `clank-embed/src/log_sink.rs` was written to prevent.

The correct sink, `DurableLogSink`, is `pub(crate)`. There is therefore no public way to combine
it with a hand-picked provider mix. `EmbeddedShell::with_setup` — the crate's advertised extension
point — documents precisely that combination in an example that **cannot compile for any external
consumer**; it survived because the fence is `ignore`d rather than doctested. An embedder
following the documentation gets replay-corrupted logs, cannot fix it, and is never told.

`clank-embed` is also the only one of ten workspace members with no `[lints]` section, so it runs
without `unreachable_pub`, `missing_docs`, `unwrap_used`, `expect_used` or `clippy::pedantic` —
while CI still reports it as covered.

### 4. One piece of logic is reimplemented five times and has already drifted

Decoding the Golem CLI's `agent invoke --format json` output exists in five places: the
conformance backend (Rust, dual-shape, unit-tested), `golem-e2e.sh`, `golem-probe.sh`,
`clank-repl.sh`, and `golem-native-testing/probe.sh`. Only the Rust one is robust.

Both failure modes are silent: the wrong key matches nothing, the wrong shape finds no path. A
healthy agent then reads back as completely dead. This already caused a documented
"4 passed, 288 failed" false catastrophe when the dev SDK landed. `golem-probe.sh` was found
carrying the same bug on 2026-09-11 and fixed; **`clank-repl.sh` still has it** and is
non-functional on this branch, with nothing to catch it.

### 5. The fork story is undocumented, miscounted, and carries a live panic

`docs/WASM_CHANGES.md` — the document whose job is fork tracking — asserts "exactly two
third-party source forks… no other crate is pinned to a git rev, verified." On this branch there
are **three fork repos resolving to 29 patched crates** plus two vendored patches. The third
(`golemcloud/wit-bindgen` ×4) is invisible from every `Cargo.toml`; it arrives transitively
through a `golem-rust` path dependency that is **an absolute, machine-local filesystem path
hardcoded in three separate manifests**, and it is **branch-pinned, not rev-pinned** — the exact
non-reproducibility the audit's P2-7 finding fixed for the other two forks.

Separately, the coreutils fork's `printf` panics on any `core::fmt` width above 65,535 (Rust
≥1.88 stores widths as `u16`). On a durable agent that panic traps and permanently wedges the
instance. It is reachable from any user script. Only the e2e's call site was worked around; the
fork is unpatched.

### 6. Structure: one crate is two, and a few things are in the wrong place

The wasm platform layer was correctly extracted into `clank-embed`. The native platform layer was
not — `native.rs`, `ai/anthropic_native.rs`, `mcp/http_native.rs` and `golem/rest_native.rs` were
folded back into `clank-core`, so the crate boundary is asymmetric between targets for no
principled reason and understanding one seam means reading three crates.

Smaller instances of the same shape: `grease::pkg` is 1,186 self-contained lines that force
`grease-tool` to depend on all ~38 of `clank-core`'s dependencies to use them; `whttp` was created
to remove HTTP duplication and there are now five independent HTTP client call sites, only two of
which use it; `--help` is implemented three separate times; `ai` and `mcp` import each other's
concrete types; `eval_line_inner` is 379 lines and `run_command` is 114.

### 7. Documentation duplicates, contradicts, and points at files that never existed

`README.md` (1,065 lines) and `docs/USAGE.md` (901) independently document the same command
surface, hand-synced on every behaviour change. `AGENTS.md` links two design documents that are
absent from the tree **and from all of git history**. `DEV_SDK_CHANGES.md` links its self-declared
"sibling" `WASM_CHANGES.md` at a path where no file exists. All three `docs/audit/*.md` still
address `clank-shell`, renamed on 2026-07-23. A real design document still presents
`wstd::block_on` as golem-rust's executor — falsified on 2026-09-11 — and is marked "RESOLVED".

The `dev-docs` workflow this file follows is itself not being followed: all three real feature
triads sit in `open/`/`proposed/` while containing "As-built verification" and "Acceptance tests
(all passing)" sections. Only the fictional template has ever completed a promotion.

## Impact

The verification gap (1) and the stale-artifact hazard mean a change can appear tested when it was
never compiled — this happened during the 2026-09-11 `wasi-fetch` migration and produced a
confident, entirely fictional 80/272 result against a binary predating every edit. The embedder
defect (3) silently corrupts the durable logs of exactly the third-party agents `clank-embed`
exists to serve. The fork gaps (5) mean the build is not reproducible off one developer's machine
and a user script can permanently wedge a production agent.

None of these is visible from reading the code, which is why they have persisted.

## Evidence

Measured on 2026-09-11 across ten parallel read-only audits (crate topology, `clank-core`
structure, seam architecture, fork inventory, execution path, documentation, comment quality, test
architecture, public API surface, and the Golem monorepo's conventions as a comparison target).
Figures cited above are counted, not estimated.

Two hypotheses were tested and **not** supported, and are recorded here so they are not re-raised:

- *"Excessive comments."* Measured 22.3% comment-to-code, with 96.4% of a size-weighted sample
  classified as load-bearing rationale or appropriate rustdoc, two instances of narration
  codebase-wide, and zero TODO/FIXME/XXX/HACK markers in 37,335 lines. Total safely trimmable:
  roughly 20 lines. The module-level `//!` headers are also currently the **only** architecture
  documentation that exists for Wall C, replay-safety and the wasip2 constraints.
- *"Little meaningful connection between crates."* `clank-core` is a clean DAG; only `session/`
  reaches into other concerns and nothing reaches back. Most apparent back-references are rustdoc
  cross-links, not `use` statements.

## Out of scope

- The execution-offload architecture (replacing the compile-everything-into-wasm model with tool
  components or a host-native WIT interface). That is a separate issue; it depends on the agent
  tools epic and on facts about `golem:tool` not yet established.
- The agent tools integration itself (`dev-docs/issues/open/agent-tools-integration.md`).
- Any behaviour change to the shell's command surface.
