//! GreeterAgent — a deliberately trivial second Golem agent.
//!
//! It exists to prove two clank seams end-to-end, from both directions:
//!
//! 1. **As the wRPC target**: a grease-installed agent invocation from clank
//!    (`WasmRpcInvoker::invoke_and_await`) round-trips to this agent in the same cluster. The
//!    reflected type is `GreeterAgent`, the constructor param is `name`, and the method is
//!    `greet(who) -> String` — all `String`, exactly what clank's positional-string arg encoding
//!    supports.
//! 2. **As the second implementer of the shell surface**: via `clank-embed`, this agent also
//!    exposes `eval`/`answer_prompt`/`abort_prompt`, so `golem agent shell 'GreeterAgent("…")'`
//!    opens an interactive shell exploring *greeter's own* sandbox (one agent instance = one
//!    worker = one isolated filesystem — clank's files are not visible here, nor vice versa).
//!    This is the proof that the surface is a contract, not a clank feature.
//!
//! The embed is the minimal *correct* Golem tier: no providers (`ask`/`mcp`/cluster degrade to
//! honest errors) except the replay-safe log sink, which every Golem embed needs — the default
//! sink's raw appends duplicate `/var/log` lines under oplog replay.

use clank_embed::{EmbeddedShell, EvalResult};
use golem_rust::{agent_definition, agent_implementation};

/// A durable greeter. The constructor `name` is the agent identity (distinct names = distinct
/// instances), echoed back in the greeting so the round-trip carries both the constructor arg and
/// the method arg — an unambiguous proof the wRPC call reached this agent with both.
#[agent_definition]
pub trait GreeterAgent {
    fn new(name: String) -> Self;

    /// Greet `who`, naming the greeter instance — a deterministic, self-identifying reply.
    async fn greet(&mut self, who: String) -> String;

    /// Run one command line in this agent's own sandbox (the shell surface `golem agent shell`
    /// drives; engine from `clank-embed`).
    async fn eval(&mut self, cmd: String) -> EvalResult;

    /// Deliver a human answer to a question surfaced in `eval`'s `pending_prompt`.
    async fn answer_prompt(&mut self, response: String) -> EvalResult;

    /// Cancel an outstanding question (the Ctrl-C convention — exit 130).
    async fn abort_prompt(&mut self) -> EvalResult;
}

pub struct GreeterAgentImpl {
    name: String,
    /// The embedded shell over this agent's own filesystem. Log-sink-only tier: replay-correct
    /// with zero providers — `ls`/`cat`/pipelines work; `ask`/`mcp` report honest errors.
    shell: EmbeddedShell,
}

#[agent_implementation]
impl GreeterAgent for GreeterAgentImpl {
    fn new(name: String) -> Self {
        Self {
            name,
            shell: EmbeddedShell::with_durable_log_sink(),
        }
    }

    async fn greet(&mut self, who: String) -> String {
        format!("Hello, {who}! — from GreeterAgent({})", self.name)
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
