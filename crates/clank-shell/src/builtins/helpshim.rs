//! A `--help` shim for hand-rolled `SimpleCommand` builtins.
//!
//! Brush's `exec_simple_builtin_impl` passes ALL arguments straight to `SimpleCommand::execute` —
//! unlike clap-derived `Command` builtins, nothing answers `--help` for a `SimpleCommand`, so each
//! one mishandled it in its own way (`model --help` → "unknown subcommand '--help'", exit 2; found
//! live in the demo). [`WithHelp`] wraps any `SimpleCommand`: when the FIRST argument is exactly
//! `--help` (or `-h`), it prints the builtin's own `DetailedHelp` content and exits 0; anything
//! else delegates untouched.
//!
//! Deliberately NOT applied to the uu-backed coreutils/texttools builtins — uu's clap answers
//! `--help` itself with richer output (pinned by `help-intercepts.clank`).

use brush_core::builtins::{ContentOptions, ContentType, Registration, SimpleCommand};
use brush_core::commands::ExecutionContext;
use brush_core::extensions::ShellExtensions;
use brush_core::{Error, ExecutionResult};
use std::io::Write;

/// A `SimpleCommand` wrapper that serves `--help`/`-h` (as the sole first argument) from the
/// wrapped builtin's `DetailedHelp` content before its own arg parsing can mangle it.
pub(crate) struct WithHelp<T>(std::marker::PhantomData<T>);

impl<T: SimpleCommand> SimpleCommand for WithHelp<T> {
    fn get_content(
        name: &str,
        content_type: ContentType,
        options: &ContentOptions,
    ) -> Result<String, Error> {
        T::get_content(name, content_type, options)
    }

    fn execute<SE, I, S>(context: ExecutionContext<'_, SE>, args: I) -> Result<ExecutionResult, Error>
    where
        SE: ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    {
        let argv: Vec<String> = args.map(|s| s.as_ref().to_string()).collect();
        if matches!(argv.get(1).map(String::as_str), Some("--help" | "-h")) {
            let name = argv.first().map(String::as_str).unwrap_or("");
            let help = T::get_content(name, ContentType::DetailedHelp, &ContentOptions::default())?;
            let mut out = context.stdout();
            let _ = out.write_all(help.as_bytes());
            if !help.ends_with('\n') {
                let _ = out.write_all(b"\n");
            }
            let _ = out.flush();
            return Ok(ExecutionResult::new(0));
        }
        T::execute(context, argv.into_iter())
    }
}

/// [`brush_core::builtins::simple_builtin`] with the `--help` shim applied — the registration
/// helper every hand-rolled `SimpleCommand` should use.
pub(crate) fn simple_builtin_with_help<T, SE>() -> Registration<SE>
where
    T: SimpleCommand + Send + Sync,
    SE: ShellExtensions,
{
    brush_core::builtins::simple_builtin::<WithHelp<T>, SE>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal SimpleCommand that would error on `--help` if it ever saw it.
    struct Probe;

    impl SimpleCommand for Probe {
        fn get_content(
            name: &str,
            content_type: ContentType,
            _options: &ContentOptions,
        ) -> Result<String, Error> {
            match content_type {
                ContentType::DetailedHelp => Ok(format!("{name} - detailed probe help")),
                _ => Ok(String::new()),
            }
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
            let argv: Vec<String> = args.map(|s| s.as_ref().to_string()).collect();
            // The shim must have consumed --help before we get here.
            assert_ne!(argv.get(1).map(String::as_str), Some("--help"));
            let _ = writeln!(context.stdout(), "ran with {} args", argv.len() - 1);
            Ok(ExecutionResult::new(0))
        }
    }

    // The wrapper's behavior is proven end-to-end through the session tests (`model --help`,
    // `which --help`, `ps --help` in help-simple-builtins.clank); this module only pins that
    // get_content passes through unchanged, since ExecutionContext cannot be constructed here.
    #[test]
    fn get_content_passes_through() {
        let help =
            WithHelp::<Probe>::get_content("probe", ContentType::DetailedHelp, &ContentOptions::default())
                .unwrap();
        assert_eq!(help, "probe - detailed probe help");
    }
}
