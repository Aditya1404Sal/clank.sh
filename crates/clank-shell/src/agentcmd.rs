//! The Golem-agent invocation seam: `clank-shell` defines the [`AgentInvoker`] trait; the durable
//! Golem binding (`golem-rust`'s generic `WasmRpc`) is implemented in the `clank-agent` crate and
//! injected via `Session::set_agent_invoker`. Mirrors the `AskProvider`→`DurableAnthropicProvider` and
//! `McpHttp`→`WstdMcpHttp` seams.
//!
//! A grease-installed Golem agent (README:671, 795) becomes a `/usr/lib/agents/bin/<name>` command;
//! running `<agent> --<ctor> val <method> [-- --<arg> val]` builds an [`AgentInvocation`] and dispatches
//! it through the injected invoker, which invokes the agent type by name in the configured Golem
//! cluster (await mode) and returns the rendered result. Native / no-cluster returns an honest error.
//!
//! Scope note: this increment supports the **await** invocation mode with string-typed constructor and
//! method arguments. `--trigger`/`--schedule`, `--revision`/`--phantom`, the reserved subcommands
//! (`oplog`/`stream`/`status`/`repl`), and PID/kill-cancel are deferred (README's full agent surface).

/// One await-mode invocation of a Golem agent, parsed from an agent-executable command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentInvocation {
    /// The Golem agent **type** name to invoke (the reflected type, e.g. `ShoppingCart`), NOT the
    /// installed command name.
    pub agent_type: String,
    /// The constructor parameters (`--<name> value`) that identify the agent instance. Order-preserving
    /// (Golem's agent identity is the ordered constructor tuple).
    pub constructor: Vec<(String, String)>,
    /// The method (kebab-case subcommand) to invoke.
    pub method: String,
    /// The method arguments (`--<name> value`), order-preserving.
    pub args: Vec<(String, String)>,
}

/// Injected Golem-agent invoker. `?Send` (wasip2 is single-threaded), `dyn`-compatible as
/// `Box<dyn AgentInvoker>`.
#[async_trait::async_trait(?Send)]
pub trait AgentInvoker {
    /// Invoke `inv` in the configured Golem cluster (await mode) and return the rendered result string,
    /// or an error message. The implementation builds the `WasmRpc` value tree from the string args.
    async fn invoke(&self, inv: &AgentInvocation) -> Result<String, String>;
}
