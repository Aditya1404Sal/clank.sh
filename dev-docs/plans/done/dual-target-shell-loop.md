---
title: "Dual-target interactive shell loop (wasm p3 + native)"
date: 2026-07-07
author: agent
issue: "dev-docs/issues/open/dual-target-shell-loop.md"
design: "dev-docs/designs/proposed/dual-target-shell-loop.md"
---

# Dual-target interactive shell loop (wasm p3 + native)

The seed PR: the first runnable clank.sh — a long-running interactive shell loop that runs
both as a WASI p3 component under wasmtime and as a native binary, from one codebase.

Issue: `dev-docs/issues/open/dual-target-shell-loop.md`.
Design: `dev-docs/designs/proposed/dual-target-shell-loop.md`.

## Developer feedback (decisions)

- **WASI model:** p3 async (`wasi:cli/run@0.3`, stream stdio), over p2 sync.
- **Compile target:** p3 interfaces on the stable `wasm32-wasip2` rustc target (confirmed to
  compile and run).
- **Structure:** shared pure `eval` + two `#[cfg]` I/O loops (not a unifying I/O trait).
- **Native scope:** basic `std::io` parity (same prompt/builtins as wasm); no readline/TUI yet.

## Research consulted

- wasmtime `p3_cli.rs` — async `wasi:cli/run` with stream stdio.
- `wasip3` `0.7.0+wasi-0.3.0` — guest bindings for `wasi:cli@0.3.0`, `cli::command::export!`,
  stream stdio; depends on `wit-bindgen ^0.57.1` (async). Runs on Wasmtime ≥ 46.

## Tasks

- [x] Workspace scaffolding: `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `.gitignore`
- [x] `crates/clank-shell`: `cdylib`+`rlib` lib, `[[bin]] clank`, wasm-only deps target-gated
- [x] Shared pure core in `src/lib.rs`: `eval(&[u8]) -> (Vec<u8>, Flow)`, `trim_eol`, consts
- [x] `src/wasm.rs` — async `wasi:cli/run` guest + stream loop, calls pure `eval`
- [x] `src/native.rs` — blocking `std::io` loop (`pub fn run`)
- [x] `src/main.rs` — bin entry (native → `native::run`; wasm → empty `main`)
- [x] Host unit tests for `eval`/`trim_eol`
- [x] AGENTS.md Build & Test for both targets

## Acceptance tests (all passing)

- **Host:** `cargo test` → 7 passed.
- **Native:** `cargo build --bin clank` → `target/debug/clank`.
  - `printf 'echo hello\nhelp\nexit\n' | cargo run -q --bin clank` → `hello` + help; exit 0.
  - `printf 'echo hi\n' | …` → `hi`; clean EOF exit.
  - `printf 'nosuchcmd\nexit\n' | …` → `command not found: nosuchcmd`.
- **Wasm:** `cargo build --release --target wasm32-wasip2 --lib`; the three tests above under
  `wasmtime run -Sp3 -W component-model-async=y` (Wasmtime 46.0.1) produce byte-identical output.

## Deviations

- Host requirement is **Wasmtime ≥ 46** (final `wasi-0.3.0`), not the "≥ 43" first assumed —
  Wasmtime 45 fails with a `wasi:cli/stdin.read-via-stream` type mismatch.
- `wasip3::cli::command::export!` works from inside `src/wasm.rs` (no crate-root hoist needed).
