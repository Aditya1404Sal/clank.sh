---
title: "HTTP Client Abstraction Layer"
date: 2026-01-22
author: John A. De Goes
---

# HTTP Client Abstraction Layer

> **Template only — fictional, not project history.** This is a worked example of the dev-docs
> workflow; its `HttpClient`/`wstd`-vs-`reqwest` scenario is invented, not a record of clank's
> actual HTTP transport work.

## Overview

The shell requires outbound HTTP on both `wasm32-wasip2` and native Rust targets. The two targets cannot share a single HTTP client crate. This design specifies a thin abstraction that accommodates both without leaking target-specific concerns into call sites.

## Design

A single trait `HttpClient` is defined in a target-agnostic crate:

```rust
#[async_trait]
pub trait HttpClient: Send + Sync {
    async fn send(&self, req: Request) -> Result<Response, HttpError>;
}
```

Two implementations are provided via conditional compilation:

- `NativeHttpClient` — wraps `reqwest`, enabled on native targets
- `WasiHttpClient` — wraps `wstd`, enabled on `wasm32-wasip2`

All call sites depend on `Arc<dyn HttpClient>`, injected at startup. No call site contains `#[cfg(target_arch)]` directives.

## Error Handling

`HttpError` covers: connection failure, timeout, non-2xx response (with status code and body), and TLS error. Maps cleanly to shell exit code `4` (remote call failed).

## Decisions

- Streaming responses deferred to v2. Buffered responses are sufficient for v1 model API calls and MCP tool invocations.
- Timeout is configured globally via `~/.config/ask/ask.toml`, with per-call override available via `--timeout`.

## Alternatives Considered

- **Direct `reqwest` usage everywhere with WASM stubs:** Rejected. Stubs that panic at runtime are worse than a compile-time boundary.
- **Single crate with cfg flags at call sites:** Rejected. Scatters target awareness throughout the codebase.
