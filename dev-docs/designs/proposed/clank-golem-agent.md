---
title: "clank.sh as a Golem agent instance"
date: 2026-07-08
author: agent
---

# clank.sh as a Golem agent instance

Design for wrapping clank's existing shell `Session` in a Golem agent so it deploys to the Golem
executor as a durable instance. Written as intent; the empirical verdict on the executor conflict
(below) is reconciled at merge.

## Overview

A new crate `crates/clank-agent` exports the `golem:agent` world via the `golem-rust` SDK. It holds
the existing `clank_shell::session::Session` (Brush `Shell` + `Transcript`) as **durable agent
state** and exposes one method:

```rust
async fn run_line(&mut self, cmd: String) -> String   // one command = one invocation
```

The `Session` core is reused verbatim; only the *driver* changes — from a stdin REPL
(`wasm.rs`) to an invocable agent method. The existing `wasi:cli/run` REPL driver and the native
driver stay as-is.

## Why a new crate (not converting clank-shell)

The Golem agent world and clank's `wasi:cli/run` world cannot both be exported from one wasm
artifact. `clank-shell` keeps producing the REPL component; `clank-agent` produces the agent
component and depends on `clank-shell` (`default-features = false`) for the `Session` core only.
This requires feature-gating clank-shell's `mod wasm` (which does
`wasip3::cli::command::export!(Component)`) behind a `repl-driver` feature, so the agent build links
the core as a plain rlib with **no `wasi:cli/run` export** to clash with the agent world.

## The executor / `block_on` conflict (the crux) — RESOLVED

Brush hard-requires a **tokio** runtime in TLS context (bare `tokio::spawn` and `spawn_blocking`
inside brush-core). clank's wasm `Session` drives Brush with a tokio current-thread
`rt.block_on(...)`. But the `golem-rust` SDK has **zero tokio dependency** and drives every agent
invocation with `wstd::runtime::block_on` (`sdks/rust/golem-rust/src/agentic/agent_registry.rs:147`);
its "never use block_on" guidance means the SDK owns the outermost block_on and methods should be
plain `async`/`.await`.

So clank's tokio `block_on` becomes **nested inside** the SDK's wstd `block_on` — two single-threaded
executors nested. For a **simple command** (echo/cat/our builtins) Brush's future is pure-CPU and
never awaits a wasip2 pollable, so nesting completes; pipelines/subshells hit
`tokio::spawn`/`spawn_blocking` (no threads on the agent) and stay out of scope.

**Verdict (verified on a local `golem server`):** the nested block_on **works**. `golem agent invoke
'ClankAgent("demo")' run_line '"echo hi"'` returns `"hi\n"`. We keep the tokio `block_on` Session
**verbatim** — no driver pivot. Deviating from the SDK's "no block_on" guideline is safe here because
a current-thread runtime driving a pure-CPU future that completes synchronously never yields to (or
starves) the outer wstd scheduler.

## Durability model

Golem durability is **oplog-based, not serde-based** by default: the live agent struct (holding the
non-serde `Session`) stays in worker memory across invocations, so no serialization is needed on the
happy path — the transcript and Brush shell state persist for free. On recovery Golem replays the
oplog (re-runs `new` + past `run_line`s); deterministic commands reconstruct correctly.
Non-deterministic commands (clock/random) and snapshotting (oplog compaction, which *would* need
serde) are out of scope for this milestone.

The agent constructor `new(name)` makes `name` the agent identity, so per-identity instances are
isolated (each gets its own `Session`/transcript/filesystem).

## Cargo / manifest shape

- `crates/clank-agent`: `edition = "2024"`, `[lib] crate-type = ["cdylib"]`, deps `golem-rust`
  (path to the vendored SDK, `export_golem_agentic` feature), `wstd`, `serde`, and `clank-shell`
  (`default-features = false`).
- Root `golem.yaml` (`manifestVersion 1.5.0`): component `clank:agent`, `template: rust`,
  `dir: crates/clank-agent`, local environment. The exact `templates.rust` block is captured from a
  real `golem new` scaffold, not hand-written.
- Workspace root: add the member; host any `[profile.release]` (member profiles are ignored); the
  `[patch.crates-io] uucore` fork is inherited transitively.

## Filesystem: what the Golem model actually gives us (verified)

- **Golem mounts `/` read-only** by default (`ls -la /` → `dr-xr-xr-x`, owner `somebody:somegroup`);
  `/tmp` does not exist. Writable storage is provisioned per path via `golem.yaml`
  `files: [{ sourcePath, targetPath, permissions: read-write }]` (`sourcePath` is relative to
  `golem.yaml`). A provisioned file reads back correctly through coreutils `cat`.
- **coreutils builtins that read work** (after the uucore fix below). Reading a provisioned file via
  `cat` returns its contents.
- **Brush file redirects (`>`, `<`, `>>`) do NOT work on wasip2.** `echo x > file` fails with
  `I/O write error: failed to duplicate open file`. Root cause is Brush-internal, not Golem: a
  redirect stores an `OpenFile::File(std::fs::File)` and Brush **clones** it during fd setup
  (`brush-core openfiles.rs:100/122-127`), calling `File::try_clone()`, which fails on wasip2 and
  falls back to a discarding sink. This is the same class of limitation as pipes/subshells. So
  file *writing through the shell* is deferred until Brush's openfiles clone is patched for wasm
  (e.g. reopen-by-path instead of `File::try_clone`).

## uucore empty-argv panic (fixed in the fork)

Any uucore builtin (`mkdir`, `ls`, …) initially **trapped the whole Golem worker** (a
`DeterministicTrap` → `abort`, poisoning the agent). Cause: uucore caches `ARGV =
std::env::args_os()`, which is **empty** under a Golem agent invocation (the host supplies no CLI
argv), then `UTIL_NAME`/`EXECUTION_PHRASE` index `ARGV[0..2]` → panic. Fixed in
`Aditya1404Sal/coreutils@wasip2-oscompat` (commit `35f35ed`): seed a fallback program name when argv
is empty on wasi, and clamp `UTIL_NAME`'s indices. `Cargo.lock` bumped to the new fork HEAD.

## Verification (as-built)

`golem build` → `golem server run` → `golem deploy` → `golem agent invoke`:
- `run_line '"echo hi"'` → `"hi\n"`.
- Durability: `Y=hi` then (separate invocation) `echo $Y` → `hi`; `context show` shows all prior
  commands from earlier invocations on the same identity.
- Isolation: a different identity (`ClankAgent("other")`) has an empty transcript.
- coreutils read: `cat` of a provisioned file returns its contents (uucore fix confirmed).

## Not built (future work)

Shell-driven file **writing** (needs a Brush openfiles wasm patch for `>`/`<`/`>>`); a clean
writable working directory (Golem `/` is read-only; the coreutils `close(1)` capture also leaves a
`.clank-uu-capture` file to clean up); HTTP mount/endpoints; snapshotting; pipelines/subshells on the
agent; deterministic-recovery wrappers for clock/random; MCP; Golem cloud deploy.
