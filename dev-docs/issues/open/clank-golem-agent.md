---
title: "clank runs as a wasi:cli/run component, not a Golem agent instance"
date: 2026-07-08
author: agent
---

# clank runs as a wasi:cli/run component, not a Golem agent instance

## Problem

clank.sh's real deployment target is a **durable agent instance in the Golem executor** —
confirmed directly with the Golem team (John de Goes, vigoo). The project is wasm-only; the
`README.md` "durable inside Golem" model is the destination.

Today clank compiles to a `wasi:cli/run` component (a long-running REPL over stdin/stdout
streams, run with `wasmtime run`). Golem does **not** run `wasi:cli/run` components as agents —
it invokes exported agent *methods* (the `golem:agent` world), with durable per-instance state.
So there is no way to deploy clank to a Golem server or invoke it as an agent: the shape does not
line up with what the executor runs.

## Capability gap

- No component exporting the `golem:agent` world; nothing `golem deploy` / `golem agent invoke`
  can target.
- The shell state (Brush `Shell`, cwd, env, the transcript) is not held as durable agent state
  across invocations — each `wasmtime run` is a fresh process.
- The reusable `Session` core exists (`clank_shell::session::Session`) but is only ever driven by
  the stdin REPL driver, never by an invocable method.

## What "resolved" looks like

- A Golem agent (`crates/clank-agent`) wraps `Session` and exposes a line-at-a-time `eval` method
  returning structured stdout, stderr, and exit status; each command is one invocation.
- It builds with `golem build`, deploys to a local `golem server`, and is invocable via
  `golem agent invoke 'ClankAgent("name")' eval '"echo hi"'`.
- The shell + transcript are **durable across invocations** for a given agent identity: a second
  invocation sees the first's transcript (`context show`), and per-identity instances are isolated.
- The per-agent `std::fs` (Golem maps `/` to a per-instance host dir) backs file commands durably.

## Out of scope

Golem cloud deployment; HTTP API / mount routing; snapshotting (oplog compaction, needs serde on
`Session`); pipelines / subshells / `$(...)` on the agent (no threads — the existing wasm
limitation); MCP; authorization; simplifying the coreutils `close(1)` capture onto real fs. This
issue is only about clank existing and running as a Golem agent instance with durable shell state.
