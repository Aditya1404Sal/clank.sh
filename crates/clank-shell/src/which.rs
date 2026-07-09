//! The `which` builtin: file-backed command resolution.
//!
//! The README splits resolution into two authorities: `type` (Brush's own builtin, kept as-is) is
//! authoritative for *all* commands — builtins, aliases, functions, and `$PATH` files — while
//! `which` finds **file-backed commands only**. clank has no `which` from Brush, so this is a small
//! hand-written [`SimpleCommand`] (like [`ps`](crate::ps)) that reports only real executable files
//! found on `$PATH`, ignoring builtins/aliases/functions.
//!
//! It reuses Brush's own path search (`Shell::find_executables_in_path`) — the same machinery
//! `type` uses — so `which` is a strict subset of `type`'s lookup surface, not a reimplementation.
//! `$PATH` entries that don't exist (all of `/usr/lib/{mcp,agents,prompts}/bin` until `grease`
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

        let mut out = context.stdout();
        // Exit status follows POSIX `which`: 0 if every name resolved, 1 if any did not.
        let mut all_found = true;
        for name in &names {
            // File-backed only: the first real executable file on $PATH. Builtins/aliases/functions
            // are intentionally NOT reported (that's `type`'s job).
            match context.shell.find_executables_in_path(name).next() {
                Some(path) => {
                    let _ = writeln!(out, "{}", path.display());
                }
                None => {
                    all_found = false;
                }
            }
        }

        let code = if names.is_empty() || all_found { 0 } else { 1 };
        Ok(ExecutionResult::new(code))
    }
}

/// The `which` builtin registration, for `build_shell`.
pub(crate) fn builtins<SE: ShellExtensions>() -> Vec<(String, Registration<SE>)> {
    use brush_core::builtins::simple_builtin;
    vec![(Which::NAME.into(), simple_builtin::<Which, SE>())]
}

/// The `which` manifest. `shell-internal` scope (README classifies `which` with `type`), `Allow`.
pub(crate) fn manifests() -> Vec<Manifest> {
    use crate::manifest::ExecutionScope;
    vec![
        Manifest::builtin(Which::NAME, Which::SYNOPSIS).with_scope(ExecutionScope::ShellInternal)
    ]
}
