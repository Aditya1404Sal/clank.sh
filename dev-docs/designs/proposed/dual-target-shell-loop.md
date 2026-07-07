---
title: "Dual-target interactive shell loop (wasm p3 + native)"
date: 2026-07-07
author: agent
---

# Dual-target interactive shell loop (wasm p3 + native)

Design for the shell's entrypoint and interactive I/O, built to run on two targets from one
codebase. Written as intent; reflects the as-built state at merge.

## Overview

`crates/clank-shell` is a long-running read/eval/print loop. Command evaluation is a pure,
target-agnostic core; only the stdin/stdout mechanism differs by target:

- **wasm32** — a `wasi:cli/run` component (WASI 0.3 / p3, async stream stdio).
- **native** — an ordinary binary over blocking `std::io`.

The split is by `#[cfg(target_arch = "wasm32")]`, per `README.md` *Compile targets*:
"Conditional compilation handles these seams, backed by a small trait with two
implementations where needed."

## Entrypoint: `wasi:cli/run` (WASI 0.3, async)

The wasm build exports **`wasi:cli/run@0.3.0`** with an **`async fn run() -> Result<(), ()>`**
(`Ok` → exit 0, `Err` → nonzero). p3 async is chosen over p2 sync deliberately: `README.md`'s
concurrency model (multiple in-shell processes progressing at once inside one component)
needs component-model async, which a synchronous `run` cannot express. Mirrors the upstream
wasmtime `p3_cli.rs`.

**Target vs. interface:** compiled with the stable **`wasm32-wasip2` rustc target** but
binding the **p3 (`wasi:cli@0.3.0`) interfaces** via component-model async (the `wasip3`
crate). The `wasm32-wasip2` target emits a component directly. This reconciles the
`wasm32-wasip2` target named throughout `README.md` with the p3 async interface style: the
*target* is p2, the *interfaces* are p3.

**Host:** the `wasmtime` CLI, `wasmtime run -Sp3 -W component-model-async=y <wasm>`. Requires
**Wasmtime ≥ 46** (final `wasi-0.3.0`); earlier versions implement a pre-final p3 draft whose
`wasi:cli/stdin.read-via-stream` differs and reject the component. No bespoke host binary.

## Shared core: pure `eval`

Command dispatch is a **pure function** — no I/O, no async:

```rust
pub fn eval(line: &[u8]) -> (Vec<u8>, Flow)   // (bytes to write, continue/exit)
```

Both drivers call it and write the returned bytes through their own mechanism. `trim_eol`,
`Flow`, and the `PROMPT`/`HELP` constants live alongside it. This is the one place command
resolution grows over time (the process table, `$PATH`, Brush); keeping it pure means it is
shared verbatim and unit-testable on the host with plain `cargo test`.

Placeholder builtins for this first iteration: `exit`, `echo`, `help`, an unknown-command
error, and empty-line-reprompts.

## Two I/O drivers

- **`src/wasm.rs`** (`#[cfg(target_arch = "wasm32")]`): the `wasi:cli/run` guest. stdin is an
  async byte stream (`read_via_stream`, pulled with `.next()`/`read`); stdout is produced by
  writing chunks into a stream whose receiver is handed to `write_via_stream`, driven as a
  concurrent future (`futures::join!`) so the prompt/output flush while the loop awaits stdin.
  Stream chunks are reassembled into `\n`-delimited lines; EOF is the stdin stream ending.
- **`src/native.rs`** (`#[cfg(not)]`): `pub fn run() -> io::Result<()>` — print prompt, flush,
  blocking `BufRead::read_line`, `eval`, write, flush. `read_line == 0` is EOF. No concurrency
  needed because the prompt is flushed before the blocking read.
- **`src/main.rs`**: the `clank` bin entry — native → `native::run()`; wasm → empty `main`
  (the wasm entrypoint is the exported `run`, not `main`). Canonical wasm build is `--lib`.

## Cargo shape

- `[lib] crate-type = ["cdylib", "rlib"]` — `cdylib` is the wasm component; `rlib` lets the
  native bin link the crate.
- `[[bin]] name = "clank"`.
- Wasm-only deps (`wasip3` 0.7, `wit-bindgen` 0.57 async, `futures` 0.3) gated behind
  `[target.'cfg(target_arch = "wasm32")'.dependencies]`; the shared core and native path use
  only `std`.

## Key decisions / rejected alternatives

- **p3 async over p2 sync** — a synchronous entrypoint can't host the concurrent-process
  model the rest of the design needs; adopting it would force a later rewrite of the spine.
- **No unifying `Terminal` trait / generic loop** — wasm I/O is fundamentally async (p3
  streams must be awaited) and native is blocking. A single shared loop would force an async
  runtime onto native or a fake-sync shim over the wasm streams: machinery for no benefit.
  The chosen shape (pure core + two thin drivers) *is* the README's "abstraction with two
  implementations," applied where sharing is real (logic) and split where divergence is real
  (I/O). Revisit if a native async story or the full TUI abstraction actually needs it.
- **Single crate (lib + bin) over a `core`/`wasm`/`native` crate split** — matches the "cfg
  alternate paths" intent with minimal churn; a native `cargo build` emits an unused `cdylib`,
  an accepted triviality. Crate split deferred.
- **`wasmtime` CLI over a custom embedder** — the CLI runs p3 command components directly; a
  native/Golem embedder is deferred and doesn't change the guest contract.

## As-built verification

- `cargo test` → 7 host tests pass (echo / echo-no-args / exit / help / empty / unknown +
  `trim_eol` LF/CRLF/bare).
- Native `target/debug/clank` and the wasm component under Wasmtime 46.0.1 produce
  **byte-identical** output for: `echo hello`+`help`+`exit`; `echo hi` then EOF; unknown
  command. Exit 0 throughout. `wasip3::cli::command::export!` works from inside `src/wasm.rs`.

## Not built (future work)

- Command parsing / Brush behind `eval`; the transcript; the process table / PIDs / job
  control; exit-code fidelity beyond `Ok`/`Err`; native readline / history / full TUI; a
  native/Golem embedder.
