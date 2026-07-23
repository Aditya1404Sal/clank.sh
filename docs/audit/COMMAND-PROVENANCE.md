# clank — Command Provenance (from-scratch vs crate-backed)

**Target:** `main` (worktree `~/Desktop/clank.sh`). The utilities below are identical on `main-rc-1`;
only the `ask`/agent **provider** layer differs between branches, and no command in this audit
depends on it.
**Question:** which registered commands are implemented **from scratch** (no external crate doing the
real work) versus **backed by a production crate** — so hand-rolled surfaces can be assessed for
production-readiness / "don't reinvent the wheel", within the `wasm32-wasip2` constraint.
**Method:** static — the command registry (`crates/clank-core/src/registry.rs`), the `tools/` impls,
and the `[dependencies]` graph, cross-checked against actual crate usage in each file. No behavioral
run for this pass.
**Verified:** 2026-07-24.

> **Headline.** The "looks like a Unix utility" surface is *mostly already crate-backed* (uutils,
> brush, ripgrep's `grep`, `jaq`, `similar`, `diffy`, `infer`, `reqwest`/`wstd`). The genuinely
> hand-rolled surface is smaller than it looks, and most of it is **forced by wasm** (no viable
> crate, or the obvious crate relies on unix `MetadataExt` / exec-bit checks that don't exist on
> wasip2). There are **three** spots that are real wheel-reinvention worth acting on: URL
> resolution, and `find`/`xargs`.

The authoritative command inventory is built in `crates/clank-core/src/registry.rs:226` (`build()`),
which pulls per-module `manifests()` producers; command names were read from each module's
`builtins()`.

---

## 1. Backed by a production crate — *not* reinvented ✅

| Command(s) | Backing crate | Anchor |
|---|---|---|
| Shell language + `cd echo export exec exit unset source alias jobs fg bg history read wait type command` | **brush** (`brush-core/builtins/parser`, forked for wasm) | `crates/clank-core/Cargo.toml` deps; `registry.rs:47` (`MANUAL_MANIFESTS`) |
| `cat cp cut env head ls mkdir mv printf rm sleep sort tail tee touch tr uniq wc` (18) | **uutils** `uu_*` (forked for wasm; patched in workspace `[patch.crates-io]`) | `tools/coreutils.rs`; `Cargo.toml` `uu_*` deps |
| `grep` | **`grep`** — BurntSushi / ripgrep engine (`grep::searcher`, `grep::regex`, `grep::printer`) | `tools/texttools.rs:380` |
| `jq` | **`jaq`** (`jaq-core` + `jaq-json` + `jaq-std`) | `tools/texttools.rs` |
| `diff` | **`similar`** (`similar::TextDiff`) | `tools/texttools.rs` |
| `patch` | **`diffy`** (`diffy::Patch`/`apply`) | `tools/texttools.rs` |
| `file` | **`infer`** (`infer::get_from_path`) | `tools/texttools.rs` |
| `curl` / `wget` — **transport** | **`reqwest`**+rustls (native) / **`wstd`** WASI-HTTP (wasm), via `whttp` | `utilities/whttp/Cargo.toml:23-33` |
| `grease` integrity | **`ed25519-dalek`** (verify-only) + **`sha2`** + **`base64`** | `crates/clank-core/Cargo.toml` |
| config / `ask` wire | `serde` · `toml` · `serde_json`; `golem-ai-llm` (agent), `reqwest` (native) | `crates/clank-core/Cargo.toml` |

The uutils and brush **forks** are a real maintenance cost, but that's maintaining a fork of a
production engine — not reinventing one.

---

## 2. Genuinely from scratch — hand-rolled command logic ⚠️

Helper crates listed are used only for a *piece* (regex engine, time formatting, arg parsing); the
command semantics are clank's own.

| Command | ~Lines | Helper crates | Anchor | Verdict |
|---|---|---|---|---|
| **`awk`** | ~1,270 | `regex` (ERE only) | `tools/awk.rs` | Own lexer/parser/interpreter. No dominant embeddable Rust awk lib (`frawk` is LLVM-heavy, not wasm-friendly). **Keep** — but it's the #1 correctness/maintenance surface. |
| **`sed`** | ~530 | `regex` (patterns) | `tools/texttools.rs:506` (`run_sed`) | Own script/address parser. No strong sed *library* exists. **Keep**, high test scrutiny. |
| **`stat`** | ~370 | `chrono` | `tools/stat.rs:6` | `uu_stat` explicitly rejected: "its formatting core is `MetadataExt`-unix throughout" (dead on wasm) and clank needs virtual `/bin`,`/proc` reporting. **Wasm-justified — keep.** |
| **`which`** | ~120 | none | `tools/which.rs` | The `which` crate does exec-bit fs checks that *lie* on wasip2 (see [[wasip2-fs-existence-checks]] — `Path::exists()` used instead). **Wasm-justified — keep.** |
| **`ps`** | — | none | `runtime/ps.rs` | Novel: reads clank's own agent proc table. No crate could exist. **Correct as-is.** |
| **`man`** | ~140 | none | `tools/man.rs` | Help-text shim, not real man. Trivial. **Fine.** |
| **`find`** | ~370 | `regex` | `tools/find.rs` | Hand-rolled directory walk + predicates. **Reconsider — see §4.** |
| **`xargs`** | ~230 | `clap` | `tools/xargs.rs` | Hand-rolled batching. **Reconsider — see §4.** |
| **URL resolution** | — | none | `utilities/whttp/src/lib.rs:178` (`resolve_url`), `:218` (`has_scheme`) | Hand-rolled RFC-3986 relative resolution via `split_once("://")`. **Reconsider — see §4.** |

---

## 3. Novel product commands — from scratch *by nature*, not reinvention

`ask` · `model` · `context` · `mcp` · `grease` · `golem` · `kill` · `prompt-user` · `export --secret`
(`secretenv`). These are clank's product surface; they lean on crates for the hard parts (crypto,
HTTP, serde, `golem-rust`). Nothing to "un-reinvent." Registered via
`ai::ask` / `ai::model` / `builtins::context` / `mcp::cmd` / `grease::cmd` / `golem::cluster` /
`builtins::kill` / `builtins::promptuser` (see `registry.rs:252-274`).

---

## 4. The three worth acting on

1. **URL resolution → adopt the `url` crate.** `whttp` hand-rolls relative-reference resolution
   (`resolve_url` / `has_scheme`, `utilities/whttp/src/lib.rs:178`) — the classic footgun surface
   (ports, userinfo, `..` segments, scheme-relative `//host`). The `url` crate (servo/rust-url) is
   *the* production standard and is pure-Rust / wasm-clean. Low-risk, high-payoff swap; keep the
   redirect-policy logic, replace the parsing/joining.
2. **`find` walk → `walkdir`.** Mature, pure-Rust; builds on `std::fs`, which clank already relies on
   under wasip2 (worth a quick `--target wasm32-wasip2` confirmation). Keep the predicate/flag layer,
   replace the hand-rolled traversal.
3. **`find` + `xargs` → or lift from `uutils/findutils`.** Stays in the uutils family already forked
   here, and covers **both** bespoke tools from one maintained source.

**Leave alone** (wasm forces it, or no viable lib): `awk`, `sed`, `stat`, `which`, `ps`, `man`. For
`awk`/`sed` the production-readiness lever isn't a crate swap (none exists) — it's **test coverage**,
already carried by the conformance corpus (`crates/clank-conformance/scenarios/{sed-awk,text-tools}.clank`).

---

## Summary counts

- **Crate-backed engines:** 18 coreutils + `grep` + `jq` + `diff` + `patch` + `file` + curl/wget
  transport + the shell language ≈ the bulk of the utility surface.
- **Hand-rolled command logic:** `awk`, `sed`, `find`, `xargs`, `stat`, `which`, `ps`, `man`, +
  `whttp` URL resolution — **9 surfaces**, of which **3** (`find`, `xargs`, URL resolution) have a
  production crate that likely fits wasm, and **6** are wasm-justified or novel.
- **Novel product commands:** 9 (`ask`, `model`, `context`, `mcp`, `grease`, `golem`, `kill`,
  `prompt-user`, `export --secret`) — from scratch by design.
