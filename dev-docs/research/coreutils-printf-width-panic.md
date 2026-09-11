---
title: "printf's width padding panics above u16::MAX and wedges the durable agent"
date: 2026-09-11
author: agent
---

# `printf` width padding panics above `u16::MAX`

Investigation for `workspace-cohesion` ticket 6. The fix belongs in
`github.com/Aditya1404Sal/coreutils` (Aditya's fork), which this agent cannot push to — so the
diagnosis and the patch are recorded here ready to apply.

## Symptom

`printf '%150000s' ''` on the durable Golem agent panics inside uucore and **traps the guest**.
A trapped instance is not recoverable: every later invocation returns "Previous invocation failed".
During a 2026-09-11 e2e run this fired once in the transcript-eviction setup and cost 239 of 272
assertions, all reading back as empty output.

Bisected precisely: **width 65,000 works, 70,000 traps.**

Reachable from any user script. This is the one finding in the epic that is a live production
hazard rather than a maintainability one.

## Diagnosis

**It is not a missing guard. It is a guard at the wrong threshold.**

The fork already checks the width — `uucore/src/lib/features/format/mod.rs:134`:

```rust
const MAX_FORMAT_WIDTH: usize = 1_000_000;

fn check_width(width: usize) -> std::io::Result<()> {
    if width > MAX_FORMAT_WIDTH { Err(...OutOfMemory, "formatting width too large") } else { Ok(()) }
}
```

But Rust ≥1.88 stores `core::fmt` dynamic widths as **`u16`**, so the real ceiling is **65,535** —
`Argument::from_usize` asserts above it. The guard sits at 1,000,000, roughly 15× too high, so
**every width between 65,536 and 1,000,000 passes the check and then aborts inside the formatter.**
`MAX_FORMAT_WIDTH` was presumably chosen when `core::fmt` widths were `usize`.

### The codebase already knows this

`uucore/src/lib/features/format/num_format.rs:329-344` carries a helper whose doc comment states the
bug exactly:

```rust
/// Left-pad `s` with `'0'` until it is at least `width` characters long.
///
/// Unlike `format!("{s:0>width$}")`, this does not feed `width` into the
/// standard formatting machinery, which panics with "Formatting argument out
/// of range" once the dynamic width exceeds `u16::MAX`. A large precision such
/// as `%.100000d` is valid input for `printf`/`seq`, so it must not panic.
fn zero_pad_to(s: &str, width: usize) -> String { ... }
```

So the defect was found and fixed for the **precision** path. The same defect remains in the
**width** path. The technique is already blessed here; it just was not applied everywhere.

### All six affected sites

| File | Line | Expression |
|---|---:|---|
| `format/spec.rs` | 555 | `write!(writer, "{: <padlen$}", "")` |
| `format/spec.rs` | 557 | `write!(writer, "{: >padlen$}", "")` |
| `format/num_format.rs` | 731 | `{sign_indicator}{s:<remaining_width$}` |
| `format/num_format.rs` | 737 | `{s:>width$}` |
| `format/num_format.rs` | 739 | `{sign_indicator}{s:>remaining_width$}` |
| `format/num_format.rs` | 750 | `{sign_indicator}{prefix}{rest:0>remaining_width$}` |

**`spec.rs` is the one clank hits** (`%150000s` is string padding). The five `num_format.rs` sites
are the same latent bug on the numeric paths — `printf '%100000d' 1` should trap identically.

## Why not simply lower `MAX_FORMAT_WIDTH` to 65,535

It would stop the panic, and it would be wrong. GNU `printf '%150000s' ''` prints 150,000 spaces
happily; erroring instead is a capability regression, and `zero_pad_to`'s own comment already
argues the point for precision ("a large precision such as `%.100000d` is valid input"). The cap
should bound *memory*, which is what 1,000,000 is for. The formatter limit is an implementation
detail that padding should not be subject to at all.

## The patch (`spec.rs` — the site clank hits)

```diff
--- a/src/uucore/src/lib/features/format/spec.rs
+++ b/src/uucore/src/lib/features/format/spec.rs
+/// Write `n` spaces without feeding `n` to `core::fmt`.
+///
+/// `write!(w, "{: >n$}", "")` PANICS for `n > u16::MAX` on Rust >= 1.88, which stores dynamic
+/// formatting widths as `u16` (`Argument::from_usize` asserts). `MAX_FORMAT_WIDTH` is 1_000_000, so
+/// every width in 65_536..=1_000_000 passed `check_width` and then aborted inside the formatter.
+/// Under a durable wasm host that abort traps the guest and wedges the instance for every later
+/// invocation, so it is not a recoverable error — it must not happen at all.
+///
+/// Same technique as `num_format::zero_pad_to`, which already documents this for the precision path.
+fn write_spaces(mut writer: impl Write, mut n: usize) -> std::io::Result<()> {
+    const SPACES: [u8; 256] = [b' '; 256];
+    while n > 0 {
+        let chunk = n.min(SPACES.len());
+        writer.write_all(&SPACES[..chunk])?;
+        n -= chunk;
+    }
+    Ok(())
+}
+
 fn write_padded(
     mut writer: impl Write,
     text: &[u8],
     width: usize,
     left: bool,
 ) -> Result<(), FormatError> {
     let padlen = width.saturating_sub(text.len());
 
-    // Check if the padding length is too large for formatting
+    // Bounds MEMORY, not the formatter: `write_spaces` below has no `u16` ceiling.
     super::check_width(padlen).map_err(FormatError::IoError)?;
 
     if left {
         writer.write_all(text)?;
-        write!(writer, "{: <padlen$}", "")
+        write_spaces(&mut writer, padlen)
     } else {
-        write!(writer, "{: >padlen$}", "")?;
+        write_spaces(&mut writer, padlen)?;
         writer.write_all(text)
     }
     .map_err(FormatError::IoError)
 }
```

`impl Write` covers `&mut W where W: Write`, so `&mut writer` satisfies the helper.

The five `num_format.rs` sites need the same treatment: build the padded `String` with
`std::iter::repeat_n` (as `zero_pad_to` does) rather than a `{:>width$}` format, then write it.

## Verification

```bash
# in the fork
cargo test -p uucore --features format
printf '%150000s' '' | wc -c        # → 150000, no panic
printf '%100000d' 1 | wc -c         # → 100000, once num_format is fixed too
```

Then, in clank:

1. Bump the `rev` for the 20 `uu_*`/`uucore` entries in `[patch.crates-io]` (root `Cargo.toml`).
2. Restore the single-conversion form in `scripts/golem-e2e.sh` — it is currently three sub-limit
   conversions to route around this bug, and that line becomes the regression test:
   ```diff
   -run_line "printf '%60000s%60000s%40000s' '' '' ''" >/dev/null
   +run_line "printf '%150000s' ''" >/dev/null
   ```
3. Run `scripts/golem-e2e.sh --takeover`; the eviction block must pass and the agent must answer
   normally afterwards (the wedge signature is "Previous invocation failed" on every later call).

## Worth upstreaming

This is not clank-specific and not fork-specific: the `u16` ceiling landed in Rust ≥1.88 and the
guard predates it. `uutils/coreutils` upstream has the same shape unless it has since been fixed, and
`zero_pad_to` shows the project already accepts this fix pattern. A PR there would remove one of
the reasons this fork exists.
