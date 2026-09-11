---
title: "On the agent, awk's printf width bound is skipped for any width that overflows a 32-bit usize"
date: 2026-09-11
author: agent
---

# awk's width bound is skipped when the width overflows `usize`

## Problem

`parse_width_and_precision` in `crates/clank-core/src/tools/awk.rs` parses a `%` conversion's width
as

```rust
let width: Option<usize> = digits.parse().ok();
if width.is_some_and(|w| w > crate::config::limits::MAX_PRINTF_WIDTH) {
    return Err(too_big("field width").into());
}
```

A width whose digits do not fit in `usize` fails to parse, `.ok()` turns that failure into `None`,
and `None` reads as "no width given". The bound is therefore **skipped rather than enforced**: the
conversion prints unpadded and exits 0.

`usize` is 64 bits natively and **32 bits on `wasm32-wasip2`**, so the same input behaves differently
per target. `%9999999999d` (9,999,999,999 > 4,294,967,295) fits natively and is refused with "field
width exceeds 65536"; on the agent it overflows and prints `1`.

The precision path has the same shape one statement later — `digits.parse().unwrap_or(0)` — so an
overflowing precision silently becomes 0.

## Evidence

The golem conformance tier fails `resilience-hostile-input` at its first step:

```
run awk 'BEGIN{printf "%9999999999d", 1}'
stdout expected empty          got: "1"
stderr missing substring "field width exceeds"
exit code: expected non-zero, got 0
```

The native tier passes the same scenario, and `awk.rs`'s unit test
`an_absurd_printf_width_is_refused_not_allocated` asserts exactly this refusal and passes — natively,
the only place unit tests run. Step 3 of the scenario (`%.9999999999f`, the precision path) is never
reached on the agent, because the harness stops a scenario at its first failing step; by the code it
fails the same way.

The scenario (`25e2a22`) and the bound (`620c947`) both arrived with `main`'s merge into `main-rc-1`.
Nothing in the coreutils fork is involved: `awk.rs` does not use uucore at all.

## Consequences

- **Not an allocation hazard.** The width is dropped, not honoured, so nothing large is built; the
  bound's memory purpose holds.
- **Its contract does not.** The resilience tier's rule is that absurd input is refused. On the agent
  this input is accepted and silently rewritten into a different request — "output the caller never
  asked for", which the function's own doc comment names as exactly what refusing exists to prevent.
- **Target-dependent output for identical input**, in a shell whose conformance suite exists to keep
  the two targets identical.
- **The unit test cannot see it**, because it runs where `usize` is wide enough.

## Scope

On `wasm32-wasip2`, any width or precision from 2^32 up. Natively the same skip happens from 2^64 up
(`%99999999999999999999d`), which no current test exercises.
