//! `GreeterAgent` — a deliberately trivial second Golem agent.
//!
//! It exists only to be the *target* of a grease-installed agent invocation from clank: proving that
//! `WasmRpcInvoker`'s `invoke_and_await` actually round-trips to another agent type in the same
//! cluster. Deployed alongside `clank:agent` via `golem.yaml`. The reflected type is `GreeterAgent`,
//! the constructor param is `name`, and the method is `greet(who) -> String` — all `String`, which is
//! exactly what clank's positional-string arg encoding supports.

use golem_rust::{agent_definition, agent_implementation};

/// A durable greeter. The constructor `name` is the agent identity (distinct names = distinct
/// instances), echoed back in the greeting so the round-trip carries both the constructor arg and
/// the method arg — an unambiguous proof the wRPC call reached this agent with both.
#[agent_definition]
pub trait GreeterAgent {
    /// Construct a greeter bound to `name` — the durable agent identity echoed back in every greeting.
    fn new(name: String) -> Self;

    /// Greet `who`, naming the greeter instance — a deterministic, self-identifying reply.
    async fn greet(&mut self, who: String) -> String;
}

/// The concrete `GreeterAgent`: a single `name` field carrying the identity set at construction.
pub struct GreeterAgentImpl {
    /// The greeter's identity, set once at construction and echoed in each greeting.
    name: String,
}

#[agent_implementation]
impl GreeterAgent for GreeterAgentImpl {
    fn new(name: String) -> Self {
        Self { name }
    }

    async fn greet(&mut self, who: String) -> String {
        format!("Hello, {who}! — from GreeterAgent({})", self.name)
    }
}
