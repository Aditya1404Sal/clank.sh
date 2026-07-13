# WASM_CHANGES.md — third-party / upstream modifications in use for `wasm32-wasip2`

Audience: a maintainer who needs to know exactly what was forked, patched, or cfg-split to run
`clank.sh` inside a `wasm32-wasip2` component (the durable Golem agent) and why. Every entry below
was verified against the tree on branch `clank-golem-agent`. Native builds are unaffected by all of
it — every change is either a fork that keeps native behavior identical, or a `cfg`-gated branch.

There are exactly **two** third-party source forks (Brush, coreutils) and **one** `[patch.crates-io]`
block. Everything else is in-repo `cfg(target_arch = "wasm32")` code plus default-feature trimming.
No other crate is pinned to a git rev (verified: the only `git+` sources in `Cargo.lock` are the
Brush and coreutils forks).

---

## 1. The Brush fork (shell interpreter)

**WHAT:** `brush-core`, `brush-builtins`, `brush-parser` — the bash-compatible shell interpreter
clank embeds. Redirected from crates.io to a fork.

**WHERE:** root `Cargo.toml` `[workspace.dependencies]`, lines 54–56:

```
brush-core    = { git = "https://github.com/Aditya1404Sal/brush", rev = "0f4a89c" }
brush-builtins = { git = "https://github.com/Aditya1404Sal/brush", rev = "0f4a89c" }
brush-parser  = { git = "https://github.com/Aditya1404Sal/brush", rev = "0f4a89c" }
```

Fork branch `std-utils` (stacked on `wall-c-wasm-pipes`, branched from upstream `0300a84`); pinned
to exact commit `0f4a89c` (full hash `0f4a89cfbc57c85e08fd442240409d39ea7981bd` in `Cargo.lock`).
All three crates are one monorepo and are pinned in lockstep. The published crates would be
`brush-core 0.5 / brush-builtins 0.2 / brush-parser 0.4` (see the version strings in
`crates/clank-shell/Cargo.toml` lines 21–23, which the workspace git pin overrides).

**WHY wasip2 forced it — two independent reasons, both documented inline in `Cargo.toml` lines 42–53:**

**(a) File redirects — the `OpenFile::File` clone.** Published `brush-core 0.5.0` stores a redirect
target as `OpenFile::File(std::fs::File)` and duplicates it with `File::try_clone()`. `try_clone`
is `Unsupported` on `wasm32-wasip2`, so `echo > file` **silently discarded the write** on the agent.
Upstream `main` refactored `OpenFile::File` to hold an `Arc<File>` (clone becomes `Arc::clone` — no
syscall), which fixes redirects on wasip2, but that fix is not on crates.io. The fork carries it.
This is why the native capture path in `crates/clank-shell/src/session/mod.rs:1167` can write
`OpenFile::File(out_fd.into())` (an `Arc` conversion, not a `try_clone`), and why `effective_stdin`
in `crates/clank-shell/src/tools/coreutils.rs:198` matches on `OpenFile::File(_)`.

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

**WHERE:** root `Cargo.toml` — the **only** `[patch.crates-io]` block, lines 18–41 (verified: exactly
one `[patch` block in the file). Every entry points at:

```
git = "https://github.com/Aditya1404Sal/coreutils", branch = "wasip2-oscompat"
```

Resolved commit `35ecf24d7caa2202940a18ef61be5037776ecd36`. The `[patch]` block names 19 crates
(`uucore` + the 18 `uu_*`), but `Cargo.lock` resolves **20** `git+` source lines from the fork — the
20th is `uucore_procs`, pulled transitively because `uucore` depends on it from the same repo. The
`clank-shell` `Cargo.toml` still *names* the plain `"0.9"` versions (lines 39–59); the workspace
`[patch]` transparently redirects them to the fork.

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
`crates/clank-shell/src/tools/coreutils.rs` (`uu_builtin!` macro at line 260, registration list at 520).

---

## 3. In-repo `cfg(target_arch = "wasm32")` infrastructure

These are not forks — they are wasm-specific branches clank carries itself, alongside the native
branch, in this repo.

### 3a. In-memory output capture + current-thread runtime — `crates/clank-shell/src/session/mod.rs`

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
  multi-thread runtime from `main` (`crates/clank-shell/src/main.rs:11`, `Runtime::new()`).
- The `rt` field itself is `#[cfg(target_arch = "wasm32")]` (`session/mod.rs:283`).

### 3b. stdio binding via `__wasilibc_fd_renumber` — `crates/clank-shell/src/tools/coreutils.rs`

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

### 3c. HTTP transport seam — wstd (wasm) / reqwest (native)

The only HTTP client that works inside a Golem/wasip2 component is `wstd::http` (WASI-HTTP, recorded
in the oplog and replayed on recovery). Native uses `reqwest`. The seam is a small trait with two
`cfg`-gated implementations:

- **`crates/wcurl/src/lib.rs`** — `fetch` is `#[cfg(target_arch = "wasm32")]` wstd (`lib.rs:91`) vs
  `#[cfg(not(...))]` reqwest (`lib.rs:126`). Deps split in `crates/wcurl/Cargo.toml` lines 22–30.
- **`crates/waget/`** — same split (`crates/waget/Cargo.toml` lines 22–29).
- **`crates/clank-agent/src/mcp_http.rs`** — `WstdMcpHttp` implements the dual-target
  `clank_shell::mcp::client::McpHttp` seam using `wstd::http` (this agent crate is wasm-only, so it can
  link the Golem-host-only `wstd` client that `clank-shell` cannot). It additionally collects response
  headers because MCP needs `Mcp-Session-Id`.
- Both `wcurl`/`waget` pin `wstd = "=0.6.5"` to match `clank-agent` so the whole app resolves one
  `wstd`. `reqwest` is `default-features = false, features = ["rustls-tls"]` — see §6.

Note the load-bearing dispatch rule in `session/mod.rs:718–780`: `curl`/`wget`/`ask`/`mcp`/`grease` are
awaited directly at the Session layer, **not** through `execute`. `execute` drives Brush on the
nested `rt.block_on` (the "Wall C" shape), where the wstd WASI-HTTP reactor is not the running
executor; awaiting these one level under the Golem SDK's `wstd::block_on` keeps the reactor live.

### 3d. `wasi:cli/run` p3 entrypoint vs native blocking split

- **`crates/clank-shell/src/lib.rs`** — `mod wasm` is `#[cfg(all(target_arch = "wasm32", feature =
  "repl-driver"))]` (lib.rs:61); `pub mod native` is `#[cfg(not(target_arch = "wasm32"))]`
  (lib.rs:64).
- **`crates/clank-shell/src/wasm.rs`** — exports the `wasi:cli/run` component
  (`wasip3::cli::command::export!`, wasm.rs:17). The CLI world bindings are p3/0.3-async: root
  `Cargo.toml` pulls `wasip3 = "0.7"` and `wit-bindgen 0.57` with the `async` feature (lines 11–12),
  gated wasm-only-and-optional in `clank-shell/Cargo.toml` lines 90–93 behind the `repl-driver`
  feature (lines 87). stdout is a concurrent writer future joined via `futures::join!` (kept over
  `wit_bindgen::spawn`, which has no join handle — wasm.rs:9–11).
- **`crates/clank-shell/src/main.rs`** — native `main` builds `Runtime::new()` and blocks on
  `native::run()`; wasm `main` is empty (main.rs:26) because the wasm entrypoint is the exported
  component, and the canonical wasm build is `--lib`.
- **`crates/clank-shell/src/native.rs`** — the blocking `std::io` REPL loop (`native::run`,
  native.rs:10). Also hosts `ask repl`, which is native-only (the durable agent cannot block on human
  input between turns).
- **The Golem agent** (`crates/clank-agent`) links `clank-shell` with `default-features = false`
  (`clank-agent/Cargo.toml:23`) to **drop** `repl-driver`, so the agent's `golem:agent` world does
  not clash with a second `wasi:cli/run` export (comment at `clank-shell/Cargo.toml:82–86`).

---

## 4. Hand-rolled / in-process text & data tools

**Root cause is the same for all of them:** wasip2 has **no process spawn** — you cannot fork/exec
`grep`, `sed`, `jq`, `awk`, `find`, `stat`, etc. Every "external" text tool must therefore be Rust
running *inside* the component. Two sub-cases:

**(i) Library-backed builtins** — where a dual-target, wasm-buildable Rust crate exists, clank wraps
it rather than reimplementing. In `crates/clank-shell/src/tools/texttools.rs` (registration at
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
- `crates/clank-shell/src/tools/awk.rs` — no Rust awk crate builds for wasm32-wasip2 (frawk/zawk
  hard-require the cranelift/LLVM **JIT** backends); this is a from-scratch lexer + recursive-descent
  parser + tree-walking evaluator (awk.rs:1–4).
- `crates/clank-shell/src/tools/find.rs` — uutils' findutils is bin-only with a C `onig` dependency that
  doesn't build for wasm32-wasip2; hand-written subset of the common predicates (find.rs:1–4).
- `crates/clank-shell/src/tools/stat.rs` — wasip2 has no `stat(2)` struct (no inode, uid/gid, mode bits,
  block counts); prints `-` for fields the sandbox cannot know rather than inventing them
  (stat.rs:1–4).

(The prompt framed all seven of grep/jq/sed/awk/diff/patch/file as reimplementations "because no
wasm-buildable crate exists"; that is only literally true for the group (ii) tools. grep/jq/diff/
patch/file *do* wrap wasm-buildable library crates — they are in-process because there is nothing to
fork/exec on wasm, not because no crate exists.)

---

## 5. `golem-stuff/` is reference-only, not a build input

`golem-stuff/golem/` is a **vendored checkout of the Golem repo** kept for reference (it has its own
`Cargo.toml` and is a separate workspace). It is **not** part of clank's build:
- The root workspace `members` list (Cargo.toml:3) contains only `crates/*`; `golem-stuff` is not a
  member and is referenced **nowhere** in any workspace/crate `Cargo.toml` (verified by grep).
- The actual Golem SDK, `golem-rust`, resolves from **crates.io 2.1.0**
  (`crates/clank-agent/Cargo.toml:16`; `Cargo.lock`: `golem-rust 2.1.0`,
  `source = registry+https://github.com/rust-lang/crates.io-index`).

(The sibling `golem-temp/` directory is likewise not a workspace member.)

---

## 6. What is NOT forked or pinned

For completeness, so a maintainer doesn't go hunting for phantom patches:

- **`getrandom`, `ring`, `tokio`, `wstd`** are plain crates.io dependencies — no fork, no git rev, no
  `[patch]`. Verified: `Cargo.lock` shows `ring 0.17.14`, `getrandom 0.2.17`/`0.4.3`, all
  `registry+…crates.io`. The only `git+` sources in the lock are the Brush and coreutils forks.
- Wasm compatibility for these is handled by **`cfg`-gated deps** (the `[target.'cfg(...)']` blocks
  in the crate `Cargo.toml`s) and by **`default-features = false` trimming**, not by patching:
  - `ed25519-dalek = { version = "2", default-features = false }`
    (`clank-shell/Cargo.toml:77`) — **verify-only** (no keygen/signing), which drops the
    `rand_core`/`getrandom` requirement so grease signature verification builds clean on wasm. (The
    signing side is a **dev-dependency** only, `clank-shell/Cargo.toml:107`, never in the agent
    build.)
  - `reqwest = { version = "0.12", default-features = false, features = ["rustls-tls"] }`
    (native-only, `wcurl`/`waget` `Cargo.toml`) — pure-Rust rustls TLS, no system libcurl/OpenSSL.
  - `chrono` with `default-features = false, features = ["clock"]` (`clank-shell/Cargo.toml:69`).

---

## Comment consistency

The Brush fork's "Wall C" work retired the old "wasm pipelines are a limitation" caveat. Two module
docs (`session/mod.rs` and `wasm.rs`) still carried it and were corrected to match this document: pipelines
and `$(...)` **work** on wasm (the inline-sequential `OpenFile::Stream` path). What genuinely remains
unavailable is spawning real external processes — wasip2 has no process spawn — which is a distinct
constraint from pipelines and is described as such.
