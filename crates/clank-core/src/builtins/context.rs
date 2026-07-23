//! The Brush-registered `context` builtin — `context` inside NESTED execution contexts.
//!
//! Top-level `context ...` lines are intercepted in `Session::eval_line` (preserving the
//! "inspection output is not recorded back into the transcript" rule) and never reach Brush. But
//! Brush dispatch has its own resolution inside `$(...)`, pipelines, `xargs`, and `eval` — where
//! `context` used to be an unknown word that fell through to (unsupported) external exec. This
//! builtin fills that hole, making the README's composition idioms real: `S=$(context show)`,
//! `context show | head`.
//!
//! It reaches the session transcript through the thread-local slot `Session` installs per line
//! ([`crate::install_transcript`]). On wasm, execution is single-threaded and inline, so the slot
//! always resolves. On native, `$()`/pipeline stages run on worker threads that can't see the
//! slot — those error honestly rather than silently operating on nothing.

use std::io::Write;

use brush_core::builtins::{ContentOptions, ContentType, Registration, SimpleCommand};
use brush_core::commands::ExecutionContext;
use brush_core::extensions::ShellExtensions;
use brush_core::{Error, ExecutionResult};

pub(crate) struct Context;

impl SimpleCommand for Context {
    fn get_content(
        name: &str,
        content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, Error> {
        match content_type {
            ContentType::ShortDescription => {
                Ok(format!("{name} - manage the session transcript (show/clear/budget/trim/summarize)\n"))
            }
            ContentType::ShortUsage => {
                Ok(format!("{name}: {name} [show|clear|budget [n]|trim <n>|summarize]\n"))
            }
            ContentType::DetailedHelp => Ok(format!(
                "{name} - manage the session transcript as a first-class value\n\n\
                 context show — print the session transcript\n\
                 context clear — discard the session transcript\n\
                 context budget [n] — show or set the transcript token budget\n\
                 context trim <n> — drop the oldest n transcript entries\n\
                 context summarize — print an AI summary of the transcript (needs the model; \
                 top-level only, confirms unless run with sudo)\n"
            )),
            ContentType::ManPage => brush_core::error::unimp("man page not yet implemented"),
        }
    }

    fn execute<SE, I, S>(context: ExecutionContext<'_, SE>, args: I) -> Result<ExecutionResult, Error>
    where
        SE: ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    {
        let argv: Vec<String> = args.skip(1).map(|s| s.as_ref().to_string()).collect();
        let Some(transcript) = crate::active_transcript() else {
            let _ = writeln!(
                context.stderr(),
                "context: the session transcript is not reachable from this execution context"
            );
            return Ok(ExecutionResult::new(1));
        };
        let out = crate::apply_context(
            &mut transcript.lock().unwrap(),
            argv.iter().map(String::as_str),
        );
        let _ = context.stdout().write_all(&out);
        Ok(ExecutionResult::new(0))
    }
}

pub(crate) fn builtins<SE: ShellExtensions>() -> Vec<(String, Registration<SE>)> {
    use crate::builtins::helpshim::simple_builtin_with_help;
    vec![("context".into(), simple_builtin_with_help::<Context, SE>())]
}
