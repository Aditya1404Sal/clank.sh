# clank — Conventions & Restructure

**Scope:** `main-rc-1` worktree, compared against `main`. Read-only audit (no build/lint/test run).
**Method:** every claim is anchored; counts come from `rg`/`find` over `crates/` excluding `target/`.
Confidence grades: `Confirmed` (read it) / `Likely` (pattern strong, some context untraced) /
`Unverified` (only a tool can confirm — see `VERIFY.md`).

This document is the *cost-of-maintenance* half of the audit. Production-consequence defects are in
`AUDIT.md`; where they overlap (e.g. stringly-typed errors) they are cross-referenced, not
double-weighted.

---

## Part 1 — Convention gaps (grouped by theme, with counts)

### C1 [P2] No lint or format configuration anywhere — the single cheapest quality win

**Anchor:** repo root — only `rust-toolchain.toml` exists. Absent: `clippy.toml`, `rustfmt.toml`,
`deny.toml`, `.cargo/config.toml`, and any `[workspace.lints]` in `Cargo.toml`.
**Confidence:** Confirmed · **Branch:** both

For a ~33k-LOC largely-generated codebase this is the highest-leverage improvement available: it
converts a whole class of future audit findings into compiler warnings, mechanically. Add to the
workspace root `Cargo.toml`:

```toml
[workspace.lints.rust]
unsafe_code = "warn"          # not "forbid": clank has ~7 real unsafe blocks (see AUDIT unsafe finding)
unreachable_pub = "warn"      # will surface the pub-vs-pub(crate) gap in C3
missing_docs = "warn"         # clank-shell is a real library (rlib consumed by embed/agent)

[workspace.lints.clippy]
all = "warn"
pedantic = "warn"
unwrap_used = "warn"          # the highest-value lint for THIS codebase — ~470 non-test unwrap/expect
expect_used = "warn"
```

Then a `rustfmt.toml` (even empty, to pin style) and a `deny.toml` (licenses + advisories + the
`[patch]`ed git deps). CI (`.github/workflows/conformance.yml` is the only workflow) should run
`cargo fmt --check` and `cargo clippy -- -D warnings`. Commands in `VERIFY.md`.

### C2 [P2] Errors are uniformly stringly-typed — `Result<_, String>`, zero structured enums

**Anchor:** 119 `Result<…, String>` return sites across the non-test tree; `#[derive(…Error)]`
occurrences: **0**; `thiserror` in any crate manifest: **0**. `anyhow` appears only in the two dev
crates (`crates/grease-tool/Cargo.toml`, `crates/clank-conformance/Cargo.toml`), which is fine.
**Confidence:** Confirmed · **Branch:** both

This is *internally consistent* (so it is not the "mixed anyhow/thiserror with no rule" finding), but
it means no caller anywhere can match on an error kind — only display it. `clank-shell` is an `rlib`
consumed by `clank-agent`, `clank-embed`, and `clank-conformance`, so it is a library by the
crate-boundary test, and a library that returns `String` errors pushes every recovery decision onto
string-matching. This compounds as the surface grows. Direction: a `thiserror` enum per subsystem
(`grease::Error`, `mcp::Error`, `ai::Error`) with `#[from]` wrapping and `#[non_exhaustive]`, at
least on the paths callers branch on (install failure kinds, MCP transport vs protocol errors). Not
a mechanical sweep — do it where a caller actually needs to distinguish outcomes; leave the rest.

### C3 [P2, Likely] `pub`-heavy visibility

**Anchor:** 409 `pub fn`/`pub struct`/`pub enum` vs 88 `pub(crate)` + 27 `pub(super)` (~78% of
exported items are fully public).
**Confidence:** Likely (much of `clank-shell`'s `pub` surface is the intended library API for
`clank-embed`/`clank-agent`; I did not trace which items those consumers actually import) · **Branch:** both

Everything `pub` is a commitment. The correct default is private, widening to `pub(crate)` then `pub`
on demonstrated need. The `unreachable_pub` lint from C1 will produce the exact list of
over-exposed items; tighten **after** the crate boundaries in Part 2 settle (visibility last, per the
migration ordering) so you are not re-adjusting twice.

### C4 [P3] `get_` prefix getters — mostly false positives, a genuine handful

**Anchor:** 21 `fn get_*` sites, but several are not getter-convention violations:
`mcp/client.rs:520 get_prompt` mirrors the MCP `prompts/get` protocol method (domain-correct);
`http_native.rs:123 get_returns_status_and_body`, `wcurl/parse.rs:386 get_flag_records_intent` are
**test names**; `get_content` recurs as an internal helper name.
**Confidence:** Confirmed · **Branch:** both

Low value. The Rust API guideline is getters are `name()` not `get_name()`, but the real violation
count here is a handful, not 21. Fix opportunistically; not worth a dedicated pass.

### C5 [P2] Supply-chain: a `[patch]` git dependency pinned to a **branch**, not a revision

**Anchor:** root `Cargo.toml` `[patch.crates-io]` — `uucore` and every `uu_*` crate point at
`github.com/Aditya1404Sal/coreutils`, **`branch = "wasip2-oscompat"`**; the three `brush-*` crates
point at a fork **`rev = "02de798"`** (pinned correctly).
**Confidence:** Confirmed · **Branch:** both

The `brush` pin is reproducible (a rev). The coreutils pin is a **branch** — its `Cargo.lock` entry
records whatever commit the branch pointed at when the lock was last written, but any `cargo update`
silently advances it, and a fresh checkout resolves to branch-tip. For a security-sensitive shell
whose coreutils fork carries wasm-specific `unsafe`/permission changes, pin coreutils to an exact
`rev` too, and vendor or mirror both forks (they are single-maintainer personal forks — a delete or
force-push breaks every build). This is a real production-readiness concern for a release candidate:
the build is not reproducible from source control alone. Tracked in `VERIFY.md` (`cargo deny check`
will also flag the git sources).

---

## Part 2 — Workspace restructure

The layout standard: a newcomer opening the repo root should tell within thirty seconds what ships,
what is a fixture, what is an example, what is plumbing. clank is *mostly* there — the recent reorg
into concern directories (`mcp/`, `grease/`, `ai/`, `golem/`, `tools/`, `builtins/`, `runtime/`,
`session/`) is clean, there are **no god-modules** (`utils`/`common`/`types` — none found), module
style is consistent `dir/mod.rs`, and nesting stays shallow (max depth 2 module levels). So this is
tuning, not a rescue. Three moves, ranked by payoff over disruption.

### R1 [do first] `clank-shell` is a `cdylib` + `rlib` + binary and the crate everything depends on

**Anchor:** `crates/clank-shell/Cargo.toml` `crate-type = ["cdylib", "rlib"]` plus
`crates/clank-shell/src/main.rs`; every other clank crate depends on it (`clank-agent`,
`clank-embed`, `clank-conformance` path-dep it).
**Confidence:** Confirmed · **Branch:** both

One 27.7k-LOC crate is simultaneously: the native binary (`main.rs`), the wasm-agent library, and
the shared rlib. Consequences: (a) it serialises the build — everything waits on it; (b) `cdylib`
means it cannot be `cargo test`ed the same way as a pure lib, and it does not link natively as part
of `--workspace` (a known footgun — tests must enumerate `-p`); (c) the binary and library are
entangled (`main.rs` logic is not testable without the binary).

Direction — **not urgent, but the highest-clarity move**: split the shared engine into a pure `rlib`
(`clank-core` or keep `clank-shell` as lib-only) and put the native binary in a thin
`clank-cli` crate (`main.rs` parses args → calls the lib). The wasm `cdylib` export already lives in
`clank-agent`, so `clank-shell` does not itself need to be `cdylib` — dropping that crate-type is the
single change that most simplifies the build graph. Verify nothing imports `clank-shell` expecting the
`cdylib` artifact (it should not — `clank-agent` is the component).

### R2 [do second] `greeter-agent` is a demonstration fixture living in `crates/`

**Anchor:** `crates/greeter-agent/` (75 LOC, `crate-type = ["cdylib"]`) — its own doc comment says it
"exists to prove two clank seams end-to-end" and is "a deliberately trivial second Golem agent."
**Confidence:** Confirmed · **Branch:** main-rc-1 only (greeter-agent is part of the RC's fuller
feature set; confirm it is absent/different on `main` before moving — see note)

It is a test fixture / example by its own description, sitting among production crates, so it appears
in the workspace's default surface and is built for consumers who do not need it. *Caveat that
softens this:* it is a genuine second wasm component (a real compile target, and the second
implementer of the `clank-embed` shell surface — that is the thing it proves), so the crate boundary
is partly earned by the wasm heuristic. Direction: move it under `fixtures/` (or `examples/` if you
want it read as documentation), set `publish = false`, and exclude it from default workspace members
so `cargo build` at the root does not compile it. This keeps the seam-proof while telling the truth
about what it is.

### R3 [do third] `wcurl` / `waget` residual duplication in their arg parsers

**Anchor:** `crates/wcurl/src/parse.rs` + `crates/waget/src/parse.rs` (each ~16-unwrap dense),
plus each crate's `lib.rs`/`main.rs`. Both already path-dep the shared `whttp` transport
(`crates/wcurl/Cargo.toml:20`, `crates/waget/Cargo.toml:20`), so the *transport* duplication the
divergent-duplication pattern warns about is **already half-solved**.
**Confidence:** Confirmed (the line-by-line diff was done) · **Branch:** both

The line-by-line diff came back **mostly benign**: the two are genuinely different tools, so most flag
differences are correct-per-tool asymmetry (`-i`/`-I`/`-G` in wcurl; `-O`/`-t`/`--content-disposition`
in waget), not defects. The one divergence worth acting on is **D1** (AUDIT P3-5): wcurl never sets
`max_redirects` and silently inherits `whttp::Request::new`'s literal `50`, while waget sets `20`
explicitly — so `curl -L`'s cap is an invisible dependency on a constant in a third crate. Two cosmetic
mismatches also exist (a `-v` that's a real feature in wcurl but a silent no-op in waget; a doc/impl
mismatch on waget's `-nc`), neither a live bug. So the restructure case here is **weaker than the
duplication pattern usually implies** — the shared transport is already extracted, and the residual
parser overlap is small. Direction: pin wcurl's redirect cap explicitly (P3-5), and only if the
parsers grow further, lift shared flag-handling into a small `whttp-cli` helper; not urgent today.

---

## Migration path (compiles after every step)

1. **C1 + C5** — add `[workspace.lints]`, `rustfmt.toml`, `deny.toml`, and pin coreutils to a `rev`.
   No code movement, immediate benefit, zero risk. **This is the single first move.**
2. **R2** — move `greeter-agent` to `fixtures/` (or `examples/`), `publish = false`, exclude from
   default members. Pure relocation; only path references and the root `members` list change.
3. **R3** — consolidate `wcurl`/`waget` parsing into a shared helper (after the divergence diff).
4. **R1** — split `clank-shell` into lib + `clank-cli` binary and drop the `cdylib` crate-type.
   Larger; do it when the build-graph pain justifies it.
5. **C3** — tighten `pub` → `pub(crate)` using the `unreachable_pub` output, **last**, once R1/R3
   have settled the boundaries.

**If you do one thing:** step 1. It is zero-risk, mechanical, and it is what turns the next audit's
worth of findings into warnings you see on every build.

---

## Coverage ledger (restructure)

Read in full: root + all 9 crate `Cargo.toml`s; `authz.rs`; `grease/pkg.rs`; `grease/state.rs`
(sizes only); `session/grease.rs` (install flow); `session/ask.rs` (gate + `resolve_command_scope`);
`clank-agent/src/lib.rs`; `greeter-agent/src/lib.rs`; `clank-embed/src/shell.rs` (eval wrapper).
Probed (counts, not full reads): visibility, error-type, getter, module-style, `cfg`, and
supply-chain density across the whole tree.
**Not examined for this document:** the internal module structure of `builtins/`, most of `tools/`,
and `runtime/` beyond `proctable.rs` — a deeper module-boundary review of those was out of scope for
a findings-first pass. The `wcurl`/`waget` parser divergence (R3) is being quantified separately.
