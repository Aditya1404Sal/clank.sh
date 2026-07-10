use clank_shell::session::{LineResult, Session};
use golem_rust::{Schema, agent_definition, agent_implementation};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Schema, Serialize, Deserialize)]
pub struct EvalResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u8,
    /// Set when this call surfaced a `prompt-user` question the shell is now awaiting a response
    /// to. The caller must collect a human answer and deliver it via `answer_prompt` — the shell
    /// never blocks. `None` for every ordinary command.
    pub pending_prompt: Option<PendingPromptView>,
}

/// The wire view of a pending `prompt-user` question surfaced to the caller.
#[derive(Clone, Debug, Schema, Serialize, Deserialize)]
pub struct PendingPromptView {
    /// The question (with any piped markdown prepended) to present to the human.
    pub question: String,
    /// If present, the response must be one of these values.
    pub choices: Option<Vec<String>>,
}

/// A durable shell instance. The constructor parameter `name` is the agent identity, so distinct
/// names are isolated instances (each with its own shell state, transcript, and filesystem).
#[agent_definition]
pub trait ClankAgent {
    fn new(name: String) -> Self;

    /// Evaluate a bash-compatible command line and return structured process output. If the command
    /// is `prompt-user`, the shell surfaces the question in `pending_prompt` and returns immediately
    /// (it never blocks); deliver the human's response via `answer_prompt`.
    async fn eval(&mut self, cmd: String) -> EvalResult;

    /// Deliver a response to an outstanding `prompt-user` question (see `eval`'s `pending_prompt`).
    /// `response` is the human's answer, or an empty/absent value via `abort` for an abort. Returns
    /// the resolved result (the response on stdout, exit 0; or exit 130 on abort). Errors if no
    /// prompt is outstanding, or leaves the prompt pending if the answer isn't an allowed choice.
    async fn answer_prompt(&mut self, response: String) -> EvalResult;

    /// Abort an outstanding `prompt-user` question (the Ctrl-C convention — exit 130). Separate from
    /// `answer_prompt` so an empty string stays a valid *answer* rather than an abort signal.
    async fn abort_prompt(&mut self) -> EvalResult;

    /// Run one shell command line and return its output. Mutates the durable session (shell state
    /// and transcript persist across invocations).
    async fn run_line(&mut self, cmd: String) -> String;
}

pub struct ClankAgentImpl {
    _name: String,
    /// The live shell session — durable across invocations. Built lazily on first eval
    /// because `Session::new` is async and the constructor is sync.
    session: Option<Session>,
}

#[agent_implementation]
impl ClankAgent for ClankAgentImpl {
    fn new(name: String) -> Self {
        Self {
            _name: name,
            session: None,
        }
    }

    async fn eval(&mut self, cmd: String) -> EvalResult {
        if let Err(result) = self.ensure_session().await {
            return result;
        }
        let result = self.session.as_mut().unwrap().eval_line(&cmd).await;
        eval_result(result)
    }

    async fn answer_prompt(&mut self, response: String) -> EvalResult {
        if let Err(result) = self.ensure_session().await {
            return result;
        }
        let result = self
            .session
            .as_mut()
            .unwrap()
            .answer_prompt(Some(response))
            .await;
        eval_result(result)
    }

    async fn abort_prompt(&mut self) -> EvalResult {
        if let Err(result) = self.ensure_session().await {
            return result;
        }
        let result = self.session.as_mut().unwrap().answer_prompt(None).await;
        eval_result(result)
    }

    async fn run_line(&mut self, cmd: String) -> String {
        let result = self.eval(cmd).await;
        format!("{}{}", result.stdout, result.stderr)
    }
}

impl ClankAgentImpl {
    /// Ensure the durable session is built (lazily, since `Session::new` is async and the
    /// constructor is sync). Returns `Err(EvalResult)` carrying a clean error if startup failed.
    async fn ensure_session(&mut self) -> Result<(), EvalResult> {
        if self.session.is_none() {
            match Session::new().await {
                Ok(mut s) => {
                    // Install the durable Anthropic provider so `ask` can reach the model. Only the
                    // agent build has the Golem-host LLM bindings; native leaves `ask` unconfigured.
                    s.set_ask_provider(Box::new(crate::ask_provider::DurableAnthropicProvider));
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
        Ok(())
    }
}

/// Map a shell [`LineResult`] to the wire [`EvalResult`].
fn eval_result(result: LineResult) -> EvalResult {
    EvalResult {
        stdout: String::from_utf8_lossy(&result.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&result.stderr).into_owned(),
        exit_code: result.exit_code,
        pending_prompt: result.pending_prompt.map(|p| PendingPromptView {
            question: p.question,
            choices: p.choices,
        }),
    }
}
