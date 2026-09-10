---
title: "clank cannot use, or be used as, a Golem agent tool"
date: 2026-09-10
author: agent
---

# clank cannot use, or be used as, a Golem agent tool

## Problem

Golem 1.6 introduces **agent tools**: stateless WASM callables with CLI-shaped metadata (command
tree, options, flags, positionals, stdin/stdout streams, exit-coded errors) that agents invoke
through the `golem:tool/host` `tool-rpc` resource, executed in per-invocation sidecar instances
that share the calling agent's filesystem, environment and identity. Upstream `main` at
`8d0fd1f3c` (2026-09-10) implements discovery, manifest declaration and binding, sidecar
execution, streams, durability and typed clients in all four SDK languages.

clank has no relationship with this feature in either direction:

- **As a consumer.** A tool bound to `ClankAgent` is invisible to the shell. It is not on `$PATH`,
  `type`/`which`/`--help` do not know it, `ask` cannot reach it, and `grease` — whose README
  already reserves package type 6 for "wRPC WASM components" — has no way to bind or list it.
  The tool metadata is deliberately CLI-shaped so an agent can reason about it like a Unix
  utility; the WIT itself names a "full CLI projection" as a future deliverable. clank is the
  shell in which that projection belongs, and today provides none of it.
- **As a provider.** A third-party agent can only get a shell by compiling the whole `Session`
  into its own component via `clank-embed`. There is no way to bind clank the way any other tool
  is bound — declared once in `golem.yaml`, updated independently, governed by the operator's
  binding (and, once GOL-39 lands, by bypass-resistant middleware) — nor to reach a shell inside
  an agent that did not embed one.

## Impact

- Operators who adopt agent tools cannot drive them from clank, the one place where a CLI-shaped
  surface would be used as a CLI. Every tool must instead be called from typed SDK clients in
  agent code.
- The "connect to any agent and poke around its sandbox" experience discussed with the Golem team
  (John de Goes, vigoo, 2026-07) is unreachable: the interim `agent shell` client works, but only
  against agents that embed the full shell.
- Model-driven tool use in clank stays confined to intercepted coreutils, HTTP and MCP tools; the
  guardrail model Golem is building around tools (annotations, binding narrowing, middleware)
  does not apply to anything clank does.

## Context

- Direction from the Golem team (Slack, 2026-07): clank should work outside Golem too (durable
  hosting is a perk, not the base experience); grease should be able to install agent tools; the
  end-state for "shell inside any agent" is clank as a `bash`-like tool rather than component
  composition, with `agent shell` as the interim client.
- Upstream constraints that shape any solution, all verified against the code: tools are
  invocable only from inside a guest (no REST/CLI/MCP path); bindings are compiled per agent type
  at deploy time; a tool body runs in a fresh Store per invocation; a filesystem-capable tool's
  stdout is published only at the terminal; a trap inside a tool interrupts the owning agent; the
  manifest cannot yet express a filesystem grant (GOL-29); middleware has no host runtime yet
  (GOL-39).
- The `golem agent shell` PR (golemcloud/golem#3700) was auto-closed for want of a vouched author,
  not on review; it lives on the `clank-connect-patch` branch and remains the primary interactive
  client.
- The golem-rust SDK clone clank builds against is 91 commits behind upstream `main` and predates
  tool execution entirely; the wasm build is already broken by that clone's wasip3 rebase.

## Out of Scope

This issue does not cover component composition of clank into agents, MCP import/export of tools
through Golem, TTY host imports, authoring tool middleware in clank, or changes to the existing
Golem-agent package kind. Those are separate capability questions.
