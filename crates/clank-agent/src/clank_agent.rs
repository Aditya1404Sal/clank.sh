use clank_shell::session::Session;
use golem_rust::{Schema, agent_definition, agent_implementation};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Schema, Serialize, Deserialize)]
pub struct EvalResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u8,
}

/// A durable shell instance. The constructor parameter `name` is the agent identity, so distinct
/// names are isolated instances (each with its own shell state, transcript, and filesystem).
#[agent_definition]
pub trait ClankAgent {
    fn new(name: String) -> Self;

    /// Evaluate a bash-compatible command line and return structured process output.
    async fn eval(&mut self, cmd: String) -> EvalResult;

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
        if self.session.is_none() {
            match Session::new().await {
                Ok(s) => self.session = Some(s),
                Err(e) => {
                    return EvalResult {
                        stdout: String::new(),
                        stderr: format!("clank: failed to start shell: {e}\n"),
                        exit_code: 1,
                    };
                }
            }
        }

        let result = self.session.as_mut().unwrap().eval_line(&cmd).await;
        EvalResult {
            stdout: String::from_utf8_lossy(&result.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&result.stderr).into_owned(),
            exit_code: result.exit_code,
        }
    }

    async fn run_line(&mut self, cmd: String) -> String {
        let result = self.eval(cmd).await;
        format!("{}{}", result.stdout, result.stderr)
    }
}
