//! Internal coreutils: uutils (`uu_*`) crates registered as clank builtins, so `cat`/`ls`/etc.
//! run **inside** the component instead of forking host programs — matching the README's
//! "core commands resolved internally, no fork/exec" model.
//!
//! Each `uu_*::uumain(args) -> i32` writes to `std::io::stdout()` directly, so to make its
//! output compose with brush's fd table (pipes, redirections, the transcript capture) we
//! redirect the real fd 1/2 onto the `OpenFile`s brush assigned for the command, run `uumain`,
//! then restore them. On wasm there is no restore-based swap: fds 0-2 are rebound per call via
//! `__wasilibc_fd_renumber` to staging/capture files (stdin staged in, stdout and stderr captured
//! to separate files and replayed into Brush's stdout/stderr so the streams stay distinct).
//!
//! `uucore` is patched for `wasm32-wasip2` via `[patch.crates-io]` in the workspace root.

use std::io::Write;

use brush_core::builtins::{ContentOptions, ContentType, Registration, SimpleCommand};
use brush_core::commands::ExecutionContext;
use brush_core::extensions::ShellExtensions;
use brush_core::{Error, ExecutionResult};

/// Serializes the native fd-1/2 swap below. The `dup2` redirect targets the **process-global**
/// stdout/stderr, so two threads running uu_* builtins at once would clobber each other's capture.
/// clank executes one line at a time in production, but parallel tests (many `Session`s on the
/// multi-thread runtime) do run uu_* concurrently — this guard makes that safe. Held only around
/// the swap+uumain+restore, never across an `.await`.
#[cfg(not(target_arch = "wasm32"))]
static FD_SWAP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Bookkeeping for [`ShellCwd`]: how many builtin calls are currently inside a cwd window, and where
/// to return when the last of them leaves.
///
/// Locked **only** for that bookkeeping — never across a builtin's run — so it cannot deadlock a
/// pipeline (see [`ShellCwd`]).
struct CwdState {
    /// Number of live [`ShellCwd`] guards.
    depth: usize,
    /// Where the first guard in found the process; restored when the last one leaves.
    restore: Option<std::path::PathBuf>,
}

static CWD_STATE: std::sync::Mutex<CwdState> = std::sync::Mutex::new(CwdState {
    depth: 0,
    restore: None,
});

/// Points the **process's** working directory at the **shell's** for the duration of in-process
/// builtin calls, restoring it when the last one finishes.
///
/// Brush keeps `cd` in its own `Shell::working_dir` and deliberately never calls `set_current_dir` —
/// the right call for a shell *library*, which hands the working directory to each spawned child
/// rather than mutating its host process. But clank's builtins are never spawned (wasip2 has no
/// process spawn at all): they run *in this process* and resolve relative paths through
/// `std::env::current_dir()`. Without this bridge they ignore `cd` outright — `cd sub; ls` lists the
/// process's directory and `cd sub; cat f` reads the wrong `f`, while `pwd` and Brush's own redirects
/// (which consult `working_dir`) look perfectly correct. That split is the bug this closes.
///
/// **Restoring is load-bearing**, not tidiness: the cwd is process-global and `Shell::new` seeds a new
/// session's `working_dir` from `std::env::current_dir()`, so a leaked `cd` would silently become the
/// starting directory of the next `Session` built in the same process.
///
/// **Why it is refcounted rather than a plain save/restore.** Brush runs pipeline stages concurrently
/// on native, so `cd sub; a | b` has two guards live at once. A per-guard restore would have the
/// *first* stage to finish put the directory back while the *second* is still running — yanking the
/// cwd out from under it, so a path it opens late resolves against the wrong place. Instead the first
/// guard in records the return path and only the last one out restores it. (Serializing the guards
/// instead is not an option: `run_tool` cannot hold a lock across a stage's run, or a stage blocked
/// writing into a pipe would deadlock the stage draining it.)
///
/// Stages of one line share a `working_dir`, so a nested guard finds the process already where it
/// wants it and simply leaves it. What refcounting cannot fix is two *different* sessions with
/// *different* working dirs running builtins at the same time: there is one process directory and they
/// disagree about it, so whoever is between calls loses. That cannot arise in production — one agent,
/// one line at a time — and only parallel tests get near it. Closing it properly means teaching every
/// tool to resolve against `working_dir` explicitly, which is impossible for the `uu_*` crates: they
/// call `current_dir()` deep inside code we don't own.
struct ShellCwd;

impl ShellCwd {
    fn enter<SE: ShellExtensions>(context: &ExecutionContext<'_, SE>) -> Self {
        // Poisoning is harmless — the state is two plain fields — so recover the guard either way.
        let mut state = CWD_STATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let target = context.shell.working_dir();
        let current = std::env::current_dir().ok();

        // Already there is the common case (no `cd` yet, or a sibling stage of this line moved us to
        // the same place) — then there is nothing to move and nothing to put back.
        if current.as_deref() != Some(target) {
            // If the move doesn't take, leave the builtin to behave as it did before rather than fail
            // the command: `cd` is then cosmetic for this call — the pre-fix behavior, not worse.
            if std::env::set_current_dir(target).is_ok() && state.depth == 0 {
                // Only the first guard in records the return path. A later one would record a
                // directory a sibling stage had already moved us to, and restoring *that* at the end
                // would leave the process somewhere the shell never was.
                state.restore = current;
            }
        }

        state.depth += 1;
        Self
    }
}

impl Drop for ShellCwd {
    fn drop(&mut self) {
        let mut state = CWD_STATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.depth -= 1;
        // Last one out returns the process, so a still-running stage never has it moved underneath it.
        if state.depth == 0 {
            if let Some(dir) = state.restore.take() {
                let _ = std::env::set_current_dir(dir);
            }
        }
    }
}

/// Stage this pipeline stage's piped stdin into a private temp fd, so a uutils tool that reads stdin
/// (`sort`, `head`, `wc`, `tr`, `cat`) sees the upstream stage's bytes instead of the terminal.
///
/// Returns `None` — leaving fd 0 alone — for anything that is not an `OpenFile::PipeReader`. That
/// narrowness is deliberate and load-bearing: **the shell's own stdin must never be staged.** When
/// clank is script-fed (`clank < script`) brush hands a command the inherited stdin as an
/// `OpenFile::File`, which is indistinguishable by variant from a per-command `< file` redirect —
/// draining it swallows the rest of the script and the session dies at that line (observed: `ls |
/// wc -l` consumed the remaining 11 lines and reported 67 instead of 56). A `PipeReader` can only
/// come from a pipeline stage, so it is always safe to consume. The cost is that `uu < file` still
/// reads the real fd 0 rather than the redirect — exactly as before this function existed, so no
/// regression — while pipelines, the common case, now compose.
///
/// The upstream bytes are drained here, BEFORE the caller takes [`FD_SWAP_LOCK`], and that ordering
/// is load-bearing too. Brush runs non-final pipeline stages concurrently on native
/// (`spawn_blocking`), so in `ls | sort` the producer `ls` needs the lock to write fd 1 while the
/// consumer `sort` is still reading. Draining outside the guarded region lets the producer acquire
/// the lock and finish; draining inside it would deadlock the pipeline.
///
/// The backing file is unlinked immediately — the open fd keeps it alive, so it needs no cleanup and
/// is invisible to the user, and concurrent stages can never collide on a shared path.
///
/// Two limits worth knowing, neither of them new here:
///
/// - This only rebinds the *fd*. When clank itself is script-fed (`clank < script`) the REPL reads
///   through the process-global buffered [`std::io::stdin`] (`native.rs`), which has already pulled
///   later script lines into userspace — a uu consumer drains that buffer in addition to the staged
///   pipe, so `ls | wc -l` over-counts by the number of unconsumed lines and the rest of the script
///   is skipped. `dup2` cannot reach bytes that are already in a `BufReader`. The pre-fix code was
///   broken here too (it saw *only* the leftover lines); fixing it properly means moving the REPL
///   off the shared buffered stdin.
/// - The upstream is drained to EOF, so an unbounded producer (`yes | head -1`) never completes —
///   `head` cannot early-exit and hand the producer an EPIPE, because the drain has to finish before
///   the lock is taken. Bounded input, the shell's normal case, is unaffected.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::items_after_statements)] // the SEQ counter lives beside its only use
fn stage_piped_stdin<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
) -> Option<std::fs::File> {
    use brush_core::openfiles::{OpenFile, OpenFiles};
    use std::io::Seek;

    let Some(mut source @ OpenFile::PipeReader(_)) = context.try_fd(OpenFiles::STDIN_FD) else {
        return None;
    };

    // Unique per process AND per call: two pipeline stages stage their stdin concurrently.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(".clank-uu-stdin-{}-{seq}", std::process::id()));

    let mut staged = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .ok()?;
    let _ = std::fs::remove_file(&path);
    // Stream straight to the file — never `read_to_end` into a `Vec` first. Materializing the whole
    // upstream in memory made `cat /dev/zero | head -1` climb past 16 GB RSS before the watchdog
    // killed it; copying in fixed-size chunks keeps this flat, which is what the temp file is for.
    std::io::copy(&mut source, &mut staged).ok()?;
    staged.seek(std::io::SeekFrom::Start(0)).ok()?;
    Some(staged)
}

/// Run a uutils `uumain` closure with the process's stdin/stdout/stderr pointed at the `OpenFile`s
/// brush assigned for this command, so its input and output land wherever brush wants them.
#[cfg(not(target_arch = "wasm32"))]
#[allow(unsafe_code, clippy::similar_names)] // libc dup/dup2/signal/close FFI over raw fds (see per-call SAFETY); saved_in/out/err intentional
pub(crate) fn run_uu<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
    uumain: impl FnOnce() -> i32,
) -> i32 {
    use brush_core::openfiles::OpenFiles;
    use std::io::Write;
    use std::os::fd::AsRawFd;

    // Drain a pipeline stage's piped stdin BEFORE taking the lock — the ordering keeps concurrent
    // stages from deadlocking, and only a `PipeReader` is ever consumed. See `stage_piped_stdin`.
    let staged_stdin = stage_piped_stdin(context);

    // Serialize the process-global fd swap (see `FD_SWAP_LOCK`). Poisoning is harmless here — the
    // guarded region restores fds even on panic paths — so recover the guard either way.
    let _fd_guard = FD_SWAP_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    // Relative operands (`cat f`, `ls`, `touch f`) resolve against the shell's `cd`, not the process's
    // directory. See `ShellCwd`.
    let _cwd = ShellCwd::enter(context);

    // A broken pipe (e.g. `cat | head`) must not kill the whole clank process; make writes to
    // a closed pipe return EPIPE instead of raising SIGPIPE.
    // SAFETY: `libc::signal` is an FFI call with no memory-safety precondition — `SIGPIPE`/`SIG_IGN`
    // are valid constants and it returns the previous handler (or `SIG_ERR`), never exhibiting UB.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };

    // Flush anything already buffered before swapping the underlying fds.
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    // Point the real fds at the OpenFiles brush assigned for this command (a redirect target or the
    // transcript-capture file), run uumain, then restore them. fd 0 is swapped ONLY for a pipeline
    // stage's `PipeReader` (see `stage_piped_stdin`) — the shell's own stdin, whether a terminal or
    // a script file, is never taken as a tool's pipe source. That keeps the terminal case exactly as
    // it was; it does NOT make clank sole owner of its input in the script-fed case, where the REPL's
    // buffered `std::io::stdin` is a channel `dup2` cannot reach (see `stage_piped_stdin`).
    let redirect = |shell_fd, real_fd| -> i32 {
        // SAFETY: `libc::dup`/`dup2` are FFI calls over raw fds with no memory-safety precondition;
        // they return -1 on error (handled by the `saved >= 0` guard in the restore loop) rather than
        // exhibiting UB. `real_fd` is a standard fd (0/1/2); `fd` is borrowed from a live `OpenFile`.
        let saved = unsafe { libc::dup(real_fd) };
        if let Some(target) = context.try_fd(shell_fd) {
            if let Ok(fd) = target.try_borrow_as_fd() {
                // SAFETY: as above — `dup2` over valid raw fds, -1 on error, never UB.
                unsafe { libc::dup2(fd.as_raw_fd(), real_fd) };
            }
        }
        saved
    };
    let saved_in = match &staged_stdin {
        Some(staged) => {
            // SAFETY: `dup`/`dup2` over valid raw fds (fd 0 and the staged file's live fd); they
            // return -1 on error and never exhibit UB.
            let saved = unsafe { libc::dup(0) };
            unsafe { libc::dup2(staged.as_raw_fd(), 0) };
            saved
        }
        None => -1,
    };
    let saved_out = redirect(OpenFiles::STDOUT_FD, 1);
    let saved_err = redirect(OpenFiles::STDERR_FD, 2);

    // uucore's exit code is a process-global (`AtomicI32`) that upstream resets by process
    // exit — which never happens in-process here. Without this, one failed command poisons
    // every later success's exit code (ls-of-missing made a subsequent touch report 2).
    uucore::error::set_exit_code(0);
    let code = uumain();

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    for (saved, real_fd) in [(saved_in, 0), (saved_out, 1), (saved_err, 2)] {
        if saved >= 0 {
            // SAFETY: `saved` is a live fd from a successful `dup` above (the `saved >= 0` guard);
            // `dup2` restores it onto the standard fd and `close` releases it. FFI, -1 on error, no UB.
            unsafe {
                libc::dup2(saved, real_fd);
                libc::close(saved);
            }
        }
    }
    code
}

/// wasm: rebind the process stdio with `dup2` (wasi-libc implements it over wasip2), pointing each
/// standard fd at a real file so uutils' process-global I/O composes with Brush's fd table:
/// - fd 0 ← a staging file holding this command's piped stdin (or empty) — uutils tools that read
///   stdin get the pipe bytes, and NEVER the real wasip2 stdin resource, whose blocking read TRAPS
///   the component (a durable agent has no interactive stdin).
/// - fd 1/2 ← a capture file; `uumain`'s output is read back and handed to brush's stdout
///   (`context.stdout()`), which the Session drains to the p3 stream + transcript.
///
/// `dup2` (not close-then-reopen) is essential: "the next open claims the lowest free fd" is NOT a
/// dependable invariant mid-session — once fds 0-2 have been recycled, unrelated opens land on them
/// and the reopen trick assigns the wrong numbers (observed live: stdin at 1, stderr at 0). After
/// the call, fds 0-2 intentionally STAY bound to the staging/capture files as stable anchors for
/// the next call. Requires a writable /tmp (created on demand); without one we run uncaptured.
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code, clippy::items_after_statements, clippy::similar_names)] // __wasilibc_fd_renumber FFI (see SAFETY in bind_to_fd); the fd-renumber shim + bind_to_fd helper sit with their first use; out_bytes/err_bytes share the _bytes suffix
pub(crate) fn run_uu<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
    uumain: impl FnOnce() -> i32,
) -> i32 {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    // Relative operands resolve against the shell's `cd`, not the process's directory. Single-threaded
    // here, so no lock is needed. See `ShellCwd`.
    let _cwd = ShellCwd::enter(context);

    extern "C" {
        /// wasi-libc's descriptor renumber: atomically MOVES `fd` onto `newfd`, closing whatever
        /// `newfd` previously was and freeing the source number. (There is no `dup2` symbol in the
        /// wasm32-wasip2 libc; this is the primitive behind it.)
        fn __wasilibc_fd_renumber(fd: i32, newfd: i32) -> i32;
    }

    /// Move `file`'s descriptor onto `target_fd`. The `File` is forgotten first so its `Drop` can
    /// never close the (freed, possibly recycled) source number afterwards.
    fn bind_to_fd(file: std::fs::File, target_fd: i32) {
        let raw = file.as_raw_fd();
        std::mem::forget(file);
        // SAFETY: `__wasilibc_fd_renumber` is an FFI call over raw fds with no memory-safety
        // precondition; `raw` is a live fd we just leaked via `mem::forget` (so its `Drop` can't race
        // the renumber and close the freed/recycled source), and `target_fd` is a standard fd.
        unsafe { __wasilibc_fd_renumber(raw, target_fd) };
    }

    // Separate capture files for fd 1 and fd 2, so the tool's stdout and stderr reach Brush's
    // stdout/stderr `OpenFile`s as DISTINCT streams — `cmd 2>/dev/null`, `2>file`, and the
    // structured EvalResult's stderr all work for uutils commands. (The former single shared file
    // preserved write-order interleaving but made the two streams inseparable.) Kept out of the
    // working directory so they don't pollute it.
    const CAPTURE_OUT_PATH: &str = "/tmp/.clank-uu-out";
    const CAPTURE_ERR_PATH: &str = "/tmp/.clank-uu-err";

    // The stdin staging file — the piped bytes a uutils tool will see as fd 0.
    const STDIN_PATH: &str = "/tmp/.clank-uu-stdin";

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    // A fresh agent has no /tmp until something creates it.
    let _ = std::fs::create_dir_all("/tmp");

    // fd 0 ← staged stdin. The bytes are complete before this stage runs (wasm pipeline stages
    // execute inline in order), so the whole pipe payload is staged in one go.
    let mut stdin_bytes = Vec::new();
    let _ = effective_stdin(context).read_to_end(&mut stdin_bytes);
    if std::fs::write(STDIN_PATH, &stdin_bytes).is_ok() {
        if let Ok(f) = OpenOptions::new().read(true).open(STDIN_PATH) {
            bind_to_fd(f, 0);
        }
    }

    // fd 1 ← fresh stdout capture; fd 2 ← fresh stderr capture (both truncating).
    let fresh_capture = |path| {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    };
    let captured = match fresh_capture(CAPTURE_OUT_PATH) {
        Ok(f) => {
            bind_to_fd(f, 1);
            true
        }
        Err(_) => false, // no writable fs: run uncaptured
    };
    if captured {
        if let Ok(f) = fresh_capture(CAPTURE_ERR_PATH) {
            bind_to_fd(f, 2);
        }
    }

    // uucore's exit code is a process-global (`AtomicI32`) that upstream resets by process
    // exit — which never happens in-process here (a wasm agent instance is one long-lived
    // process across invocations). Without this, one failed command poisons every later
    // success's exit code.
    uucore::error::set_exit_code(0);
    let code = uumain();
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    // Replay each capture into its Brush `OpenFile` so the streams stay separate all the way to
    // the fd table (redirects) and the EvalResult (stdout vs stderr).
    if captured {
        if let Ok(out_bytes) = std::fs::read(CAPTURE_OUT_PATH) {
            let _ = context.stdout().write_all(&out_bytes);
        }
        if let Ok(err_bytes) = std::fs::read(CAPTURE_ERR_PATH) {
            let _ = context.stderr().write_all(&err_bytes);
        }
    }
    code
}

/// This command's *effective* stdin on the wasm agent: the source Brush assigned when it is a pipe
/// stage (`OpenFile::Stream`, the in-memory pipe reader) or a redirect (`OpenFile::File`) — and
/// **empty input** when it is the default `OpenFile::Stdin`. The real wasip2 stdin resource must
/// never be read: a durable agent has no interactive stdin and `input-stream.blocking-read` on it
/// TRAPS the whole component (wedging the agent instance).
#[cfg(target_arch = "wasm32")]
fn effective_stdin<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
) -> Box<dyn std::io::Read> {
    use brush_core::openfiles::{OpenFile, OpenFiles};
    match context.try_fd(OpenFiles::STDIN_FD) {
        Some(f @ (OpenFile::File(_) | OpenFile::PipeReader(_) | OpenFile::Stream(_))) => Box::new(f),
        _ => Box::new(std::io::empty()),
    }
}

/// Run a tool closure over Brush's assigned streams — used by the text/data builtins
/// (grep/jq/sed/…) whose Rust-library implementations we control. Unlike [`run_uu`], this does NOT
/// swap process fds: it hands the closure `context.stdin()`, `context.stdout()`, and
/// `context.stderr()`, which are Brush's `OpenFile`s. On wasm those are the in-memory capture/pipe
/// streams, so output is captured and piped input (`cmd | grep …`) reaches the tool — writing to the
/// process-global `io::stdout()` / reading process-global `io::stdin()` do neither on wasm. The
/// `stdin` reader lets a tool consume an upstream pipeline stage's output when given no file operands.
/// No `/tmp` capture file, no fd games; on wasm the stdin handed over is [`effective_stdin`] (never
/// the trapping real stdin resource).
pub(crate) fn run_tool<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
    run: impl FnOnce(&mut dyn std::io::Read, &mut dyn std::io::Write, &mut dyn std::io::Write) -> i32,
) -> i32 {
    // File operands (`grep pat f`, `find .`, `stat f`) resolve against the shell's `cd`. Note this
    // takes no lock across the run — brush runs pipeline stages concurrently on native, so blocking a
    // stage another is draining through a pipe would deadlock; `ShellCwd` is refcounted instead.
    let _cwd = ShellCwd::enter(context);
    let mut stdin = tool_stdin(context);
    let mut out = context.stdout();
    let mut err = context.stderr();
    let code = run(&mut stdin, &mut out, &mut err);
    let _ = out.flush();
    let _ = err.flush();
    code
}

/// This command's stdin as a reader — Brush's assigned `OpenFile` on native, and on wasm the
/// [`effective_stdin`] guard (piped/redirected input, or empty for the default stdin: the real
/// wasip2 stdin resource must never be read — its blocking read traps the agent). For builtins
/// that read stdin outside the [`run_tool`] closure shape (e.g. `xargs`).
pub(crate) fn tool_stdin<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
) -> Box<dyn std::io::Read> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        Box::new(context.stdin())
    }
    #[cfg(target_arch = "wasm32")]
    {
        effective_stdin(context)
    }
}

/// Shared `get_content` body: derive real help content from a synopsis rather than a stub. Used by
/// every coreutils builtin (macro-generated and the hand-written `/proc`-shimming `cat`/`ls`).
#[allow(clippy::needless_pass_by_value)] // ContentType is a fieldless brush enum matched here; by-ref would ripple to every caller
fn uu_get_content(name: &str, synopsis: &str, content_type: ContentType) -> Result<String, Error> {
    match content_type {
        ContentType::ShortDescription => Ok(format!("{name} - {synopsis}\n")),
        ContentType::ShortUsage => Ok(format!("{name}: {name} [args...]\n")),
        ContentType::DetailedHelp => {
            Ok(format!("{name} - {synopsis}\n\n(uutils coreutils builtin)\n"))
        }
        ContentType::ManPage => brush_core::error::unimp("man page not yet implemented"),
    }
}

/// Define a brush `SimpleCommand` that dispatches to a uutils `uumain`, prepending `argv[0]`.
///
/// The `$synopsis` is the command's one-line description: it feeds both `get_content` (so Brush's
/// `help`/`type` surface real content instead of a stub) and the command [`Manifest`] built in
/// [`manifests`] — defined once, so the two can't drift.
macro_rules! uu_builtin {
    ($ty:ident, $name:literal, $synopsis:literal, $uumain:path) => {
        pub(crate) struct $ty;

        impl $ty {
            const NAME: &'static str = $name;
            const SYNOPSIS: &'static str = $synopsis;
        }

        impl SimpleCommand for $ty {
            fn get_content(
                name: &str,
                content_type: ContentType,
                _options: &ContentOptions,
            ) -> Result<String, Error> {
                uu_get_content(name, $ty::SYNOPSIS, content_type)
            }

            #[allow(clippy::cast_sign_loss)] // code is clamped to 0..=255 before the u8 cast
            fn execute<SE, I, S>(
                context: ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<ExecutionResult, Error>
            where
                SE: ShellExtensions,
                I: Iterator<Item = S>,
                S: AsRef<str>,
            {
                // brush already passes the command name as args[0], which is what uutils'
                // `uumain` expects for argv[0].
                let _ = $name;
                let argv = args.map(|s| std::ffi::OsString::from(s.as_ref()));
                let code = run_uu(&context, move || $uumain(argv));
                Ok(ExecutionResult::new(code.clamp(0, 255) as u8))
            }
        }
    };
}

// `cat` and `ls` are hand-written (not `uu_builtin!`) so they can serve the virtual `/proc`
// namespace: `/proc` operands are resolved from the process table and written to stdout, while any
// real path is delegated to the uutils `uumain` unchanged. See `Cat`/`Ls` below.
uu_builtin!(Wc, "wc", "count lines, words, and bytes", uu_wc::uumain);
uu_builtin!(Head, "head", "print the first lines of a file", uu_head::uumain);
uu_builtin!(Sort, "sort", "sort lines of text", uu_sort::uumain);
uu_builtin!(Rm, "rm", "remove files and directories", uu_rm::uumain);
uu_builtin!(Mv, "mv", "move or rename files", uu_mv::uumain);
uu_builtin!(Cp, "cp", "copy files and directories", uu_cp::uumain);
// mkdir uses the same convention as the others: brush passes the command name as argv[0], which is
// what uumain expects — do NOT skip it, or flags like `-p` get dropped (dropping `-p` turned
// `mkdir -p /tmp/a/b` into a non-recursive mkdir that fails when an intermediate dir is missing).
uu_builtin!(Mkdir, "mkdir", "create directories", uu_mkdir::uumain);
// `env` is hand-written (not `uu_builtin!`) so the LISTING form filters `export --secret` variables
// out of the environment — uu_env reads `std::env` directly, which would otherwise leak a secret
// through `env` (and, worse, `env | grep …` in a pipeline). See `Env` below.
uu_builtin!(Cut, "cut", "select fields from each line", uu_cut::uumain);
uu_builtin!(Tr, "tr", "translate or delete characters", uu_tr::uumain);
uu_builtin!(Uniq, "uniq", "report or omit repeated lines", uu_uniq::uumain);
uu_builtin!(Tail, "tail", "print the last lines of a file", uu_tail::uumain);
uu_builtin!(Tee, "tee", "copy stdin to stdout and files", uu_tee::uumain);
uu_builtin!(Touch, "touch", "create files or update timestamps", uu_touch::uumain);
uu_builtin!(Sleep, "sleep", "pause for a duration", uu_sleep::uumain);
// printf shadows Brush's builtin (registered after `default_builtins`; last write wins). Brush's
// printf is gated to `any(unix, windows)` upstream, so without this the wasm agent has no printf
// at all and the word falls through to (unsupported) external exec. Trade-off: bash's `printf -v
// VAR` (assign to a shell variable) is not supported by uu_printf on either target.
uu_builtin!(Printf, "printf", "format and print data", uu_printf::uumain);

/// An operand is a path (not a flag) if it doesn't start with `-`. (Flags always pass through to
/// uutils.) Used to detect whether an invocation touches the virtual `/proc` namespace.
fn is_flag(arg: &str) -> bool {
    arg.starts_with('-')
}

/// `cat`, hand-written to serve the virtual `/proc` namespace. If no operand is a `/proc` path, the
/// whole argv is delegated to `uu_cat::uumain` unchanged (real-file behavior + all flags preserved).
/// Otherwise each operand is served in order: `/proc` paths from the process-table resolver, real
/// paths delegated per-file to `uu_cat`.
pub(crate) struct Cat;

impl Cat {
    const NAME: &'static str = "cat";
    const SYNOPSIS: &'static str = "concatenate files and print to stdout";
}

impl SimpleCommand for Cat {
    fn get_content(
        name: &str,
        content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, Error> {
        uu_get_content(name, Cat::SYNOPSIS, content_type)
    }

    #[allow(clippy::cast_sign_loss)] // code is clamped to 0..=255 before the u8 cast
    fn execute<SE, I, S>(context: ExecutionContext<'_, SE>, args: I) -> Result<ExecutionResult, Error>
    where
        SE: ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    {
        let argv: Vec<String> = args.map(|s| s.as_ref().to_string()).collect();
        let touches_virtual = argv.iter().skip(1).any(|a| {
            !is_flag(a)
                && (crate::runtime::procfs::is_proc_path(a)
                    || crate::runtime::binfs::is_bin_path(a)
                    || a == "/dev/null")
        });

        // Fast path: nothing virtual → delegate the whole argv to uutils unchanged.
        if !touches_virtual {
            let os_argv = argv.iter().map(std::ffi::OsString::from);
            let code = run_uu(&context, move || uu_cat::uumain(os_argv));
            return Ok(ExecutionResult::new(code.clamp(0, 255) as u8));
        }

        // Mixed/virtual path: serve each operand in order.
        let environ = crate::runtime::procfs::current_environ();
        let table = crate::runtime::proctable::active();
        let mut out = context.stdout();
        let mut had_error = false;
        for op in argv.iter().skip(1).filter(|a| !is_flag(a)) {
            if op == "/dev/null" {
                // Reads as empty (the emulated null device is not a real fs entry uu_cat can open).
            } else if crate::runtime::binfs::is_bin_path(op) {
                // `/bin/<name>` → the command's help text (static registry; no Session access).
                if let Ok(content) = crate::runtime::binfs::resolve(op) {
                    let _ = out.write_all(content.as_bytes());
                } else {
                    let _ = writeln!(context.stderr(), "cat: {op}: No such file or directory");
                    had_error = true;
                }
            } else if crate::runtime::procfs::is_proc_path(op) {
                let resolved = table
                    .as_ref()
                    .map(|t| crate::runtime::procfs::resolve(op, &t.lock().unwrap(), &environ));
                if let Some(Ok(content)) = resolved {
                    let _ = out.write_all(content.as_bytes());
                } else {
                    let _ = writeln!(context.stderr(), "cat: {op}: No such file or directory");
                    had_error = true;
                }
            } else {
                // A real path in a mixed invocation: delegate just this operand to uutils.
                let one = [std::ffi::OsString::from("cat"), std::ffi::OsString::from(op)];
                let code = run_uu(&context, move || uu_cat::uumain(one.into_iter()));
                if code != 0 {
                    had_error = true;
                }
            }
        }
        Ok(ExecutionResult::new(u8::from(had_error)))
    }
}

/// `env`, hand-written so the LISTING form redacts `export --secret` variables. `uu_env` reads the
/// process environment (`std::env`) directly, so a secret var — which is set in `std::env` for Full
/// env parity (real subprocesses inherit it) — would otherwise leak through `env` and, worse,
/// `env | grep …` as a pipeline stage. This custom builtin renders the *filtered* environment
/// ([`crate::runtime::procfs::current_environ`], which drops secret keys) for the listing case, and
/// delegates to `uu_env` unchanged for the run-a-command form (`env CMD`, `env NAME=val CMD`), which
/// needs the real process env to launch the child.
pub(crate) struct Env;

impl Env {
    const NAME: &'static str = "env";
    const SYNOPSIS: &'static str = "print the environment";
}

impl SimpleCommand for Env {
    fn get_content(
        name: &str,
        content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, Error> {
        uu_get_content(name, Env::SYNOPSIS, content_type)
    }

    #[allow(clippy::cast_sign_loss)] // code is clamped to 0..=255 before the u8 cast
    fn execute<SE, I, S>(context: ExecutionContext<'_, SE>, args: I) -> Result<ExecutionResult, Error>
    where
        SE: ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    {
        let argv: Vec<String> = args.map(|s| s.as_ref().to_string()).collect();

        // Delegate to uu_env ONLY when there is a real command to run — a bare operand that is neither
        // a flag nor a `NAME=value` assignment. Running a command must launch a child with the real
        // process environment (Full env parity — the child inherits secrets). Every listing form
        // (with or without `-i`/`-u`/`-0`) is served here so the secret filter always applies; there
        // is no child to inherit anything, so uu_env's env-manipulation flags reduce to display rules
        // we can honor directly.
        let has_command = argv
            .iter()
            .skip(1)
            .any(|a| !is_flag(a) && !a.contains('='));
        if has_command {
            let os_argv = argv.iter().map(std::ffi::OsString::from);
            let code = run_uu(&context, move || uu_env::uumain(os_argv));
            return Ok(ExecutionResult::new(code.clamp(0, 255) as u8));
        }

        // Listing form. Start from the secret-filtered environment (unless `-i`/`--ignore-environment`,
        // which starts empty), apply any `-u NAME`/`--unset NAME` removals and inline `NAME=value`
        // additions, then print. `-0`/`--null` uses NUL separators (GNU `env -0`); otherwise
        // `KEY=VALUE\n` lines, sorted for a deterministic order (uu_env's is arbitrary; no test pins it).
        let mut null_sep = false;
        let mut ignore_env = false;
        let mut unset: Vec<String> = Vec::new();
        let mut adds: Vec<(String, String)> = Vec::new();
        let mut it = argv.iter().skip(1).peekable();
        while let Some(a) = it.next() {
            match a.as_str() {
                "-0" | "--null" => null_sep = true,
                "-i" | "--ignore-environment" => ignore_env = true,
                "-u" | "--unset" => {
                    if let Some(name) = it.next() {
                        unset.push(name.clone());
                    }
                }
                _ if a.starts_with("-u") => unset.push(a[2..].to_string()),
                _ => {
                    if let Some((k, v)) = a.split_once('=') {
                        adds.push((k.to_string(), v.to_string()));
                    }
                }
            }
        }

        let mut env: std::collections::BTreeMap<String, String> = if ignore_env {
            std::collections::BTreeMap::new()
        } else {
            crate::runtime::procfs::current_environ().into_iter().collect()
        };
        for name in &unset {
            env.remove(name);
        }
        for (k, v) in adds {
            env.insert(k, v);
        }

        let mut out = context.stdout();
        for (k, v) in env {
            if null_sep {
                let _ = write!(out, "{k}={v}\0");
            } else {
                let _ = writeln!(out, "{k}={v}");
            }
        }
        Ok(ExecutionResult::new(0))
    }
}

/// `ls`, hand-written to serve the virtual `/proc` namespace. `ls /proc/<pid>` and `ls /proc/clank`
/// list the fixed child names; real paths are delegated to `uu_ls::uumain` unchanged. Top-level
/// `ls /proc` (enumerating every pid) is deferred to a later increment.
pub(crate) struct Ls;

impl Ls {
    const NAME: &'static str = "ls";
    const SYNOPSIS: &'static str = "list directory contents";
}

impl SimpleCommand for Ls {
    fn get_content(
        name: &str,
        content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, Error> {
        uu_get_content(name, Ls::SYNOPSIS, content_type)
    }

    #[allow(clippy::cast_sign_loss, clippy::similar_names)] // code clamped to 0..=255 before the u8 cast; bin_operand/mcp_operand/proc_operand share the _operand suffix
    fn execute<SE, I, S>(context: ExecutionContext<'_, SE>, args: I) -> Result<ExecutionResult, Error>
    where
        SE: ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    {
        let argv: Vec<String> = args.map(|s| s.as_ref().to_string()).collect();

        // `/bin` (the virtual builtin namespace): `ls /bin` lists every command name; `ls /bin/<name>`
        // names the file (like real `ls` on a file), since `/bin/<name>` resolves but isn't a dir.
        let bin_operand = argv
            .iter()
            .skip(1)
            .find(|a| !is_flag(a) && crate::runtime::binfs::is_bin_path(a))
            .cloned();
        if let Some(op) = bin_operand {
            let mut out = context.stdout();
            if let Some(children) = crate::runtime::binfs::list_children(&op) {
                let _ = writeln!(out, "{}", children.join("\n"));
                return Ok(ExecutionResult::new(0));
            }
            // `/bin/<name>`: a file, not a directory. Exists → print its path; else not found.
            if crate::runtime::binfs::resolve(&op).is_ok() {
                let _ = writeln!(out, "{op}");
                return Ok(ExecutionResult::new(0));
            }
            let _ = writeln!(context.stderr(), "ls: {op}: No such file or directory");
            return Ok(ExecutionResult::new(1));
        }

        // `/mnt/mcp/...` (the MCP resource mount): list the virtual tree from the per-line index,
        // which includes dynamic resources that have no real file on disk (static ones are real files
        // but listing them here keeps the view uniform).
        let mcp_operand = argv
            .iter()
            .skip(1)
            .find(|a| !is_flag(a) && crate::runtime::mcpfs::is_mcp_path(a))
            .cloned();
        if let Some(op) = mcp_operand {
            let index = crate::runtime::mcpfs::active().unwrap_or_default();
            let mut out = context.stdout();
            match crate::runtime::mcpfs::classify(&op, &index) {
                crate::runtime::mcpfs::McpPathKind::Directory(children) => {
                    let _ = writeln!(out, "{}", children.join("\n"));
                    return Ok(ExecutionResult::new(0));
                }
                crate::runtime::mcpfs::McpPathKind::Static
                | crate::runtime::mcpfs::McpPathKind::Dynamic { .. }
                | crate::runtime::mcpfs::McpPathKind::Template => {
                    // A file/executable (not a dir): `ls <path>` names it, like real `ls`.
                    let _ = writeln!(out, "{op}");
                    return Ok(ExecutionResult::new(0));
                }
                crate::runtime::mcpfs::McpPathKind::NotFound => {
                    let _ = writeln!(context.stderr(), "ls: {op}: No such file or directory");
                    return Ok(ExecutionResult::new(1));
                }
            }
        }

        let proc_operand = argv
            .iter()
            .skip(1)
            .find(|a| !is_flag(a) && crate::runtime::procfs::is_proc_path(a))
            .cloned();

        // Only the fixed-child-name listing of `/proc/<pid>` and `/proc/clank` is served here.
        if let Some(op) = proc_operand {
            if let Some(children) = crate::runtime::procfs::list_children(&op) {
                let mut out = context.stdout();
                let _ = writeln!(out, "{}", children.join("\n"));
                return Ok(ExecutionResult::new(0));
            }
            let _ = writeln!(context.stderr(), "ls: {op}: No such file or directory");
            return Ok(ExecutionResult::new(1));
        }

        // No virtual operand → delegate unchanged.
        let os_argv = argv.iter().map(std::ffi::OsString::from);
        let code = run_uu(&context, move || uu_ls::uumain(os_argv));
        Ok(ExecutionResult::new(code.clamp(0, 255) as u8))
    }
}

/// The coreutils builtins to register on the shell, in addition to brush's bash set.
pub(crate) fn builtins<SE: ShellExtensions>() -> Vec<(String, Registration<SE>)> {
    use brush_core::builtins::simple_builtin;
    vec![
        ("cat".into(), simple_builtin::<Cat, SE>()),
        ("ls".into(), simple_builtin::<Ls, SE>()),
        ("wc".into(), simple_builtin::<Wc, SE>()),
        ("head".into(), simple_builtin::<Head, SE>()),
        ("sort".into(), simple_builtin::<Sort, SE>()),
        ("mkdir".into(), simple_builtin::<Mkdir, SE>()),
        ("rm".into(), simple_builtin::<Rm, SE>()),
        ("mv".into(), simple_builtin::<Mv, SE>()),
        ("cp".into(), simple_builtin::<Cp, SE>()),
        ("env".into(), simple_builtin::<Env, SE>()),
        ("cut".into(), simple_builtin::<Cut, SE>()),
        ("tr".into(), simple_builtin::<Tr, SE>()),
        ("uniq".into(), simple_builtin::<Uniq, SE>()),
        ("tail".into(), simple_builtin::<Tail, SE>()),
        ("tee".into(), simple_builtin::<Tee, SE>()),
        ("touch".into(), simple_builtin::<Touch, SE>()),
        ("sleep".into(), simple_builtin::<Sleep, SE>()),
        ("printf".into(), simple_builtin::<Printf, SE>()),
    ]
}

/// The [`Manifest`] for each coreutils builtin, built from the same `NAME`/`SYNOPSIS` constants the
/// commands expose — so a builtin and its manifest can't describe themselves differently. The
/// registry drift-guard test asserts this list's names match [`builtins`]'s.
///
/// Mostly `Subprocess` scope, `Allow` policy (uutils file/text tools); the constructor defaults
/// cover that, so each entry is a one-liner. The exception is `rm`, a destructive op the README
/// classifies `sudo-only` — the first policy actually enforced (see [`crate::authz`]). Richer
/// per-command input schemas and the write-to-`~`=`confirm` file-path policies come later.
pub(crate) fn manifests() -> Vec<crate::manifest::Manifest> {
    use crate::manifest::{AuthorizationPolicy, Manifest};
    vec![
        Manifest::builtin(Cat::NAME, Cat::SYNOPSIS),
        Manifest::builtin(Ls::NAME, Ls::SYNOPSIS),
        Manifest::builtin(Wc::NAME, Wc::SYNOPSIS),
        Manifest::builtin(Head::NAME, Head::SYNOPSIS),
        Manifest::builtin(Sort::NAME, Sort::SYNOPSIS),
        Manifest::builtin(Mkdir::NAME, Mkdir::SYNOPSIS),
        // Destructive: sudo-only (README's authorization example table).
        Manifest::builtin(Rm::NAME, Rm::SYNOPSIS).with_policy(AuthorizationPolicy::SudoOnly),
        Manifest::builtin(Mv::NAME, Mv::SYNOPSIS),
        Manifest::builtin(Cp::NAME, Cp::SYNOPSIS),
        Manifest::builtin(Env::NAME, Env::SYNOPSIS),
        Manifest::builtin(Cut::NAME, Cut::SYNOPSIS),
        Manifest::builtin(Tr::NAME, Tr::SYNOPSIS),
        Manifest::builtin(Uniq::NAME, Uniq::SYNOPSIS),
        Manifest::builtin(Tail::NAME, Tail::SYNOPSIS),
        Manifest::builtin(Tee::NAME, Tee::SYNOPSIS),
        Manifest::builtin(Touch::NAME, Touch::SYNOPSIS),
        Manifest::builtin(Sleep::NAME, Sleep::SYNOPSIS),
        Manifest::builtin(Printf::NAME, Printf::SYNOPSIS).with_help(
            "printf FORMAT [ARG...] — format and print data (uutils printf). Supports %s %d %x \
             %f etc. and \\n escapes. bash's `printf -v VAR` (assign to a shell variable) is not \
             supported.",
        ),
    ]
}
