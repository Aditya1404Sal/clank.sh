# wasip2 constraints — the missing primitives and what each one forces

clank's wasm target is `wasm32-wasip2`, running as a Golem component. wasip2 is not a general-purpose
POSIX target: it deliberately omits a set of primitives that a conventional Unix shell leans on
constantly. Every one of them is missing for the same underlying reason — a WASI *component* has no
kernel underneath it granting process, signal, or raw-descriptor control — and each missing primitive
forces a specific, identifiable design choice elsewhere in clank. This page is the index from "what's
missing" to "what that made us build"; for exhaustive per-fork detail (exact pins, exact patched
functions), see [`docs/WASM_CHANGES.md`](../WASM_CHANGES.md) and the generated
[`docs/FORKS.md`](../FORKS.md) — this page does not duplicate either, it cross-references them.

## The table

| Missing primitive | Forces | Where it lands |
|---|---|---|
| Process spawn (no `fork`/`exec`) | Every command clank offers must be Rust code running *inside* the component, not a spawned external program — hence the Brush fork (the shell engine itself) and the coreutils/text-tool forks (the commands it runs) | [`docs/WASM_CHANGES.md`](../WASM_CHANGES.md) §2, §4 |
| `pipe(2)` (no OS pipe) | Brush's own pipeline and `$(...)` machinery, which upstream wires through OS pipes plus `tokio::spawn`/`spawn_blocking`, is replaced in the fork by an in-memory `OpenFile::Stream`-backed pipe run inline-sequentially | [`docs/WASM_CHANGES.md`](../WASM_CHANGES.md) §1(b); see also [`wall-c.md`](wall-c.md)'s closing note |
| Threads | Brush's internal async (which still needs *a* tokio runtime even with no OS threads) runs on an owned **current-thread** runtime built and `block_on`'d inside [`Session::execute`](../../crates/clank-core/src/session/mod.rs), instead of native's ambient multi-thread runtime; every thread-local slot in the codebase (`ACTIVE_TRANSCRIPT`, the proc-table/transcript install slots) is single-occupancy on wasm by construction, which is a simplifying assumption, not a limitation, there | [`docs/WASM_CHANGES.md`](../WASM_CHANGES.md) §3a; [`lib.rs`](../../crates/clank-core/src/lib.rs) |
| `dup2` | uutils' `uumain` functions write to the process-global `std::io::stdout()`/`stderr()`, which must be redirected onto Brush's assigned fds around each call; native does this with real `libc::dup`/`dup2`, but wasi-libc exposes no `dup2` symbol at all, so the wasm path uses `__wasilibc_fd_renumber` instead — an atomic descriptor-*move*, not a duplicate-and-swap | [`docs/WASM_CHANGES.md`](../WASM_CHANGES.md) §3b; [`tools/coreutils.rs`](../../crates/clank-core/src/tools/coreutils.rs) |
| Blocking/interactive stdin read | A durable agent has no real terminal on the other end of wasip2's stdin resource; calling `input-stream.blocking-read` on it **traps the whole component and wedges the agent instance** — not just the one call. clank's stdin plumbing is therefore built to never touch that resource at all | [`docs/WASM_CHANGES.md`](../WASM_CHANGES.md) §3b; [`tools/coreutils.rs`](../../crates/clank-core/src/tools/coreutils.rs) |
| `stat(2)` | No inode, uid/gid, mode bits, or block counts exist to report. clank's `stat` builtin prints real values for what the sandbox *can* know (size, type, mtime/atime, birth where the host supports it) and a literal `-` for every field it cannot, rather than inventing plausible-looking numbers | [`tools/stat.rs`](../../crates/clank-core/src/tools/stat.rs) |
| `nix` (the crate) | Brush's own `nix` usage for Unix process operations is compiled out entirely on wasm — clank replaces the whole process-execution layer at that boundary, and `nix` is gated `cfg(unix)` inside `brush-core`, so the wasm32-wasip2 build pulls zero `nix` transitively | [`crates/clank-core/Cargo.toml`](../../crates/clank-core/Cargo.toml) |

## Expanding each row

**No process spawn.** This is the root constraint the other rows mostly trace back to. A conventional
Unix shell is thin because it delegates almost everything to `fork`+`exec`: `grep`, `sed`, `jq`, real
`stat`, even sub-shells are just other programs the kernel starts. wasip2 grants none of that — there
is no kernel underneath a component to ask. Every command clank exposes must therefore already be
Rust code linked into the one component, decided at *compile* time, not resolved at *run* time from a
`$PATH` full of binaries. This is why the shell engine itself is a patched fork of Brush rather than a
wrapper around a system shell, and why every coreutils-shaped command (`cat`, `ls`, `wc`, `cp`, …) is a
real `uutils` crate linked in and fd-rebound around, not a spawned `/bin/cat`. It also forces the
hand-rolled tools documented in `docs/WASM_CHANGES.md` §4 — `awk`, `find`, `stat` — where no
wasm-buildable crate existed to link in either, so clank wrote them from scratch.

**No `pipe(2)`.** Once every command is in-process Rust rather than a spawned program, a "pipe" can no
longer be two file descriptors connected by the kernel. The forked Brush instead gives a pipeline stage
an in-memory buffer (`OpenFile::Stream`) to write into and read from, and runs the stages
**inline-sequentially** rather than concurrently: the producer stage runs to completion and drops its
writer, which is what hands the reader a clean EOF. This is a genuinely different execution model from
upstream Brush's OS-pipe-plus-concurrent-tasks pipeline — see `wall-c.md` for why clank's *own*
commands (curl, ask, …) have a further, separate restriction on where in such a pipeline they may
appear.

**No threads.** wasip2 is single-threaded. Brush still needs an async runtime to drive its own internal
`tokio::spawn`/`spawn_blocking` calls even with no OS threads underneath, so `Session::execute` builds
and owns a current-thread tokio `Runtime` on wasm (a field that only exists under
`cfg(target_arch = "wasm32")`) and drives Brush with `block_on` on it. This is also *why* thread-local
state throughout the codebase is safe on wasm in a way it structurally cannot be relied on for native:
`lib.rs`'s `ACTIVE_TRANSCRIPT` slot (the mechanism that lets `$(context show)` reach the session
transcript from inside a Brush builtin) is single-occupancy per OS thread, so on wasm — exactly one
thread, always — it always resolves; on native, a `$()`/pipeline stage that Brush happens to run on a
different worker thread of the multi-thread runtime genuinely cannot see it, and errors honestly
instead of silently reading the wrong session's transcript.

**No `dup2`.** Redirecting a `uu_*` command's output away from the process's real stdout/stderr and
into whatever Brush's fd table currently points at (a pipe buffer, a redirect target, the terminal)
needs some way to rebind a low-numbered fd. Native has `libc::dup`/`dup2`. wasi-libc's C surface
doesn't define `dup2` at all, so the wasm implementation calls `__wasilibc_fd_renumber` — wasi-libc's
own descriptor-renumber primitive, which atomically *moves* a descriptor onto a target number rather
than duplicating it — binding fd 0 to a staged-stdin file and fds 1/2 to separate capture files. The
module doc in `tools/coreutils.rs` records why this uses renumber-to-fixed-targets rather than
close-then-reopen: "the next open claims the lowest free fd" is not a dependable invariant mid-session
(observed live: stdin landing on fd 1, stderr on fd 0).

**No blocking/interactive stdin read.** This is the sharpest trap in the list: it's not merely
unsupported, it's actively dangerous. A durable Golem agent instance has no human sitting at a
terminal between invocations, so wasip2's real stdin resource never has data queued and never signals
EOF either — calling `input-stream.blocking-read` on it hangs the host call, which **traps the
component and wedges the whole agent instance**, not just the command that tried to read. clank's
wasm-only `effective_stdin`/`tool_stdin` helpers in `tools/coreutils.rs` are built around never letting
that resource be touched: they return whatever Brush already assigned (a piped/redirected
`OpenFile::File`/`PipeReader`/`Stream`) when one exists, and `std::io::empty()` — never the real stdin
handle — for the default `OpenFile::Stdin` case.

**No `stat(2)`.** There is no inode table, no uid/gid, no permission bits, no block count in a wasip2
component's view of its filesystem. clank's `stat` builtin (`tools/stat.rs`) treats this as an honest
constraint rather than something to paper over: it reports real values for the fields the host *can*
supply (size, type, timestamps) and a literal `-` for every field it structurally cannot know, matching
the shell's general "honest constraints over false surfaces" stance rather than inventing plausible
numbers. (This is also why `uu_stat` — upstream uutils' own `stat`, whose formatting core is
`MetadataExt`-unix throughout — was rejected outright rather than patched.)

**No `nix`.** Brush upstream uses the `nix` crate for real Unix process operations — signals, process
groups, and similar. clank replaces that entire layer (there is no process to signal or group on
wasip2), so no `nix` code needs to exist on the wasm side at all; `brush-core` itself gates its `nix`
usage behind `cfg(unix)`, so the wasm32-wasip2 build never links it. This is a "the constraint made the
dependency vanish" case rather than a fork or a workaround.

## What this table doesn't cover

The concrete list of every forked/patched crate — exact repository, pin kind, resolved revision, and
which packages arrive only transitively — is generated, not hand-maintained, and lives in
[`docs/FORKS.md`](../FORKS.md) (`dev-tools/fork-inventory`, regenerated and CI-gated). The vendored
native-only forks (`reedline-fork`, `crossterm-fork`) are unrelated to wasip2 — they exist for a native
terminal-latency reason and never enter the wasm build at all; see `docs/FORKS.md` §3.
