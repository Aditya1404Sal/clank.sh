//! Honest-error Brush builtins for the Session-intercepted commands in NESTED contexts.
//!
//! `curl`, `wget`, `ask`, and `kill` are Session-layer interceptions: curl/wget/ask must await
//! their async work directly under the Golem SDK's `wstd::block_on` (the WASI-HTTP reactor is not
//! live inside `execute`'s nested runtime — the "Wall C shape"), and `kill` mutates Session state
//! (the bg-job mapping, the pending prompt). Top-level lines never reach Brush for these names.
//!
//! But inside `$(...)`, pipelines, `xargs`, and `eval`, Brush dispatches directly — and these
//! words used to fall through to external exec, dying with the misleading "operation not
//! supported on this platform". These stubs replace that with the README's honest-constraints
//! answer: a clear message naming the actual limitation and exit 1. (On native they also shadow
//! Brush's unix `kill` builtin, keeping kill semantics synthetic and identical on both targets.)

use std::io::Write;

use brush_core::builtins::{ContentOptions, ContentType, Registration, SimpleCommand};
use brush_core::commands::ExecutionContext;
use brush_core::extensions::ShellExtensions;
use brush_core::{Error, ExecutionResult};

macro_rules! session_stub {
    ($ty:ident, $name:literal) => {
        pub(crate) struct $ty;

        impl SimpleCommand for $ty {
            fn get_content(
                name: &str,
                content_type: ContentType,
                _options: &ContentOptions,
            ) -> Result<String, Error> {
                // Serve the real manifest help (same source as `cat /bin/<name>`), so `help`
                // inside Brush matches the top-level surfaces.
                let help = crate::runtime::binfs::registry()
                    .get($name)
                    .map(|m| m.help_text.clone())
                    .unwrap_or_else(|| $name.to_string());
                match content_type {
                    ContentType::ShortDescription => Ok(format!("{name} - session command\n")),
                    ContentType::ShortUsage => Ok(format!("{name}: see `{name} --help`\n")),
                    ContentType::DetailedHelp => Ok(format!("{help}\n")),
                    ContentType::ManPage => {
                        brush_core::error::unimp("man page not yet implemented")
                    }
                }
            }

            fn execute<SE, I, S>(
                context: ExecutionContext<'_, SE>,
                _args: I,
            ) -> Result<ExecutionResult, Error>
            where
                SE: ShellExtensions,
                I: Iterator<Item = S>,
                S: AsRef<str>,
            {
                let _ = writeln!(
                    context.stderr(),
                    "{name}: only available as a top-level command (it runs at the session layer); \
                     not usable inside $(...), xargs, or eval on this build",
                    name = $name,
                );
                // `ask` DOES work as the final stage of a pipeline (`cat x | ask \"…\"`) — the
                // session pre-extracts the upstream. This stub only fires when `ask` is NOT the tail
                // (mid-pipe or inside a substitution); point the user at the forms that work.
                if $name == "ask" {
                    let _ = writeln!(
                        context.stderr(),
                        "ask: to feed input to ask, put it as the LAST pipeline stage \
                         (cat x | ask \"…\") or inline it (ask \"$(cat x)\")",
                    );
                }
                // curl/wget DO work as the FIRST pipeline stage (`curl -s URL | jq .x`) — the
                // session runs the HTTP and feeds the response to the downstream. This stub only
                // fires for the other shapes (mid-pipe, after `&&`/`;`, inside a substitution).
                if $name == "curl" || $name == "wget" {
                    let _ = writeln!(
                        context.stderr(),
                        "{name}: to pipe a response, put {name} FIRST in the pipeline \
                         ({name} -s URL | jq .x), or write to a file (-o f) and read that",
                        name = $name,
                    );
                }
                Ok(ExecutionResult::new(1))
            }
        }
    };
}

session_stub!(CurlStub, "curl");
session_stub!(WgetStub, "wget");
session_stub!(AskStub, "ask");
session_stub!(KillStub, "kill");
session_stub!(McpStub, "mcp");
session_stub!(GreaseStub, "grease");
session_stub!(GolemStub, "golem");

pub(crate) fn builtins<SE: ShellExtensions>() -> Vec<(String, Registration<SE>)> {
    use brush_core::builtins::simple_builtin;
    vec![
        ("curl".into(), simple_builtin::<CurlStub, SE>()),
        ("wget".into(), simple_builtin::<WgetStub, SE>()),
        ("ask".into(), simple_builtin::<AskStub, SE>()),
        ("kill".into(), simple_builtin::<KillStub, SE>()),
        ("mcp".into(), simple_builtin::<McpStub, SE>()),
        ("grease".into(), simple_builtin::<GreaseStub, SE>()),
        ("golem".into(), simple_builtin::<GolemStub, SE>()),
    ]
}
