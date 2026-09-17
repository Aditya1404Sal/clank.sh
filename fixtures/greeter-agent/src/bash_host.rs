//! Minimal durable caller of the stateless bash tool, for release acceptance.
use clank_embed::{EmbeddedShell, EvalResult};
use golem_rust::{agent_definition, agent_implementation};

#[agent_definition]
pub trait BashHost {
    fn new(name: String) -> Self;
    async fn eval(&mut self, cmd: String) -> EvalResult;
    async fn answer_prompt(&mut self, response: String) -> EvalResult;
    async fn abort_prompt(&mut self) -> EvalResult;
    async fn owner_eval(&mut self, cmd: String) -> EvalResult;
    fn reset(&mut self);
}
struct BashHostImpl {
    state: String,
    owner_shell: EmbeddedShell,
}

impl BashHostImpl {
    async fn call(&mut self, command: &str, extra: serde_json::Value) -> EvalResult {
        let result = async {
            let runtime = bash_golem::discover()?;
            let mut input = extra;
            input["state"] = self.state.clone().into();
            let output = runtime
                .invoker
                .invoke(bash::agent_tools::ToolRequest {
                    name: "bash".into(),
                    path: vec![command.into()],
                    input,
                    stdin: None,
                })
                .await
                .map_err(|e| e.message)?;
            if output.exit_code != 0 {
                return Err(String::from_utf8_lossy(&output.stderr).into_owned());
            }
            let json: serde_json::Value =
                serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
            let state = json["state"]
                .as_str()
                .ok_or("bash result has no state")?
                .to_owned();
            let result: EvalResult = serde_json::from_value(json).map_err(|e| e.to_string())?;
            self.state = state;
            Ok::<_, String>(result)
        }
        .await;
        result.unwrap_or_else(|e| EvalResult {
            stdout: String::new(),
            stderr: format!("BashHost: {e}\n"),
            exit_code: 1,
            pending_prompt: None,
            cwd: String::new(),
        })
    }
}
#[agent_implementation]
impl BashHost for BashHostImpl {
    fn new(_name: String) -> Self {
        Self {
            state: String::new(),
            owner_shell: EmbeddedShell::new(),
        }
    }
    async fn eval(&mut self, cmd: String) -> EvalResult {
        self.call("run", serde_json::json!({"script":cmd})).await
    }
    async fn answer_prompt(&mut self, response: String) -> EvalResult {
        self.call("answer-prompt", serde_json::json!({"response":response}))
            .await
    }
    async fn abort_prompt(&mut self) -> EvalResult {
        self.call("abort-prompt", serde_json::json!({})).await
    }
    async fn owner_eval(&mut self, cmd: String) -> EvalResult {
        self.owner_shell.eval(&cmd).await
    }
    fn reset(&mut self) {
        self.state.clear();
    }
}
