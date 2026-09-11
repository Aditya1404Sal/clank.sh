---
title: "Survey of WebAssembly HTTP Client Libraries"
date: 2026-01-15
author: agent
---

# Survey of WebAssembly HTTP Client Libraries

> **Template only — fictional, not project history.** This is a worked example of the dev-docs
> workflow; its `HttpClient`/`wstd`-vs-`reqwest` scenario is invented, not a record of clank's
> actual HTTP transport work.

## Motivation

The shell requires outbound HTTP for model provider calls and MCP server communication. The native target can use `reqwest`; the `wasm32-wasip2` target cannot. This research surveys the available options for the WASM target.

## Candidates

### `wstd`

A WASI-native async standard library. Provides an HTTP client built on WASI socket interfaces. Active development as of early 2026. No dependency on Tokio.

**Pros:** Purpose-built for WASM/WASI, minimal footprint, async-native.  
**Cons:** API surface smaller than `reqwest`; fewer middleware options.

### `wasm-bindgen` + `web-sys` fetch

Targets `wasm32-unknown-unknown` via browser fetch API. Not applicable to `wasm32-wasip2`.

### Custom WASI socket implementation

Implement directly against WASI preview2 socket interfaces. Maximum control, maximum effort.

**Pros:** No external dependency.  
**Cons:** Significant implementation burden; reimplements what `wstd` already provides.

## Conclusion

`wstd` is the most viable option for the `wasm32-wasip2` target. A thin trait with two implementations — `reqwest` on native, `wstd` on WASM — provides the necessary seam with minimal abstraction overhead. This pattern is consistent with the approach described in `README.md` under "Compile targets."

## References

- https://github.com/bytecodealliance/wstd
- WASI preview2 socket proposal
