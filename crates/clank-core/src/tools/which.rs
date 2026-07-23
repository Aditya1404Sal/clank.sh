//! The `which` builtin: file-backed command resolution.
//!
//! The README splits resolution into two authorities: `type` (Brush's own builtin, kept as-is) is
//! authoritative for *all* commands — builtins, aliases, functions, and `$PATH` files — while
//! `which` finds **file-backed commands only**. clank has no `which` from Brush, so this is a small
//! hand-written [`SimpleCommand`] (like [`ps`](crate::ps)) that reports only real executable files
//! found on `$PATH`, ignoring builtins/aliases/functions.
//!
//! **Why it walks `$PATH` itself instead of reusing `Shell::find_executables_in_path`:** Brush's
//! wasm `PathExt::executable()` returns `true` unconditionally (no existence check on wasip2), so
//! `find_executables_in_path` yields *phantom* paths on the agent — `which foo` would report
//! `/usr/local/bin/foo` for a command that doesn't exist. `which` must not lie, so it reads `$PATH`
//! and checks each candidate with `Path` existence (which *does* work on the agent's real per-agent
//! fs). Nonexistent `$PATH` dirs (all of `/usr/lib/{mcp,agents,prompts}/bin` until `grease`
//! populates them) simply yield no matches; nothing errors.

use std::io::Write;

use brush_core::builtins::{ContentOptions, ContentType, Registration, SimpleCommand};
use brush_core::commands::ExecutionContext;
use brush_core::extensions::ShellExtensions;
use brush_core::{Error, ExecutionResult};

use crate::manifest::Manifest;

pub(crate) struct Which;

impl Which {
    pub(crate) const NAME: &'static str = "which";
    pub(crate) const SYNOPSIS: &'static str = "locate a file-backed command on $PATH";
}

impl SimpleCommand for Which {
    fn get_content(
        name: &str,
        content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, Error> {
        match content_type {
            ContentType::ShortDescription => Ok(format!("{name} - {}\n", Which::SYNOPSIS)),
            ContentType::ShortUsage => Ok(format!("{name}: {name} <name>...\n")),
            ContentType::DetailedHelp => {
                Ok(format!("{name} - {}\n\n(clank builtin)\n", Which::SYNOPSIS))
            }
            ContentType::ManPage => brush_core::error::unimp("man page not yet implemented"),
        }
    }

    fn execute<SE, I, S>(context: ExecutionContext<'_, SE>, args: I) -> Result<ExecutionResult, Error>
    where
        SE: ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    {
        // Skip argv[0] (the command name Brush passes); the rest are names to resolve.
        let names: Vec<String> = args
            .skip(1)
            .map(|s| s.as_ref().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // Read $PATH from the shell env and split it. Reading before borrowing stdout keeps the
        // immutable borrow of `context.shell` self-contained.
        let path_dirs: Vec<std::path::PathBuf> = context
            .shell
            .env()
            .get_str("PATH", context.shell)
            .map(|p| brush_core::sys::fs::split_paths(p.as_ref()).collect())
            .unwrap_or_default();

        let mut out = context.stdout();
        // Exit status follows POSIX `which`: 0 if every name resolved, 1 if any did not.
        let mut all_found = true;
        for name in &names {
            match resolve_file_backed(&path_dirs, name) {
                Some(path) => {
                    let _ = writeln!(out, "{}", path.display());
                }
                None => all_found = false,
            }
        }

        let code = if names.is_empty() || all_found { 0 } else { 1 };
        Ok(ExecutionResult::new(code))
    }
}

/// The first `<dir>/<name>` across `path_dirs` that is a real, existing file (not a directory). A
/// `name` containing a `/` is treated as a literal path and checked directly, like `which`.
///
/// Uses `Path::exists()` (excluding directories), NOT `is_file()`: on wasip2/Golem the two diverge —
/// `exists()` correctly reports a missing path (verified: `test -e` works on the agent), while
/// `is_file()` returned true for phantom paths, making `which` report files that aren't there.
fn resolve_file_backed(path_dirs: &[std::path::PathBuf], name: &str) -> Option<std::path::PathBuf> {
    let is_real_file = |p: &std::path::Path| p.exists() && !p.is_dir();
    // An explicit path (contains a `/`) isn't searched on $PATH — check it as-is.
    if name.contains('/') {
        let p = std::path::PathBuf::from(name);
        return is_real_file(&p).then_some(p);
    }
    path_dirs.iter().find_map(|dir| {
        let candidate = dir.join(name);
        is_real_file(&candidate).then_some(candidate)
    })
}

/// The `which` builtin registration, for `build_shell`.
pub(crate) fn builtins<SE: ShellExtensions>() -> Vec<(String, Registration<SE>)> {
    use crate::builtins::helpshim::simple_builtin_with_help;
    vec![(Which::NAME.into(), simple_builtin_with_help::<Which, SE>())]
}

/// The `which` manifest. `shell-internal` scope (README classifies `which` with `type`), `Allow`.
pub(crate) fn manifests() -> Vec<Manifest> {
    use crate::manifest::ExecutionScope;
    vec![
        Manifest::builtin(Which::NAME, Which::SYNOPSIS).with_scope(ExecutionScope::ShellInternal)
    ]
}
