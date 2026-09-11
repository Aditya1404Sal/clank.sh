---
title: "Implement HTTP Client Abstraction Layer"
date: 2026-01-20
author: agent
issue: "dev-docs/issues/open/example.md"
research:
  - "dev-docs/research/example.md"
designs:
  - "dev-docs/designs/proposed/example.md"
---

# Implement HTTP Client Abstraction Layer

> **Template only — fictional, not project history.** This is a worked example of the dev-docs
> workflow; its `HttpClient`/`wstd`-vs-`reqwest` scenario is invented, not a record of clank's
> actual HTTP transport work.

## Originating Issue

Silent HTTP failures on `wasm32-wasip2` — see `dev-docs/issues/open/example.md`.

## Research Consulted

- `dev-docs/research/example.md` — surveyed available WASM HTTP client libraries; concluded `wstd` is the correct choice for the WASM target.

## Design Referenced

- `dev-docs/designs/proposed/example.md` — HTTP Client Abstraction Layer. **Note:** this design is pending human approval.

## Developer Feedback

Consulted on two open questions from the design doc:

- **Streaming vs buffered:** Defer streaming to v2. Buffered is sufficient for v1.
- **Timeout configuration:** Global config in `ask.toml` with per-call `--timeout` override.

## Approach

Introduce an `HttpClient` trait in a new `clank-http` crate. Provide `NativeHttpClient` (reqwest) and `WasiHttpClient` (wstd) behind `#[cfg]` guards. Inject `Arc<dyn HttpClient>` at startup. Add error logging to `/var/log/http.log` at the trait boundary so failures are never silent.

## Tasks

- [ ] Create `clank-http` crate with `HttpClient` trait and `HttpError` type
- [ ] Implement `NativeHttpClient` wrapping `reqwest`
- [ ] Implement `WasiHttpClient` wrapping `wstd`
- [ ] Wire `Arc<dyn HttpClient>` injection at shell startup
- [ ] Add structured logging to `/var/log/http.log` at the trait boundary
- [ ] Update `ask` to use injected client rather than direct crate calls

## Acceptance Tests

- `ask "hello"` succeeds on native target
- `ask "hello"` succeeds on `wasm32-wasip2` target
- A failed HTTP call produces a non-zero exit code (`4`) and an entry in `/var/log/http.log`
- No call site outside `clank-http` contains `#[cfg(target_arch)]`
