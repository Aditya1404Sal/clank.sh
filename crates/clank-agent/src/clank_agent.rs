use clank_embed::{EmbeddedShell, EvalResult};
use golem_rust::{agent_definition, agent_implementation};

/// A durable shell instance. The constructor parameter `name` is the agent identity, so distinct
/// names are isolated instances (each with its own shell state, transcript, and filesystem).
///
/// This is the *shell surface* `golem agent shell` drives — three methods, each returning the
/// positional `EvalResult` record. The engine behind them lives in `clank-embed` (which any Golem
/// agent can embed); this crate is just clank's own agent type wired with the full provider set.
#[agent_definition]
pub trait ClankAgent {
    fn new(name: String) -> Self;

    /// Evaluate a bash-compatible command line and return structured process output. If the command
    /// is `prompt-user`, the shell surfaces the question in `pending_prompt` and returns immediately
    /// (it never blocks); deliver the human's response via `answer_prompt`.
    async fn eval(&mut self, cmd: String) -> EvalResult;

    /// Deliver a response to an outstanding `prompt-user` question (see `eval`'s `pending_prompt`).
    /// `response` is the human's answer. Returns the resolved result (the response on stdout,
    /// exit 0). Errors if no prompt is outstanding, or leaves the prompt pending if the answer
    /// isn't an allowed choice.
    async fn answer_prompt(&mut self, response: String) -> EvalResult;

    /// Abort an outstanding `prompt-user` question (the Ctrl-C convention — exit 130). Separate from
    /// `answer_prompt` so an empty string stays a valid *answer* rather than an abort signal.
    async fn abort_prompt(&mut self) -> EvalResult;
}

pub struct ClankAgentImpl {
    _name: String,
    /// The embedded shell — durable across invocations; the Session builds lazily on first eval
    /// (`Session::new` is async, this constructor is sync) with clank's full provider set.
    shell: EmbeddedShell,
}

#[agent_implementation]
impl ClankAgent for ClankAgentImpl {
    fn new(name: String) -> Self {
        Self {
            _name: name,
            shell: EmbeddedShell::with_default_golem_providers(),
        }
    }

    async fn eval(&mut self, cmd: String) -> EvalResult {
        self.shell.eval(&cmd).await
    }

    async fn answer_prompt(&mut self, response: String) -> EvalResult {
        self.shell.answer(Some(response)).await
    }

    async fn abort_prompt(&mut self) -> EvalResult {
        self.shell.answer(None).await
    }
}
