//! [`EmbeddedShell`] — a lazily-built shell [`Session`] plus the [`LineResult`]→[`EvalResult`]
//! mapping, i.e. everything between an agent's three shell-surface methods and the shell core.

use clank_shell::session::{LineResult, Session};

use crate::wire::{EvalResult, PendingPromptView};

/// A shell session embedded in a Golem agent instance.
///
/// Construction is cheap and sync (agent constructors are sync); the async [`Session::new`] runs
/// lazily on the first `eval`/`answer`, and any deferred setup (provider installation) is applied to
/// the Session at that moment — not at construction. Startup failure is reported as an honest
/// `EvalResult { exit_code: 1, .. }` rather than a panic, so a broken environment still yields a
/// well-formed wire response.
///
/// One `EmbeddedShell` per process: the underlying Session assumes it owns process-global state
/// (the working directory, `/var/log`, the grease store). Under Golem this holds by construction —
/// one agent instance = one worker = one process.
pub struct EmbeddedShell {
    /// The live session — durable across invocations (its in-memory state is rebuilt by oplog
    /// replay re-running the same calls). Built lazily because `Session::new` is async.
    session: Option<Session>,
    /// Deferred provider/setup hook, applied once when the Session is first built. `FnOnce` with no
    /// `Send` bound: the provider seams are `?Send` and the wasm agent is single-threaded.
    setup: Option<Box<dyn FnOnce(&mut Session)>>,
}

impl EmbeddedShell {
    /// A bare shell: the full command surface over this agent's own filesystem; `ask`/`mcp`/cluster
    /// commands degrade to honest errors. NOTE: on Golem prefer [`Self::with_durable_log_sink`] —
    /// the default log sink appends, which duplicates `/var/log` lines under oplog replay.
    pub fn new() -> Self {
        Self { session: None, setup: None }
    }

    /// A shell with a deferred setup hook: `setup` runs against the `Session` when it is first
    /// built (lazily, inside the first `eval`/`answer`). This is the one extension point — install
    /// any mix of providers via the `Session::set_*` seams:
    ///
    /// ```ignore
    /// EmbeddedShell::with_setup(|s| {
    ///     s.set_log_sink(std::sync::Arc::new(clank_embed::log_sink::DurableLogSink::new()));
    ///     s.set_ask_provider(Box::new(MyProvider));
    /// })
    /// ```
    pub fn with_setup(setup: impl FnOnce(&mut Session) + 'static) -> Self {
        Self { session: None, setup: Some(Box::new(setup)) }
    }

    /// The minimal *correct* Golem embed: a bare shell plus the replay-safe `/var/log` sink (an
    /// idempotent whole-file writer — the default sink's raw appends duplicate lines when the oplog
    /// replays; see [`crate::log_sink`]).
    pub fn with_durable_log_sink() -> Self {
        Self::with_setup(|s| {
            s.set_log_sink(std::sync::Arc::new(crate::log_sink::DurableLogSink::new()));
        })
    }

    /// The full clank provider set — what `clank:agent` itself runs: the durable Anthropic `ask`
    /// provider, the wstd MCP transport, the WasmRpc agent invoker, the `golem:api` cluster
    /// interface, and the replay-safe log sink.
    #[cfg(feature = "providers")]
    pub fn with_default_golem_providers() -> Self {
        Self::with_setup(|s| {
            // The durable Anthropic provider so `ask` can reach the model (reads ANTHROPIC_API_KEY
            // from the agent environment; absent ⇒ `ask` reports not-configured).
            s.set_ask_provider(Box::new(crate::ask_provider::DurableAnthropicProvider));
            // The durable wstd HTTP transport so `mcp` can reach servers.
            s.set_mcp_http(Box::new(crate::mcp_http::WstdMcpHttp));
            // The durable WasmRpc invoker so grease-installed Golem agents can be invoked.
            s.set_agent_invoker(Box::new(crate::agent_invoker::WasmRpcInvoker));
            // The durable Golem cluster interface backing the `golem` command.
            s.set_golem_cluster(Box::new(crate::golem_cluster::GolemApiCluster));
            // The replay-safe /var/log sink (whole-file rewrite; appends duplicate under replay).
            s.set_log_sink(std::sync::Arc::new(crate::log_sink::DurableLogSink::new()));
        })
    }

    /// Evaluate one command line — the body of the agent's `eval` method.
    pub async fn eval(&mut self, cmd: &str) -> EvalResult {
        match self.ensure().await {
            Ok(session) => eval_result(session.eval_line(cmd).await),
            Err(failure) => failure,
        }
    }

    /// Resolve an outstanding `prompt-user` question — the body of `answer_prompt`
    /// (`Some(response)`) and `abort_prompt` (`None`; the Ctrl-C convention, exit 130). Two wire
    /// methods rather than an empty-string sentinel so `""` stays a valid *answer*.
    pub async fn answer(&mut self, response: Option<String>) -> EvalResult {
        match self.ensure().await {
            Ok(session) => eval_result(session.answer_prompt(response).await),
            Err(failure) => failure,
        }
    }

    /// Build the Session on first use (applying the deferred setup), or report the startup failure
    /// as a well-formed result.
    async fn ensure(&mut self) -> Result<&mut Session, EvalResult> {
        if self.session.is_none() {
            match Session::new().await {
                Ok(mut s) => {
                    if let Some(setup) = self.setup.take() {
                        setup(&mut s);
                    }
                    self.session = Some(s);
                }
                Err(e) => {
                    return Err(EvalResult {
                        stdout: String::new(),
                        stderr: format!("clank: failed to start shell: {e}\n"),
                        exit_code: 1,
                        pending_prompt: None,
                    });
                }
            }
        }
        Ok(self.session.as_mut().unwrap())
    }
}

impl Default for EmbeddedShell {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a shell [`LineResult`] to the wire [`EvalResult`].
fn eval_result(result: LineResult) -> EvalResult {
    EvalResult {
        // Move the bytes into a String on the valid-UTF-8 common path (no copy); only allocate a lossy
        // copy on the rare invalid-byte path.
        stdout: String::from_utf8(result.stdout)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()),
        stderr: String::from_utf8(result.stderr)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()),
        exit_code: result.exit_code,
        pending_prompt: result.pending_prompt.map(|p| PendingPromptView {
            question: p.question,
            choices: p.choices,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clank_shell::Flow;

    /// Drive a future on a fresh current-thread runtime (mirrors how `Session` runs natively).
    fn on_rt<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn mapper_moves_streams_and_maps_the_prompt() {
        let mapped = eval_result(LineResult {
            stdout: b"out\n".to_vec(),
            stderr: b"err\n".to_vec(),
            exit_code: 3,
            flow: Flow::Continue,
            pending_prompt: Some(clank_shell::builtins::promptuser::PendingPrompt {
                question: "Which?".to_string(),
                choices: Some(vec!["a".to_string(), "b".to_string()]),
                secret: false,
            }),
        });
        assert_eq!(mapped.stdout, "out\n");
        assert_eq!(mapped.stderr, "err\n");
        assert_eq!(mapped.exit_code, 3);
        let p = mapped.pending_prompt.expect("prompt mapped");
        assert_eq!(p.question, "Which?");
        assert_eq!(p.choices.as_deref(), Some(&["a".to_string(), "b".to_string()][..]));
    }

    #[test]
    fn mapper_is_lossy_on_invalid_utf8_rather_than_panicking() {
        let mapped = eval_result(LineResult {
            stdout: vec![0xff, 0xfe, b'x'],
            stderr: Vec::new(),
            exit_code: 0,
            flow: Flow::Continue,
            pending_prompt: None,
        });
        // The replacement character marks the bad bytes; the valid tail survives.
        assert!(mapped.stdout.contains('\u{FFFD}'));
        assert!(mapped.stdout.ends_with('x'));
    }

    #[test]
    fn bare_shell_evaluates_a_command() {
        on_rt(async {
            let mut shell = EmbeddedShell::new();
            let result = shell.eval("echo embedded-hello").await;
            assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
            assert!(result.stdout.contains("embedded-hello"), "stdout: {}", result.stdout);
            assert!(result.pending_prompt.is_none());
        });
    }

    #[test]
    fn setup_hook_runs_once_on_first_eval() {
        on_rt(async {
            use std::rc::Rc;
            use std::cell::Cell;
            let ran = Rc::new(Cell::new(0));
            let seen = ran.clone();
            let mut shell = EmbeddedShell::with_setup(move |_s| seen.set(seen.get() + 1));
            assert_eq!(ran.get(), 0, "setup is deferred, not run at construction");
            shell.eval("echo one").await;
            shell.eval("echo two").await;
            assert_eq!(ran.get(), 1, "setup runs exactly once, at first eval");
        });
    }

    /// The full pause round-trip, natively: surface → abort (130) → surface → answer (0).
    #[test]
    fn prompt_cycle_surfaces_aborts_and_answers() {
        on_rt(async {
            let mut shell = EmbeddedShell::new();

            let surfaced = shell.eval(r#"prompt-user "Which env?" --choices dev,prod"#).await;
            assert_eq!(surfaced.exit_code, 0, "stderr: {}", surfaced.stderr);
            let p = surfaced.pending_prompt.expect("question surfaced");
            assert_eq!(p.question, "Which env?");
            assert_eq!(p.choices.as_deref().map(|c| c.len()), Some(2));

            let aborted = shell.answer(None).await;
            assert_eq!(aborted.exit_code, 130, "abort follows the Ctrl-C convention");

            let surfaced = shell.eval(r#"prompt-user "Again?""#).await;
            assert!(surfaced.pending_prompt.is_some());
            let answered = shell.answer(Some("ok".to_string())).await;
            assert_eq!(answered.exit_code, 0, "stderr: {}", answered.stderr);
            assert!(answered.pending_prompt.is_none(), "prompt resolved");
        });
    }
}
