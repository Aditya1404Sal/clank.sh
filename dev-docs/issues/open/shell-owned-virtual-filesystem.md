---
title: "WASM POC uses host preopens instead of shell-owned virtual filesystem"
date: 2026-07-08
author: agent
---

# WASM POC uses host preopens instead of shell-owned virtual filesystem

## Problem

The current WASM POC exercises file-oriented commands through WASI preopened host directories
(`wasmtime --dir ...`) and direct `std::fs` access. This lets commands like `cat`, `mkdir`,
`jq`, `grep`, `diff`, `patch`, and `file` work under Wasmtime, but it does not match the
filesystem model described in `README.md`.

The README describes clank.sh as a single component with a shell-owned Linux-like filesystem:
`/tmp`, `/home/user`, `/usr`, `/bin`, `/proc`, `/mnt/mcp`, and related namespaces are part of
the shell environment. Some paths are regular shell state, some are virtual, and in Golem the
state is durable. The AI should not get ambient access to the developer's host filesystem.

## Impact

The current behavior is useful as a smoke-test harness, but it weakens the sandbox story:

- running the component with `--dir .` exposes the host repo as the shell filesystem;
- commands operate on preopened host paths instead of clank-owned paths;
- absolute paths such as `/tmp/...` only work when the host path is explicitly preopened;
- builtins and text/data tools currently call `std::fs` directly, making it harder to enforce
  clank's authorization and virtual namespace rules later;
- the POC can accidentally teach users and agents that host preopens are the product model.

## Expected Behavior

The WASM component should expose a clank-owned filesystem rooted at `/`, independent of arbitrary
host directories. Basic workflows should work without preopening the repository:

```sh
mkdir -p /tmp/clank-texttools
echo hi > /tmp/clank-texttools/a.txt
cat /tmp/clank-texttools/a.txt
```

Those operations should affect clank's `/tmp`, not macOS `/tmp` or the repository directory.
Virtual namespaces such as `/bin`, `/proc`, and `/mnt/mcp` should be resolved by the shell
filesystem layer rather than by WASI host path lookup.

## Context

This gap became visible while testing the WASM text/data builtins. The command:

```sh
wasmtime run -Sp3 -W component-model-async=y --dir . \
  target/wasm32-wasip2/release/clank_shell.wasm
```

allows relative file commands to work because `.` is a host preopen. That is acceptable for the
current POC, but it is not the final clank.sh sandbox or Golem durability model.

## Out of Scope

This issue does not require implementing Golem durability, MCP resource mounts, package
installation, authorization prompts, or a complete POSIX filesystem. It is about introducing the
shell-owned filesystem boundary so command implementations stop depending directly on host
preopens as their primary storage model.
