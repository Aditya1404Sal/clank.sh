//! T1 PROBE — THROWAWAY. `probe-tool`: a real Golem tool call from inside a synchronous Brush
//! builtin. Delete once `dev-docs/research/agent-tools-probes.md` is written.
//!
//! Same shape as the nested-`block_on` spike: clank-core cannot depend on golem-rust, so the
//! builtin only forwards argv to a thread-local hook that the embed installs on the wasm target.
//! Being an ordinary `SimpleCommand` is the point — Brush dispatches it in any position, so
//! `probe-tool produce 2 hi | tr a-z A-Z` and `$(probe-tool no-stream x)` exercise the positions
//! Wall C used to forbid, this time against a real `tool-rpc` call rather than an HTTP stand-in.

use std::cell::RefCell;
use std::io::Write;

use brush_core::builtins::{ContentOptions, ContentType, SimpleCommand};
use brush_core::commands::ExecutionContext;
use brush_core::extensions::ShellExtensions;
use brush_core::{Error, ExecutionResult};

/// argv after the command name → `(stdout bytes, exit code)`, or a message for stderr.
pub type ProbeHook = Box<dyn Fn(&[String]) -> Result<(Vec<u8>, u8), String>>;

thread_local! {
    static HOOK: RefCell<Option<ProbeHook>> = const { RefCell::new(None) };
}

/// Install the hook (wasm embed only; natively the builtin reports exit 4).
pub fn install(hook: ProbeHook) {
    HOOK.with(|h| *h.borrow_mut() = Some(hook));
}

const USAGE: &str = "probe-tool no-stream <value> | produce <count> <text> | fail [--usage] \
                     | capable <path> <text> <tag> | encode <sub> [args...]";

/// The `probe-tool` builtin.
pub struct ProbeTool;

impl SimpleCommand for ProbeTool {
    fn get_content(
        _name: &str,
        _content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, Error> {
        Ok(format!(
            "{USAGE}\n\nT1 throwaway probe of a real Golem tool call.\n"
        ))
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
        let words: Vec<String> = args.map(|s| s.as_ref().to_string()).collect();
        if words.len() < 2 {
            let _ = writeln!(context.stderr(), "probe-tool: usage: {USAGE}");
            return Ok(ExecutionResult::new(2));
        }
        let rest = &words[1..];
        let outcome = HOOK.with(|h| h.borrow().as_ref().map(|f| f(rest)));
        match outcome {
            None => {
                let _ = writeln!(
                    context.stderr(),
                    "probe-tool: agent tools need a Golem host (no hook on this target)"
                );
                Ok(ExecutionResult::new(4))
            }
            Some(Ok((stdout, code))) => {
                let _ = context.stdout().write_all(&stdout);
                Ok(ExecutionResult::new(code))
            }
            Some(Err(message)) => {
                let _ = writeln!(context.stderr(), "probe-tool: {message}");
                Ok(ExecutionResult::new(1))
            }
        }
    }
}
