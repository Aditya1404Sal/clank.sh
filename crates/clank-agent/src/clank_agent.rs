use clank_shell::session::Session;
use golem_rust::{agent_definition, agent_implementation};

/// A durable shell instance. The constructor parameter `name` is the agent identity, so distinct
/// names are isolated instances (each with its own shell state, transcript, and filesystem).
#[agent_definition]
pub trait ClankAgent {
    fn new(name: String) -> Self;

    /// Run one shell command line and return its output. Mutates the durable session (shell state
    /// and transcript persist across invocations).
    async fn run_line(&mut self, cmd: String) -> String;
}

pub struct ClankAgentImpl {
    _name: String,
    /// The live shell session — durable across invocations. Built lazily on first `run_line`
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

    async fn run_line(&mut self, cmd: String) -> String {
        if self.session.is_none() {
            match Session::new().await {
                Ok(s) => self.session = Some(s),
                Err(e) => return format!("clank: failed to start shell: {e}\n"),
            }
        }
        // `Flow` (continue/exit) is meaningless for an agent — there is no REPL loop to break.
        let (output, _flow) = self.session.as_mut().unwrap().run_line(&cmd).await;
        String::from_utf8_lossy(&output).into_owned()
    }
}
