//! `--help` plumbing shared across clank's three separate `--help` mechanisms.
//!
//! There are three, solving the same concern ("how does a command answer `--help`") at three
//! different layers, because each layer sees the line/argv at a different stage of dispatch and
//! none can reach the other two directly:
//!
//! - [`builtins::typecmd::help_for`](crate::builtins::typecmd::help_for) — the static
//!   clank-intercepted commands (`prompt-user`/`curl`/`wget`/`context`/…), resolved from the raw
//!   line BEFORE Brush ever sees it.
//! - `Session::pkg_help_for` — installed grease packages (prompts/scripts/agents), same layer.
//! - [`WithHelp`] (this module) — hand-rolled Brush `SimpleCommand`s, resolved from `argv` INSIDE
//!   Brush's own dispatch, after tokenization and routing already happened.
//!
//! Full unification isn't behaviour-preserving: the two line-string paths dequote+tokenize the
//! line differently (`typecmd`'s own word-splitter keeps going past a shell operator and ignores
//! it; [`dequote_words`] — the splitter the session layer uses, which lives here beside the other
//! shared line-word helpers — declines the whole line the moment one appears), and `WithHelp`
//! checks only the *immediate* first argument (accepting `-h` too) rather than scanning for
//! `--help` anywhere in the tail — a narrower rule suited to running inside a `SimpleCommand`
//! rather than over an unparsed line. Collapsing any of that would change what actually matches.
//! What genuinely is common — and is unified here — is: (1) `sudo` must be skipped before deciding
//! what a line asks for help on (`sudo <cmd> --help` must answer identically to `<cmd> --help`,
//! never fall through to `<cmd>`'s own arg parser), and (2) both line-string paths test the same
//! "does the tail contain a literal `--help`" predicate. See [`skip_leading_sudo`]/
//! [`asks_for_help`], used by both `typecmd::help_for` and `Session::pkg_help_for`. `WithHelp` has
//! no `sudo` to skip — the authz gate strips a leading `sudo` before any line reaches Brush's
//! dispatch, so a `SimpleCommand::execute` never sees it — which is exactly why it can't share the
//! other two's detection wholesale either.
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
use brush_parser::{tokenize_str, unquote_str, Token};
use std::io::Write;

/// Skip a leading `sudo` token from an already-dequoted word list. `sudo` only pre-authorizes a
/// command, so it must not change what `--help` (or a grease package's reserved bare `help`
/// subcommand) resolves to. Shared by the two pre-Brush `--help` paths — `typecmd::help_for` and
/// `Session::pkg_help_for` — which otherwise duplicated this exact match arm; see the module doc
/// for why `WithHelp` below does not share it.
#[must_use]
pub fn skip_leading_sudo(words: &[String]) -> &[String] {
    match words.split_first() {
        Some((first, rest)) if first == "sudo" => rest,
        _ => words,
    }
}

/// Whether `words` (the words AFTER the command name, already sudo-stripped) contains a literal
/// `--help`. Shared by the same two call sites as [`skip_leading_sudo`]; see the module doc for why
/// `WithHelp`'s own (narrower, positional) check isn't merged into this.
#[must_use]
pub fn asks_for_help(words: &[String]) -> bool {
    words.iter().any(|w| w == "--help")
}

/// The dequoted words of a **top-level** (operator-free) `line`. `None` if the line doesn't tokenize,
/// is empty, OR contains any shell operator (`|`/`;`/`&&`/redirects) — so a nested use falls through
/// to Brush (and its honest stub). Used by grease's prompt dispatch (a prompt can't run in a pipe/`$()`
/// — it makes an LLM call, the Wall-C wall). Public sibling of `leading_words`.
#[must_use]
pub fn dequote_words(line: &str) -> Option<Vec<String>> {
    let tokens = tokenize_str(line).ok()?;
    if tokens.iter().any(|t| matches!(t, Token::Operator(_, _))) {
        return None;
    }
    let words: Vec<String> = tokens
        .into_iter()
        .filter_map(|t| match t {
            Token::Word(s, _) => Some(unquote_str(&s)),
            Token::Operator(_, _) => None,
        })
        .collect();
    (!words.is_empty()).then_some(words)
}

/// A `SimpleCommand` wrapper that serves `--help`/`-h` (as the sole first argument) from the
/// wrapped builtin's `DetailedHelp` content before its own arg parsing can mangle it.
pub struct WithHelp<T>(std::marker::PhantomData<T>);

impl<T: SimpleCommand> SimpleCommand for WithHelp<T> {
    fn get_content(
        name: &str,
        content_type: ContentType,
        options: &ContentOptions,
    ) -> Result<String, Error> {
        T::get_content(name, content_type, options)
    }

    // similar_names: `args`/`argv` are the conventional arg-iterator / arg-vector pair.
    #[allow(clippy::similar_names)]
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
        if matches!(argv.get(1).map(String::as_str), Some("--help" | "-h")) {
            let name = argv.first().map_or("", String::as_str);
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
#[must_use]
pub fn simple_builtin_with_help<T, SE>() -> Registration<SE>
where
    T: SimpleCommand + Send + Sync,
    SE: ShellExtensions,
{
    brush_core::builtins::simple_builtin::<WithHelp<T>, SE>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal `SimpleCommand` that would error on `--help` if it ever saw it.
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

        // similar_names: `args`/`argv` are the conventional arg-iterator / arg-vector pair.
        #[allow(clippy::similar_names)]
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
        let help = WithHelp::<Probe>::get_content(
            "probe",
            ContentType::DetailedHelp,
            &ContentOptions::default(),
        )
        .unwrap();
        assert_eq!(help, "probe - detailed probe help");
    }
}
