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

use brush_core::builtins::{ContentOptions, ContentType, Registration, SimpleCommand};
use brush_core::commands::ExecutionContext;
use brush_core::extensions::ShellExtensions;
use brush_core::{Error, ExecutionResult};

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

/// Define a brush `SimpleCommand` that dispatches to a uutils `uumain`, prepending `argv[0]`.
macro_rules! uu_builtin {
    ($ty:ident, $name:literal, $uumain:path) => {
        pub(crate) struct $ty;

        impl SimpleCommand for $ty {
            fn get_content(
                name: &str,
                _content_type: ContentType,
                _options: &ContentOptions,
            ) -> Result<String, Error> {
                Ok(format!("{name}: uutils coreutils builtin\n"))
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

uu_builtin!(Cat, "cat", uu_cat::uumain);
uu_builtin!(Ls, "ls", uu_ls::uumain);
uu_builtin!(Wc, "wc", uu_wc::uumain);
uu_builtin!(Head, "head", uu_head::uumain);
uu_builtin!(Sort, "sort", uu_sort::uumain);
uu_builtin!(Rm, "rm", uu_rm::uumain);
uu_builtin!(Mv, "mv", uu_mv::uumain);
uu_builtin!(Cp, "cp", uu_cp::uumain);
// mkdir uses the same convention as the others: brush passes the command name as argv[0], which is
// what uumain expects — do NOT skip it, or flags like `-p` get dropped (dropping `-p` turned
// `mkdir -p /tmp/a/b` into a non-recursive mkdir that fails when an intermediate dir is missing).
uu_builtin!(Mkdir, "mkdir", uu_mkdir::uumain);

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
    ]
}
