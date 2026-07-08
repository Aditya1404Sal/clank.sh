# Plan: clank.sh as a Golem agent instance (`crates/clank-agent`)

## Context

clank.sh's real deployment target is a **durable agent instance in the Golem executor** — confirmed
directly with the Golem team (John de Goes, vigoo). The project has pivoted to **wasm-only**; native
is deprioritized (kept working, not extended). Two things the Golem team clarified collapse earlier
open questions:

- **Filesystem:** Golem maps `/` for each agent instance to a separate host dir; `std::fs` works out
  of the box on WASI, per-agent-isolated, and durable across invocations. This makes the
  `dev-docs/issues/open/shell-owned-virtual-filesystem.md` gap essentially moot — the per-agent host
  FS *is* the shell-owned filesystem.
- **Shape:** a Golem agent exports invocable *methods* (`golem:agent` world), not `wasi:cli/run` over
  stdin. So clank must be wrapped as an agent: durable state = the shell Session + transcript; each
  command = one invocation (the user-chosen "line-at-a-time agent method" model).

Today clank's reusable core already exists: `clank_shell::session::Session` with
`async fn new()` and `async fn run_line(&mut self, &str) -> (Vec<u8>, Flow)`, target-agnostic, with a
`Transcript` and the `context` builtin already wired in. The existing `wasi:cli/run` REPL driver
(`wasm.rs`) and native driver stay as-is. This plan adds a **new workspace crate** `crates/clank-agent`
that wraps `Session` in a Golem agent, deployable to a local `golem server` and invocable via
`golem agent invoke`.

The whole codebase and the **Golem monorepo (including the Rust SDK)** are checked out locally at
`golem-stuff/golem/`; the SDK is `golem-stuff/golem/sdks/rust/golem-rust`.

## The one hard problem: the executor / `block_on` conflict

Brush hard-requires a **tokio** runtime *in TLS context*: bare `tokio::spawn` (brush interp.rs
293/763/1945, commands.rs:777 — pipeline/subshell/`$(...)`) and `tokio::task::spawn_blocking`
(commands.rs:455 — owned-shell builtin). Brush's wasm tokio features are `["io-util","macros","rt",
"sync"]` (current-thread `rt`, no threads). A **simple top-level command** (echo, cat, our
coreutils/texttools builtins) routes parent-shell → `.await` and hits **neither** spawn path; only
pipelines/subshells do. clank's wasm `Session` drives Brush via a tokio current-thread
`rt.block_on(...)`.

From reading the SDK source (not docs): **`golem-rust` has ZERO tokio dependency and drives every
agent invocation with `wstd::runtime::block_on`** (`sdks/rust/golem-rust/src/agentic/agent_registry.rs:147`).
Agent methods are genuinely `async` and `.await`ed under that outer `wstd::block_on`. The
"never use `block_on`" guidance means: the SDK owns the outermost `block_on`; don't nest your own.

So clank's `Session::run_line` doing `tokio_rt.block_on(brush.run_string())` becomes a **tokio
current-thread block_on nested inside the SDK's wstd block_on** — two single-threaded executors nested.
This either works (Brush's future for a simple command completes as pure-CPU without awaiting a
wstd/wasip2 pollable — true for in-process builtins) or panics on re-entrancy. **This is decided
empirically, not by more reading.**

## Recommended approach: spike first, then wire

**Step 0 — de-risking spike (throwaway, before wiring the real crate).** Build the smallest agent that
links the *real* `Session` and calls it, then invoke on the local server:
- one agent method `async fn run_line(&mut self, cmd: String) -> String` that lazily builds
  `clank_shell::session::Session::new().await` into durable state and calls `session.run_line(&cmd).await`,
  returning `String::from_utf8_lossy(&out)`;
- with clank-shell's `mod wasm` feature-gated off (Step 2) so it links as a plain rlib with the
  existing tokio `block_on` Session intact;
- `golem server run`, then `golem agent invoke 'ClankAgent("demo")' run_line '"echo hi"'`.

Decision gate:
- **prints `hi`** → keep the existing tokio-current-thread + `block_on` Session **verbatim** (lowest
  effort, native/REPL untouched). Document the deviation from the "no block_on" guideline with the
  empirical evidence.
- **panics on nested block_on / tokio won't init** → pivot to driving Brush's top-level future with
  `wstd::runtime::block_on` while entering a tokio runtime only for context
  (`tokio::runtime::Handle::enter()` to satisfy Brush's internal `tokio::spawn`), restricted to the
  simple-command path. Feature-gate this as an alternate Session driver so it's a small pivot, not a
  rewrite.

Milestone 1 scope (either branch): **simple commands + shell language + our builtins**. Pipelines /
subshells / `$(...)` hit `tokio::spawn`/`spawn_blocking` (no threads on the agent) and remain clank's
already-documented wasm limitation.

## New files

### `crates/clank-agent/Cargo.toml`
- `edition = "2024"` (matches golem-rust macro expectations; rustc 1.96 supports it; clank-shell stays
  2021 — mixed editions are fine). `[lib] crate-type = ["cdylib"]` (component, not rlib).
- deps: `golem-rust = { path = "../../golem-stuff/golem/sdks/rust/golem-rust", features = ["export_golem_agentic"] }`
  (path-dep the vendored SDK, per the user), `wstd = "=0.6.5"`, `serde`, and
  `clank-shell = { path = "../clank-shell", default-features = false }` — the Session core only, no REPL export.
- **No `[profile.release]`** here (ignored in a member crate).

### `crates/clank-agent/src/lib.rs`
- `mod clank_agent;` (the `export_golem_agentic` feature + macros emit the component exports; no manual
  `export!`).

### `crates/clank-agent/src/clank_agent.rs` — the agent
```rust
use golem_rust::{agent_definition, agent_implementation};
use clank_shell::session::Session;

#[agent_definition]                     // mount optional; omit for milestone 1 (invoke, not HTTP)
pub trait ClankAgent {
    fn new(name: String) -> Self;       // constructor params = agent identity
    async fn run_line(&mut self, cmd: String) -> String;  // one command = one invocation
}

pub struct ClankAgentImpl { name: String, session: Option<Session> }  // Session = live durable state

#[agent_implementation]
impl ClankAgent for ClankAgentImpl {
    fn new(name: String) -> Self { Self { name, session: None } }
    async fn run_line(&mut self, cmd: String) -> String {
        if self.session.is_none() {
            match Session::new().await { Ok(s) => self.session = Some(s),
                Err(e) => return format!("clank: failed to start shell: {e}\n") }
        }
        let (out, _flow) = self.session.as_mut().unwrap().run_line(&cmd).await;  // Flow ignored: no REPL to exit
        String::from_utf8_lossy(&out).into_owned()
    }
}
```
Design notes:
- **Durable transcript for free:** the `Session` lives in agent durable state, so `run_line` records
  into the same `Transcript` across invocations; `context show`/`clear` already work via the existing
  `dispatch_context` inside `Session::run_line`. Two invocations → second sees the first's transcript.
- **Durability model (verified from SDK):** Golem durability is **oplog-based, not serde-based** by
  default — the live agent struct stays in worker memory between invocations (no serialization of
  `Session`/`Shell`/tokio `Runtime` needed on the happy path); on recovery Golem **replays the oplog**
  (re-runs `new` + past `run_line`s). Deterministic commands replay correctly; non-deterministic ones
  (clock/random) would need Golem's durability wrappers — out of scope. Snapshotting (oplog compaction)
  requires `Serialize`/`Deserialize` and is **out of scope** for milestone 1.
- `String` is the only type crossing the boundary (implements `Schema`). A structured
  `#[derive(Schema)] RunResult { output, exit_code }` is a later nicety.

### `golem.yaml` (repo root, NEW)
- `manifestVersion: 1.5.0`; `components: { clank:agent: { template: rust, dir: crates/clank-agent } }`;
  `environments: { local: {} }`; `httpApi` omitted for milestone 1.
- **Capture the exact `templates.rust` block from a real `golem new` run** rather than hand-writing the
  `componentWasm`/`linkedWasm`/`sourceWit` keys — scaffold into a scratch dir, transplant the crate +
  manifest shape, reconcile paths to repo root. **Do not let `golem new` overwrite the workspace root
  `Cargo.toml`.**

## Edits to existing files

### `crates/clank-shell/src/lib.rs` (lines 23-24) — feature-gate the REPL export
```rust
#[cfg(all(target_arch = "wasm32", feature = "repl-driver"))]
mod wasm;
```
This confines `wasm.rs`'s `wasip3::cli::command::export!(Component)` to the `repl-driver` feature, so
clank-agent (`default-features=false`) gets the Session core with **no `wasi:cli/run` export** — avoiding
a duplicate-world clash with the agent component. `session`/`coreutils`/`texttools`/`Transcript`/`Flow`
stay unconditional.

### `crates/clank-shell/Cargo.toml` — declare the feature, make REPL deps optional
- `[features] default = ["repl-driver"]` and
  `repl-driver = ["dep:wasip3", "dep:wit-bindgen", "dep:futures"]` (default keeps native + existing
  wasm REPL unchanged).
- In `[target.'cfg(target_arch="wasm32")'.dependencies]`, mark the three as
  `{ workspace = true, optional = true }`.

### `Cargo.toml` (workspace root)
- `members = ["crates/clank-shell", "crates/clank-agent"]`.
- If the golem scaffold emitted `[profile.release]` (`opt-level="s"`, `lto`), host it here (member
  profiles are ignored). `[patch.crates-io] uucore = fork` is inherited by clank-agent transitively —
  no change.

## Verification (end-to-end)

1. **Feature gate proves out (no golem):**
   - `cargo build -p clank-shell --target wasm32-wasip2 --lib --no-default-features` succeeds;
     `wasm-tools component wit target/wasm32-wasip2/debug/clank_shell.wasm` shows **no `wasi:cli/run`**.
   - Native unchanged: `printf 'echo hi\nexit\n' | cargo run -q --bin clank` prints `hi`.
   - Existing REPL wasm still builds with default features and runs under wasmtime.
2. **Spike (Step 0) is green** — `run_line('"echo hi"')` returns `hi` on the local server. (Gates the
   whole approach; do this before full wiring.)
3. **Agent build/deploy:** `golem build --yes` → `cargo build --target wasm32-wasip2` for clank-agent,
   emits `golem-temp/agents/clank_agent_debug.wasm` whose exported world is the **agent** world (not
   `wasi:cli/run`). `golem server run`; `golem deploy --yes`.
4. **Invoke:** `golem agent invoke 'ClankAgent("demo")' run_line '"echo hi"'` → `"hi\n"`.
5. **Durability (the payoff):**
   - `... run_line '"echo first"'` → `"first\n"`; then `... run_line '"context show"'` on the same
     `"demo"` → output contains `clank$ echo first` + `first` (second invocation sees the first's
     transcript).
   - Isolation: `ClankAgent("other")` `context show` → empty (fresh Session per identity).
   - Shell-state carry (bonus, only if spike shows live struct persists): `run_line '"X=42"'` then
     `run_line '"echo $X"'` → `42` (the live Brush `Shell`, not just the transcript, is durable).
6. **Filesystem (Golem model):** `run_line '"mkdir -p /tmp/clank"'`, `... '"echo hi > /tmp/clank/a.txt"'`,
   `... '"cat /tmp/clank/a.txt"'` → `hi`, operating on the per-agent durable `std::fs` (the workflow the
   VFS issue said *should* work; it fails under `wasmtime --dir` semantics but should work under Golem).

## Risks

1. **[HIGHEST] Nested `block_on` under Golem's wstd executor** — the spike (Step 0) decides Option 1 vs
   the wstd-driver fallback. Everything gates on it; run it first.
2. **Recovery replay of non-deterministic commands** — oplog replay re-runs past `run_line`s; a command
   that read a clock/random reconstructs differently. Acceptable milestone-1 limitation; note in the
   design doc. Snapshotting (needs serde on `Session`) is the eventual fix, out of scope.
3. **`golem.yaml` template-key drift / `golem new` clobbering the workspace** — capture the real
   template block from a scratch scaffold; never scaffold over the root `Cargo.toml`.
4. **coreutils `close(1)` capture under Golem** — the wasm `run_uu` needs a writable preopen; Golem's
   per-agent `std::fs` should satisfy it, but confirm coreutils output is captured (not dropped) under
   Golem. Simplifying that hack onto real fs is **out of scope** for milestone 1.
5. **Branch hygiene (per AGENTS.md):** new feature branch off `dev` under one slug
   (e.g. `clank-golem-agent`) carrying its issue + design + plan docs; the `dev` branch is local-only
   (not pushed) — don't push without asking.

## Task ordering
1. Branch off `dev` (`clank-golem-agent`); add issue/design/plan docs per dev-docs workflow.
2. **Run Step 0 spike** → record the block_on verdict.
3. Feature-gate `mod wasm` in clank-shell; prove no `wasi:cli/run` leak + native/REPL unchanged.
4. Add `crates/clank-agent` + root `golem.yaml`; add workspace member + host profile.
5. `golem build` → `server run` → `deploy` → invoke + durability + filesystem checks.
6. Reconcile the design doc with the spike's empirical verdict.
