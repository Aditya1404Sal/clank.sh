//! Internal coreutils: uutils (`uu_*`) crates registered as clank builtins, so `cat`/`ls`/etc.
//! run **inside** the component instead of forking host programs — matching the README's
//! "core commands resolved internally, no fork/exec" model.
//!
//! Each `uu_*::uumain(args) -> i32` writes to `std::io::stdout()` directly, so to make its
//! output compose with brush's fd table (pipes, redirections, the transcript capture) we
//! redirect the real fd 1/2 onto the `OpenFile`s brush assigned for the command, run `uumain`,
//! then restore them. On wasm there is no `dup2`, so stdout is captured by reopening fd 1 as a
//! temporary read/write file, then replaying those bytes into Brush's stdout.
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

/// Run a uutils `uumain` closure with the process's stdout/stderr pointed at the `OpenFile`s
/// brush assigned for this command, so its output lands wherever brush wants it.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn run_uu<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
    uumain: impl FnOnce() -> i32,
) -> i32 {
    use brush_core::openfiles::OpenFiles;
    use std::io::Write;
    use std::os::fd::AsRawFd;

    // Serialize the process-global fd swap (see `FD_SWAP_LOCK`). Poisoning is harmless here — the
    // guarded region restores fds even on panic paths — so recover the guard either way.
    let _fd_guard = FD_SWAP_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // A broken pipe (e.g. `cat | head`) must not kill the whole clank process; make writes to
    // a closed pipe return EPIPE instead of raising SIGPIPE.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };

    // Flush anything already buffered before swapping the underlying fds.
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    // Point real fds 1/2 at the OpenFiles brush assigned for this command (a redirect target
    // or the transcript-capture file), run uumain, then restore them. We deliberately do NOT
    // touch fd 0: clank's REPL owns stdin, and brush runs pipeline stages concurrently, so
    // redirecting the process-global stdin races with the shell itself. The consequence is
    // that uutils commands don't compose as pipeline stages (they read/write the real
    // terminal fds); single invocations and output redirection work and are captured.
    let redirect = |shell_fd, real_fd| -> i32 {
        let saved = unsafe { libc::dup(real_fd) };
        if let Some(target) = context.try_fd(shell_fd) {
            if let Ok(fd) = target.try_borrow_as_fd() {
                unsafe { libc::dup2(fd.as_raw_fd(), real_fd) };
            }
        }
        saved
    };
    let saved_out = redirect(OpenFiles::STDOUT_FD, 1);
    let saved_err = redirect(OpenFiles::STDERR_FD, 2);

    let code = uumain();

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    for (saved, real_fd) in [(saved_out, 1), (saved_err, 2)] {
        if saved >= 0 {
            unsafe {
                libc::dup2(saved, real_fd);
                libc::close(saved);
            }
        }
    }
    code
}

/// wasm: there is no `dup2`, but `close(1)` frees fd 1 and the next `open()` claims it (lowest
/// free fd), so opening a capture file *becomes* stdout. `uumain` then writes into that file; we
/// read it back and hand it to brush's stdout (`context.stdout()`), which the Session drains to
/// the p3 stream + transcript. clank displays via the p3 stream (not fd 1), so we never need to
/// restore fd 1. Requires a writable preopen (`wasmtime --dir`); without one we fall back to the
/// real stdout (visible, uncaptured).
#[cfg(target_arch = "wasm32")]
pub(crate) fn run_uu<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
    uumain: impl FnOnce() -> i32,
) -> i32 {
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};

    extern "C" {
        fn close(fd: i32) -> i32;
    }

    // A single capture file under /tmp (a writable preopen on Golem/wasi) receives BOTH fd 1 and
    // fd 2, so stdout and stderr are captured together in write order and neither is dropped. Kept
    // out of the working directory so it doesn't pollute it.
    const CAPTURE_PATH: &str = "/tmp/.clank-uu-capture";

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    // Point fd 1 at a fresh capture file: `close(1)` frees the descriptor and the next `open`
    // claims the lowest free fd (1). Then `close(2)` + open the SAME path in append mode so fd 2
    // also writes into the capture, after fd 1's content. uutils writes to std stdout/stderr land
    // in the file; we read it back and feed it to brush's stdout (→ transcript + p3 stream).
    unsafe { close(1) };
    let mut cap = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(CAPTURE_PATH)
    {
        Ok(f) => f, // claims fd 1
        Err(_) => return uumain(), // no writable fs: run uncaptured
    };
    unsafe { close(2) };
    // fd 2 → same file, append so it doesn't truncate fd 1's writes. Best-effort.
    let err_fd = OpenOptions::new().append(true).open(CAPTURE_PATH);

    let code = uumain();
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    drop(err_fd); // closes fd 2

    let mut out_bytes = Vec::new();
    let _ = cap
        .seek(SeekFrom::Start(0))
        .and_then(|_| cap.read_to_end(&mut out_bytes));
    drop(cap); // closes fd 1
    let _ = std::fs::remove_file(CAPTURE_PATH);

    // Feed captured output (stdout + stderr, interleaved) into brush's stdout so it reaches the
    // transcript + p3 stream.
    let _ = context.stdout().write_all(&out_bytes);
    code
}

/// Run a tool closure that writes directly to a `Write` sink — used by the text/data builtins
/// (grep/jq/sed/…) whose Rust-library implementations we control. Unlike [`run_uu`], this does NOT
/// swap process fds: it hands the closure `context.stdout()` (and `context.stderr()`), which are
/// Brush's assigned `OpenFile`s. On wasm those are the in-memory capture stream, so output is
/// captured correctly (writing to the process-global `io::stdout()` is NOT captured on wasm — the
/// bug this fixes). Target-agnostic; no `/tmp` capture file, no fd games.
pub(crate) fn run_tool<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
    run: impl FnOnce(&mut dyn std::io::Write, &mut dyn std::io::Write) -> i32,
) -> i32 {
    let mut out = context.stdout();
    let mut err = context.stderr();
    let code = run(&mut out, &mut err);
    let _ = out.flush();
    let _ = err.flush();
    code
}

/// Shared `get_content` body: derive real help content from a synopsis rather than a stub. Used by
/// every coreutils builtin (macro-generated and the hand-written `/proc`-shimming `cat`/`ls`).
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
uu_builtin!(Env, "env", "print the environment", uu_env::uumain);
uu_builtin!(Cut, "cut", "select fields from each line", uu_cut::uumain);
uu_builtin!(Tr, "tr", "translate or delete characters", uu_tr::uumain);
uu_builtin!(Uniq, "uniq", "report or omit repeated lines", uu_uniq::uumain);
uu_builtin!(Tail, "tail", "print the last lines of a file", uu_tail::uumain);
uu_builtin!(Tee, "tee", "copy stdin to stdout and files", uu_tee::uumain);
uu_builtin!(Touch, "touch", "create files or update timestamps", uu_touch::uumain);
uu_builtin!(Sleep, "sleep", "pause for a duration", uu_sleep::uumain);

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

    fn execute<SE, I, S>(context: ExecutionContext<'_, SE>, args: I) -> Result<ExecutionResult, Error>
    where
        SE: ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    {
        let argv: Vec<String> = args.map(|s| s.as_ref().to_string()).collect();
        let touches_proc = argv
            .iter()
            .skip(1)
            .any(|a| !is_flag(a) && crate::procfs::is_proc_path(a));

        // Fast path: nothing virtual → delegate the whole argv to uutils unchanged.
        if !touches_proc {
            let os_argv = argv.iter().map(std::ffi::OsString::from);
            let code = run_uu(&context, move || uu_cat::uumain(os_argv));
            return Ok(ExecutionResult::new(code.clamp(0, 255) as u8));
        }

        // Mixed/virtual path: serve each operand in order.
        let environ = crate::procfs::current_environ();
        let table = crate::proctable::active();
        let mut out = context.stdout();
        let mut had_error = false;
        for op in argv.iter().skip(1).filter(|a| !is_flag(a)) {
            if crate::procfs::is_proc_path(op) {
                let resolved = table
                    .as_ref()
                    .map(|t| crate::procfs::resolve(op, &t.lock().unwrap(), &environ));
                match resolved {
                    Some(Ok(content)) => {
                        let _ = out.write_all(content.as_bytes());
                    }
                    _ => {
                        let _ = writeln!(context.stderr(), "cat: {op}: No such file or directory");
                        had_error = true;
                    }
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
        Ok(ExecutionResult::new(if had_error { 1 } else { 0 }))
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

    fn execute<SE, I, S>(context: ExecutionContext<'_, SE>, args: I) -> Result<ExecutionResult, Error>
    where
        SE: ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    {
        let argv: Vec<String> = args.map(|s| s.as_ref().to_string()).collect();
        let proc_operand = argv
            .iter()
            .skip(1)
            .find(|a| !is_flag(a) && crate::procfs::is_proc_path(a))
            .cloned();

        // Only the fixed-child-name listing of `/proc/<pid>` and `/proc/clank` is served here.
        if let Some(op) = proc_operand {
            if let Some(children) = crate::procfs::list_children(&op) {
                let mut out = context.stdout();
                let _ = writeln!(out, "{}", children.join("\n"));
                return Ok(ExecutionResult::new(0));
            }
            let _ = writeln!(context.stderr(), "ls: {op}: No such file or directory");
            return Ok(ExecutionResult::new(1));
        }

        // No /proc operand → delegate unchanged.
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
    ]
}
