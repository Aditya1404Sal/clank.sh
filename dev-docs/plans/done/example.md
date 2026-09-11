---
title: "Implement HTTP Client Abstraction Layer"
date: 2026-01-22
completed: 2026-01-28
author: agent
issue: "dev-docs/issues/closed/example.md"
research:
  - "dev-docs/research/example.md"
designs:
  - "dev-docs/designs/approved/example.md"
realized_design: "dev-docs/designs/approved/example.md"
---

# Implement HTTP Client Abstraction Layer

> **Template only — fictional, not project history.** This is a worked example of the dev-docs
> workflow; its `HttpClient`/`wstd`-vs-`reqwest` scenario is invented, not a record of clank's
> actual HTTP transport work.

## Originating Issue

Silent HTTP failures on `wasm32-wasip2` — see `dev-docs/issues/closed/example.md`.

## Research Consulted

- `dev-docs/research/example.md` — surveyed available WASM HTTP client libraries; concluded `wstd` is the correct choice for the WASM target.

## Design Referenced

- `dev-docs/designs/approved/example.md` — HTTP Client Abstraction Layer.

## Developer Feedback

Consulted on two open questions from the design doc:

- **Streaming vs buffered:** Defer streaming to v2. Buffered is sufficient for v1.
- **Timeout configuration:** Global config in `ask.toml` with per-call `--timeout` override.

## Approach

Introduce an `HttpClient` trait in a new `clank-http` crate. Provide `NativeHttpClient` (reqwest) and `WasiHttpClient` (wstd) behind `#[cfg]` guards. Inject `Arc<dyn HttpClient>` at startup. Add error logging to `/var/log/http.log` at the trait boundary so failures are never silent.

## Tasks

- [x] Create `clank-http` crate with `HttpClient` trait and `HttpError` type
- [x] Implement `NativeHttpClient` wrapping `reqwest`
- [x] Implement `WasiHttpClient` wrapping `wstd`
- [x] Wire `Arc<dyn HttpClient>` injection at shell startup
- [x] Add structured logging to `/var/log/http.log` at the trait boundary
- [x] Update `ask` to use injected client rather than direct crate calls

## Acceptance Tests

- [x] `ask "hello"` succeeds on native target
- [x] `ask "hello"` succeeds on `wasm32-wasip2` target
- [x] A failed HTTP call produces a non-zero exit code (`4`) and an entry in `/var/log/http.log`
- [x] No call site outside `clank-http` contains `#[cfg(target_arch)]`

## Deviations

None. Implementation matched the approved plan exactly.
