---
title: "No runnable entrypoint: need a dual-target interactive shell loop"
date: 2026-07-07
author: agent
---

# No runnable entrypoint: need a dual-target interactive shell loop

## Problem

clank.sh has a complete design in `README.md` but no code. There is nothing to run. Every
capability the shell will grow (`ask`, builtins, the transcript, the process model, Golem,
MCP) assumes a long-running interactive loop as its spine — a process that starts, presents
a terminal-like prompt, reads what the user types, responds, and keeps running until they
leave. That spine does not exist yet.

`README.md` also states the shell targets **both `wasm32-wasip2` and native Rust** (see
*Compile targets* and *Compatibility Reference*): sandboxed as a WASM component (and, later,
durable inside Golem), and runnable natively so developers get a scriptable AI shell without
a cluster. So the entrypoint must exist on both targets, not just one.

## Capability gap

- No WebAssembly component that can be loaded and run on `wasmtime`.
- No native executable to run the shell on a developer machine.
- No stdin → evaluate → stdout cycle, and no code path shared between the two targets.

## What "resolved" looks like

- A single codebase presents a persistent, terminal-like read/eval/print loop that runs
  **both** as a WASI component under `wasmtime` **and** as a native binary.
- It reads lines from stdin, responds on stdout, stays alive across many commands, and exits
  cleanly on end-of-input or an explicit request to leave.
- Command evaluation is shared between the targets; only the stdin/stdout mechanism differs.

## Out of scope

Bash/Brush parsing, real command resolution, the transcript, `ask`, authorization, the
filesystem layout, Golem, MCP, and native readline/history/TUI. This issue is only about a
running, dual-target interactive loop to build everything else on.
