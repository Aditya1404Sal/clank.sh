//! Internal coreutils: uutils (`uu_*`) crates registered as clank builtins, so `cat`/`ls`/etc.
//! run **inside** the component instead of forking host programs — matching the README's
//! "core commands resolved internally, no fork/exec" model.
//!
//! Each `uu_*::uumain(args) -> i32` writes to `std::io::stdout()` directly, so to make its
//! output compose with brush's fd table (pipes, redirections, the transcript capture) we
//! redirect the real fd 1/2 onto the `OpenFile`s brush assigned for the command, run `uumain`,
//! then restore them. On wasm there is no `dup2`, so `uumain` writes to the component's real
//! stdout (visible, but not captured) — a documented wasm limitation.
//!
//! `uucore` is patched for `wasm32-wasip2` via `[patch.crates-io]` in the workspace root.

use brush_core::builtins::{ContentOptions, ContentType, Registration, SimpleCommand};
use brush_core::commands::ExecutionContext;
use brush_core::extensions::ShellExtensions;
use brush_core::{ExecutionResult, Error};

/// Run a uutils `uumain` closure with the process's stdout/stderr pointed at the `OpenFile`s
/// brush assigned for this command, so its output lands wherever brush wants it.
#[cfg(not(target_arch = "wasm32"))]
fn run_uu<SE: ShellExtensions>(
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

/// wasm: no `dup2`. `uumain` writes to the component's real stdout (visible, not captured by
/// the transcript). Pipes/redirections aren't available in the sandbox anyway.
#[cfg(target_arch = "wasm32")]
fn run_uu<SE: ShellExtensions>(
    _context: &ExecutionContext<'_, SE>,
    uumain: impl FnOnce() -> i32,
) -> i32 {
    uumain()
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

/// The coreutils builtins to register on the shell, in addition to brush's bash set.
pub(crate) fn builtins<SE: ShellExtensions>() -> Vec<(String, Registration<SE>)> {
    use brush_core::builtins::simple_builtin;
    vec![
        ("cat".into(), simple_builtin::<Cat, SE>()),
        ("ls".into(), simple_builtin::<Ls, SE>()),
        ("wc".into(), simple_builtin::<Wc, SE>()),
        ("head".into(), simple_builtin::<Head, SE>()),
        ("sort".into(), simple_builtin::<Sort, SE>()),
    ]
}