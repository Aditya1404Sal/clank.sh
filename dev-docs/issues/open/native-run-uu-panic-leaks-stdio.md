---
title: "A panicking uu_* builtin leaves native clank's stdio redirected — and hangs any pipeline it is in"
date: 2026-09-11
author: agent
---

# A panicking `uu_*` builtin leaves native stdio redirected

## Problem

Native `run_uu` (`crates/clank-core/src/tools/coreutils.rs`) runs a uutils command in-process by
pointing the **process-global** fds 0/1/2 at the files Brush assigned to the command (`dup2`), calling
`uumain()`, and then restoring the originals. The restore is a plain loop that runs after `uumain()`
returns. It is not tied to a `Drop` guard, so **if `uumain()` panics, the restore never runs.** On
native the panic strategy is `unwind`, so the panic propagates out of `run_uu` and the process keeps
going with fd 1 and fd 2 still pointing at that command's pipe or capture file, and with the three
saved descriptors leaked.

The code's own comment claims the opposite. Above the `FD_SWAP_LOCK` acquisition:

> Poisoning is harmless here — the guarded region restores fds even on panic paths — so recover the
> guard either way.

The lock half of that is true: the guard is dropped on unwind, and `unwrap_or_else(into_inner)`
recovers a poisoned lock. The fd half is not. Nothing restores the fds on an unwinding path. The
neighbouring `ShellCwd` guard shows the right pattern is already in use for the working directory,
which *is* restored on unwind because it lives in `Drop`.

## Evidence

Found while fixing the coreutils `printf` width panic
([`dev-docs/research/coreutils-printf-width-panic.md`](../../research/coreutils-printf-width-panic.md)).
With the unpatched fork, the native conformance tier ran every scenario up to `printf-wide-padding`
and then **hung for 10 min 37 s at 0.0% CPU** before it was killed. The first step of that scenario
(`printf '%65535s' '' | wc -c`, under the formatter's ceiling) is fine; the next
(`printf '%65536s' '' | wc -c`) is the first width at which uucore panicked.

A hang at zero CPU fits this defect exactly. After the panic, fd 1 still holds a duplicate of the
pipe's write end. Brush drops its own handle as the stage unwinds, but the process-level duplicate
keeps the pipe open, so `wc` never reads EOF and the pipeline never finishes. That is the inferred
mechanism. It is consistent with every observation, but it has not been confirmed with fd-level
instrumentation.

With the `printf` fix applied, the same scenario passes in 0.29 s. That removes one panic source. It
does not remove this defect.

## Consequences

- **Any** panic in **any** `uu_*` builtin on native has this effect, not only `printf`'s. uutils is
  a large surface; a panic there is not hypothetical, and this one was reachable from ordinary user
  input.
- Inside a pipeline, the result is a hang with no error and no output: the worst failure shape for
  an interactive shell and for a test runner alike.
- Outside a pipeline, the session does not hang, but everything the process writes afterwards goes
  to whichever file the panicking command was redirected to, until the process exits.
- Native `clank` is the build meant to be usable outside Golem, so this is a product-level hazard,
  not a test-only one.

## Scope

Native only. On `wasm32-wasip2` the panic strategy is `abort`: a panic traps the guest regardless of
what `run_uu` does, and no `Drop` runs. The agent-side consequence of a uucore panic is a wedged
instance, which is a separate problem with a separate remedy (not panicking in the first place).
