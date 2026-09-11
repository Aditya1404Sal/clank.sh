# WASM_CHANGES.md — third-party / upstream modifications in use for `wasm32-wasip2`

Audience: a maintainer who needs to know exactly what was forked, patched, or cfg-split to run
`clank.sh` inside a `wasm32-wasip2` component (the durable Golem agent) and why. Every entry below
was verified against the tree on branch `main-rc-1`. Native builds are unaffected by all of
it — every change is either a fork that keeps native behavior identical, or a `cfg`-gated branch.

For the current, *generated* inventory of every patched crate — pin kind, resolved rev, and the
ones that arrive transitively and are named in no `Cargo.toml` — see
[`docs/FORKS.md`](FORKS.md). That table is regenerated from `Cargo.toml`/`Cargo.lock` by
`dev-tools/fork-inventory` and is CI-gated (`check-forks`), so it cannot drift silently the way an
earlier version of this paragraph did: it asserted "exactly two forks" and "no other crate is
pinned to a git rev" for months after a third, `golemcloud/wit-bindgen`, had been resolving into
`Cargo.lock` transitively (via the `golem-rust` path dependency) the whole time. This file keeps
what a generated table cannot produce: *why* each fork exists.

---

## 1. The Brush fork (shell interpreter)

**WHAT:** `brush-core`, `brush-builtins`, `brush-parser` — the bash-compatible shell interpreter
clank embeds. Redirected from crates.io to a fork.

**WHERE:** `[workspace.dependencies]` in root `Cargo.toml` — see [`docs/FORKS.md`](FORKS.md) for the
exact pin and resolved rev (`check-forks` in CI keeps that table honest; the literal rev is not
repeated here so this file cannot go stale the way it previously did).

Fork branch `std-utils` (stacked on `wall-c-wasm-pipes`, branched from upstream `0300a84`). All
three crates are one monorepo and are pinned in lockstep. The published crates would be
`brush-core 0.5 / brush-builtins 0.2 / brush-parser 0.4` (see the version strings in
`crates/clank-core/Cargo.toml`, which the workspace git pin overrides).

**WHY wasip2 forced it — two independent reasons, both documented inline in `Cargo.toml` lines 42–53:**

**(a) File redirects — the `OpenFile::File` clone.** Published `brush-core 0.5.0` stores a redirect
target as `OpenFile::File(std::fs::File)` and duplicates it with `File::try_clone()`. `try_clone`
is `Unsupported` on `wasm32-wasip2`, so `echo > file` **silently discarded the write** on the agent.
Upstream `main` refactored `OpenFile::File` to hold an `Arc<File>` (clone becomes `Arc::clone` — no
syscall), which fixes redirects on wasip2, but that fix is not on crates.io. The fork carries it.
This is why the native capture path in `crates/clank-core/src/session/mod.rs:1167` can write
`OpenFile::File(out_fd.into())` (an `Arc` conversion, not a `try_clone`), and why `effective_stdin`
in `crates/clank-core/src/tools/coreutils.rs:198` matches on `OpenFile::File(_)`.

**(b) "Wall C" — pipelines and `$(...)` without OS pipes or threads.** `std::io::pipe()` is
unsupported on wasip2 and there is no blocking thread pool. Upstream Brush wires pipeline stages and
command substitution through OS pipes + `tokio::spawn` / `spawn_blocking`. On wasm the fork instead
runs pipeline stages **and** `$(...)` substitution through an in-memory `OpenFile::Stream`-backed
pipe, executed **inline-sequentially**: the producer stage completes and drops its writer, which
gives the reader a clean EOF. No OS pipes, no task spawning. Native behavior is unchanged.

**ACTIVE:** Yes — it is the shell interpreter for both targets; the wasm agent literally cannot run
`echo > file` or `a | b` correctly without it.

---

## 2. The coreutils fork (`uucore` + every `uu_*` command crate)

**WHAT:** `uucore` plus all 18 `uu_*` command crates clank registers as builtins
(`cat ls wc head sort mkdir rm mv cp env cut tr uniq tail tee touch sleep printf`).

**WHERE:** the **only** `[patch.crates-io]` block in root `Cargo.toml` — see
[`docs/FORKS.md`](FORKS.md) for the exact pin and resolved rev. The block names 19 crates (`uucore`
+ the 18 `uu_*`), but `Cargo.lock` resolves a 20th `git+` package from the same fork — `uucore_procs`,
pulled in transitively because `uucore` itself depends on it. The `clank-core` `Cargo.toml` still
*names* the plain `"0.9"` versions; the workspace `[patch]` transparently redirects them to the fork.

**WHY wasip2 forced it:** upstream `uucore 0.9` uses the **unstable `wasip2` std feature** and fails
to build on the target at all. The fork adds:
- a **stable `OsStr` encoded-bytes shim** (replacing the unstable-feature path), and
- an **empty-argv guard** in `uucore`, and
- a **`set_permissions` skip under wasi** in `uu_cp` (wasip2 has no POSIX mode bits to copy).

**WHY every `uu_*` crate must be patched, not just `uucore`** (the rationale is spelled out in the
`Cargo.toml` comment, lines 19–22): the published `uu_*` command crates only *share* the patched
`uucore` transitively. A fix that lives **inside a command crate** (e.g. the `uu_cp` `set_permissions`
skip) is only picked up when that command crate *itself* is sourced from the fork. Patching only
`uucore` would leave `cp` on the crates.io copy without the wasi fix. So each of the 18 command
crates clank registers is patched individually.

**ACTIVE:** Yes — these are the internal `cat`/`ls`/`cp`/… builtins registered in
`crates/clank-core/src/tools/coreutils.rs` (`uu_builtin!` macro at line 260, registration list at 520).

---

## 3. In-repo `cfg(target_arch = "wasm32")` infrastructure

These are not forks — they are wasm-specific branches clank carries itself, alongside the native
branch, in this repo.

### 3a. In-memory output capture + current-thread runtime — `crates/clank-core/src/session/mod.rs`

- **`BufSink`** (`session/mod.rs:1921`): an `Arc<Mutex<Vec<u8>>>` implementing
  `brush_core::openfiles::Stream`. Its fd-returning trait methods are `#[cfg(unix)]` upstream, so on
  wasm only `Read`/`Write`/`clone_box` are needed. Whole struct is `#[cfg(target_arch = "wasm32")]`.
- **`OpenFile::Stream` capture** (`session/mod.rs:1194` `execute`, wasm variant): wasm has no anonymous
  temp file to redirect into, so Brush's stdout/stderr fds are set to `OpenFile::Stream(BufSink…)`
  (lines 1200/1204) and the buffers are drained after the run. The **native** `execute` instead
  captures into an anonymous temp file via `OpenFile::File` (the path that also feeds real external
  programs).
- **Owned current-thread tokio runtime** (`session/mod.rs:284` field `rt`, built at `session/mod.rs:293`
  with `Builder::new_current_thread()`): wasip2 has no threads, so Brush's internal async is driven
  on an owned current-thread runtime, `block_on`'d at `session/mod.rs:1211`. Native uses the ambient
  multi-thread runtime from `main` (`crates/clank-core/src/main.rs:11`, `Runtime::new()`).
- The `rt` field itself is `#[cfg(target_arch = "wasm32")]` (`session/mod.rs:283`).

### 3b. stdio binding via `__wasilibc_fd_renumber` — `crates/clank-core/src/tools/coreutils.rs`

`uu_*` `uumain` functions write to the process-global `std::io::stdout()`/`stderr()`, so their output
must be redirected onto Brush's assigned `OpenFile`s.

- **Native `run_uu`** (`coreutils.rs:31`): saves fd 1/2 with `libc::dup`, points them at Brush's
  target with `libc::dup2`, runs `uumain`, then restores. Serialized by `FD_SWAP_LOCK`
  (`coreutils.rs:27`) because the swap targets process-global fds.
- **Wasm `run_uu`** (`coreutils.rs:99`): **there is no `dup2` symbol in the wasm32-wasip2 libc.** The
  code declares `extern "C" fn __wasilibc_fd_renumber(fd, newfd)` (`coreutils.rs:111`) — wasi-libc's
  descriptor-renumber primitive, which atomically *moves* a descriptor onto a target number. fd 0 is
  bound to a staged-stdin file (`/tmp/.clank-uu-stdin`), fd 1/2 to separate capture files
  (`/tmp/.clank-uu-out`, `/tmp/.clank-uu-err`), which are read back and replayed into
  `context.stdout()`/`context.stderr()` so the two streams stay distinct. The module doc (lines
  85–97) records **why `dup2`/renumber and not close-then-reopen**: "the next open claims the lowest
  free fd" is *not* a dependable invariant mid-session (observed live: stdin landing on fd 1, stderr
  on fd 0). After a call, fds 0–2 intentionally stay bound to the staging/capture files as stable
  anchors for the next call.
- **The "never read the real wasip2 stdin" invariant** — `effective_stdin` (`coreutils.rs:193`,
  wasm-only) and `tool_stdin` (`coreutils.rs:229`): a durable agent has no interactive stdin, and
  calling `input-stream.blocking-read` on the real wasip2 stdin resource **TRAPS the whole component
  and wedges the agent instance**. So `effective_stdin` returns the piped/redirected source when
  Brush assigned one (`OpenFile::File`/`PipeReader`/`Stream`) and `std::io::empty()` for the default
  `OpenFile::Stdin` — it never touches the real stdin resource. Native `tool_stdin` just hands over
  `context.stdin()`.

### 3c. HTTP transport seam — wasi-fetch (wasm) / reqwest (native)

Inside a Golem component, HTTP means WASI-HTTP (recorded in the oplog and replayed on recovery).
`wasi-fetch` is the client: a reqwest-shaped wrapper over the wasip3 bindings, pinned `=0.2.0` as
upstream golem pins it in its own agent components. Native uses `reqwest`. The seam is `cfg`-gated:

- **`utilities/whttp/src/lib.rs`** — `fetch_once` is the whole transport seam: a
  `#[cfg(target_arch = "wasm32")]` wasi-fetch arm and a `#[cfg(not(...))]` reqwest arm, with the
  redirect loop and `Location` resolution living *above* it so both targets behave identically.
  `wcurl`/`waget` only parse flags and format output; they hold no HTTP of their own.
- **`crates/clank-embed/src/mcp_http.rs`** — `WasiFetchMcpHttp` implements the dual-target
  `clank_core::mcp::client::McpHttp` seam (clank-embed is wasm-only, so it can link a
  Golem-host-only client that `clank-core` cannot). It additionally collects response headers
  because MCP needs `Mcp-Session-Id`.
- **`crates/clank-embed/src/ask_provider.rs`** — the same client behind `ask`.
- `reqwest` is `default-features = false, features = ["rustls-tls"]` — see §6.

**This replaced `wstd`, which the current SDK broke outright.** wstd's client resolves its reactor
from a thread-local that only `wstd::block_on` installs; golem-rust now drives agent methods with
wit-bindgen's async runtime and no longer depends on wstd at all, so every outbound request panicked
`Reactor::current must be called within a wstd runtime` and trapped the agent. `wasi-fetch` holds no
runtime state of its own — its futures are driven by whatever executor polls them.

Note the load-bearing dispatch rule in `session/mod.rs`: `curl`/`wget`/`ask`/`mcp`/`grease` are
awaited directly at the Session layer, **not** through `execute`. `execute` drives Brush on the
nested `rt.block_on` (the "Wall C" shape); a WASI-HTTP future polled by that tokio runtime is never
woken, because nothing there performs the component-model wait. Awaiting these one level under the
Golem SDK's own executor is what makes them complete.

### 3d. Native entrypoint vs. the wasm component export

**Updated 2026-09-11 — the `wasi:cli/run` p3 driver described in earlier revisions of this section is
gone, not just relocated.** Recorded here for anyone who finds a stale reference to `wasm.rs` or the
`repl-driver` feature in git history, an older doc, or a code comment written before this date.

- **What used to exist, and was removed as dead.** `crates/clank-core/src/wasm.rs` used to export a
  standalone `wasi:cli/run` component (p3/0.3-async CLI-world bindings, gated behind a `repl-driver`
  Cargo feature and a `mod wasm` in `lib.rs`). It was deleted outright: every crate in the workspace
  that depends on `clank-core` (`clank-cli`, `clank-embed`, `clank-conformance`, `grease-tool`) builds
  it with `default-features = false`, so no crate in the workspace ever enabled `repl-driver` or
  linked the artifact it built. It was also latently broken — it discarded `pending_prompt`, so a
  confirm-gated command would have printed its question and then wedged the session had the driver
  ever been revived as-is. `clank-core`'s `[lib]` is now a plain `crate-type = ["rlib"]` (see the
  comment in `clank-core/Cargo.toml`); consult git history to resurrect either if a real use case
  appears.
- **The two entrypoints that actually exist today are elsewhere.** The **native** binary is
  `crates/clank-cli/src/main.rs` — it builds a multi-thread tokio `Runtime` and blocks on
  `clank_native::run()`. The **wasm** artifact is the separate `clank-agent` crate
  (`crates/clank-agent`), a `cdylib` that exports the Golem `golem:agent` world via `golem-rust`'s
  `export_golem_agentic` feature (enabled only on that leaf crate, since a component must carry
  exactly one such export). `clank-agent` was never the `wasi:cli/run` export `wasm.rs` used to build,
  and dropping `wasm.rs` changed nothing about how `clank-agent` is exported — the two were always
  independent, which is exactly why the workspace could delete one without touching the other.
- **`crates/clank-native/src/run.rs`** — the native REPL loop (`clank_native::run`, dispatching to
  `run_interactive`/`run_plain`), `inject_native_providers`, and `run_repl` (`ask repl`, native-only —
  the durable agent cannot block on human input between turns). This is where `clank-core`'s old
  `native.rs` moved *to*, in full, along with every `reqwest`-backed provider implementation
  (Anthropic, the OpenAI-compatible family, MCP HTTP, the Golem cluster REST client) — `clank-core`
  itself now carries no `reqwest`/`reedline`/`crossterm`/`nu-ansi-term` dependency at all (see §6).

---

## 4. Hand-rolled / in-process text & data tools

**Root cause is the same for all of them:** wasip2 has **no process spawn** — you cannot fork/exec
`grep`, `sed`, `jq`, `awk`, `find`, `stat`, etc. Every "external" text tool must therefore be Rust
running *inside* the component. Two sub-cases:

**(i) Library-backed builtins** — where a dual-target, wasm-buildable Rust crate exists, clank wraps
it rather than reimplementing. In `crates/clank-core/src/tools/texttools.rs` (registration at
texttools.rs:79):
- `jq` → wraps `jaq-core` / `jaq-json` (texttools.rs:71, 122–125)
- `grep` → wraps the `grep` crate, ripgrep's library (texttools.rs:72, 373–374)
- `diff` / `patch` → `diffy` + `similar`
- `file` → the `infer` crate
- `sed` → hand-written command parser over the `regex` crate

The module doc calls these "small POC wrappers over library APIs" and notes stdin/pipeline fidelity
still leans on the fd machinery (texttools.rs:1–6).

**(ii) Genuinely hand-rolled from scratch** — where **no wasm-buildable crate exists**, clank ships a
from-scratch implementation. Each file's module doc states the reason:
- `crates/clank-core/src/tools/awk.rs` — no Rust awk crate builds for wasm32-wasip2 (frawk/zawk
  hard-require the cranelift/LLVM **JIT** backends); this is a from-scratch lexer + recursive-descent
  parser + tree-walking evaluator (awk.rs:1–4).
- `crates/clank-core/src/tools/find.rs` — uutils' findutils is bin-only with a C `onig` dependency that
  doesn't build for wasm32-wasip2; hand-written subset of the common predicates (find.rs:1–4).
- `crates/clank-core/src/tools/stat.rs` — wasip2 has no `stat(2)` struct (no inode, uid/gid, mode bits,
  block counts); prints `-` for fields the sandbox cannot know rather than inventing them
  (stat.rs:1–4).

(The prompt framed all seven of grep/jq/sed/awk/diff/patch/file as reimplementations "because no
wasm-buildable crate exists"; that is only literally true for the group (ii) tools. grep/jq/diff/
patch/file *do* wrap wasm-buildable library crates — they are in-process because there is nothing to
fork/exec on wasm, not because no crate exists.)

---

## 5. What is NOT forked or pinned

For completeness, so a maintainer doesn't go hunting for phantom patches:

- **`getrandom`, `ring`, `tokio`, `wasi-fetch`** are plain crates.io dependencies — no fork, no git rev, no
  `[patch]`. Verified: `Cargo.lock` shows `ring 0.17.14`, `getrandom 0.2.17`/`0.4.3`, all
  `registry+…crates.io`, not git-sourced. (For the complete, generated list of every `git+` source
  in `Cargo.lock` — including the ones no `Cargo.toml` names directly — see
  [`docs/FORKS.md`](FORKS.md).)
- Wasm compatibility for these is handled by **`cfg`-gated deps** (the `[target.'cfg(...)']` blocks
  in the crate `Cargo.toml`s) and by **`default-features = false` trimming**, not by patching:
  - `ed25519-dalek = { version = "2", default-features = false }`
    (`clank-core/Cargo.toml:77`) — **verify-only** (no keygen/signing), which drops the
    `rand_core`/`getrandom` requirement so grease signature verification builds clean on wasm. (The
    signing side is a **dev-dependency** only, `clank-core/Cargo.toml:107`, never in the agent
    build.)
  - `reqwest = { version = "0.12", default-features = false, features = ["rustls-tls"] }`
    (native-only, `wcurl`/`waget` `Cargo.toml`) — pure-Rust rustls TLS, no system libcurl/OpenSSL.
  - `chrono` with `default-features = false, features = ["clock"]` (`clank-core/Cargo.toml:69`).

---

## 6. The supply-chain risk that applies to every git fork

**The coreutils and brush forks live in a single maintainer's personal GitHub account**
(`Aditya1404Sal/coreutils`, `Aditya1404Sal/brush`). A delete, a rename, a force-push that drops the
pinned object, or an account change breaks **every build of clank on every machine without a warm git
cache** — CI, a fresh clone, a `golem deploy`. Cargo fails to resolve; it does not fall back.

Two things reduce that today, and one does not:

- **Reduces it:** both are pinned to an exact `rev`, not a branch. A branch pin silently advances to
  branch-tip on a fresh resolve (audit P2-7); a rev pin is reproducible from source control alone,
  and a force-push that *keeps* the object still resolves. ([`docs/FORKS.md`](FORKS.md) now reports
  pin kind per crate, so a branch pin cannot hide — it currently flags four, all `wit-bindgen`,
  arriving transitively through the dev SDK.)
- **Reduces it:** `Cargo.lock` records the full 40-char hash, so the exact object is named even where
  `Cargo.toml` abbreviates.
- **Does NOT reduce it:** nothing mirrors these repositories. The `Cargo.toml` comment says "mirror
  the fork so a delete/force-push can't break the build" — an instruction that has not been carried
  out. **This is the single highest-leverage supply-chain fix available to this project**, and it
  costs one `git push --mirror` per fork to an org-owned remote plus a one-line URL change.

The two vendored forks (`reedline-fork/`, `crossterm-fork/`) have none of this exposure — the source
is in-tree and committed. Their cost is the opposite: nothing tells you when upstream moves, so they
go stale silently.

---

## 7. The `golem` CLI fork — not a dependency, and why it is still stuck

`golem-stuff/golem` is a **nested git clone** (its own repo and remotes, gitignored by clank) of
`golemcloud/golem`, on branch `clank-connect-patch`. It builds the `golem` CLI *binary* supplying
`golem agent shell` — the command that drives a deployed clank agent interactively. **Nothing in
clank's `Cargo.toml` points at it**, which is exactly why the generated `FORKS.md` cannot see it: a
manifest-driven table structurally cannot report a fork that is not a Cargo dependency. (On
`main-rc-1` the same clone additionally supplies the dev Golem SDK *via path deps*, which **is** a
build dependency — see `DEV_SDK_CHANGES.md`.)

**The upstreaming attempt.** [PR #3700](https://github.com/golemcloud/golem/pull/3700) was opened
2026-07-16 17:19:08Z and **closed 19 seconds later, at 17:19:27Z**, by a bot: the project requires
pull-request authors to be vouched, and the author was not on the list. The CLA *was* signed. **The
PR was never reviewed on technical merit** — no maintainer read it. So the blocker is not code
quality, an API objection, or a design disagreement.

The unblock is documented in the repo's own `.github/VOUCHED.td`: a maintainer comments
`vouch @Aditya1404Sal` on any issue. **That is the whole gate.** Worth pursuing — every rebase of
this fork costs real work, and that cost recurs for as long as the patch lives out-of-tree.

**Why rebases hurt the way they do:** only five files are *modified*; everything else is new
(`interactive_shell.rs` ~1100 lines, `tests/agent_shell.rs`, the `test-components/agent-shell/`
component). New files cannot textually conflict, so a rebase is rarely a merge-conflict problem — it
is **compile-level API drift**, surfacing as build errors after a clean rebase rather than as
`<<<<<<<` markers. The 2026-09-11 rebase onto `f5a3d29b9` is the case in point: two conflicts, then
six identifier/signature changes from upstream's `worker`→`agent` rename.

**Rebase procedure:**

```bash
cd golem-stuff/golem
git branch -f clank-connect-patch-prerebase-<date> HEAD   # backup ref, always
git fetch upstream
git rebase upstream/main
cargo build -p golem-cli                                   # the real gate
git push --force-with-lease origin clank-connect-patch     # origin ONLY, never upstream
```

---

## 8. Maintenance checklist

When bumping any fork:

1. **Bump the `rev`, never point at a branch** (audit P2-7). `check-forks` reports pin kind, so a
   branch pin is visible in review rather than buried in `Cargo.lock`.
2. Regenerate the inventory: `cargo run -p fork-inventory` (CI's `check-forks` fails otherwise), and
   update the *rationale* here if the reason for the fork changed.
3. `cargo clean` first if `target/` holds wasm artifacts — a stale one produces "failed to parse
   WebAssembly module", which reads like a toolchain regression and is not one. Note
   `scripts/lib/golem-json.sh`'s freshness check now covers the related (and more common) case where
   `golem build` silently skips a component whose path dependency changed.
4. Verify on **both** targets: `cargo test -- --test-threads=1`, then `scripts/golem-e2e.sh`. A fork
   fix for wasm that breaks native is the failure mode these pins exist to prevent.
