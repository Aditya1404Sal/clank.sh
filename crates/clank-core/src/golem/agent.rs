//! The Golem-agent invocation seam: `clank-core` defines the [`AgentInvoker`] trait; the durable
//! Golem binding (`golem-rust`'s generic `WasmRpc`) is implemented in the `clank-agent` crate and
//! injected via `Session::set_agent_invoker`. Mirrors the `AskProvider`→`DurableAnthropicProvider` and
//! `McpHttp`→`WstdMcpHttp` seams.
//!
//! A grease-installed Golem agent (README:671, 795) becomes a `/usr/lib/agents/bin/<name>` command;
//! running `<agent> [--<ctor> val] [<wrapper-flags>] <method> [-- --<arg> val]` builds an
//! [`AgentInvocation`] and dispatches it through the injected invoker, which invokes the agent type by
//! name in the configured Golem cluster and returns the rendered result. Native / no-cluster returns
//! an honest error.
//!
//! Modes (README:842): **await** (default, returns the result), **trigger** (`--trigger`,
//! fire-and-forget), **schedule** (`--schedule <iso8601>`, deferred). Trigger/schedule return a handle
//! the caller retains for `kill`-cancel. `--phantom <uuid>` addresses a phantom instance;
//! `--revision <n>` is honest-stubbed (no wasm-rpc constructor slot — a `golem:api` concern).

/// The invocation mode selected by the wrapper flags (README:842).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvokeMode {
    /// Await the result (default).
    Await,
    /// Fire-and-forget (`--trigger`).
    Trigger,
    /// Schedule for a future ISO-8601 time (`--schedule <iso8601>`).
    Schedule(String),
}

/// One invocation of a Golem agent, parsed from an agent-executable command line.
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
    /// The invocation mode (await / trigger / schedule).
    pub mode: InvokeMode,
    /// The `--phantom <uuid>` instance selector, if given.
    pub phantom: Option<String>,
}

impl AgentInvocation {
    /// A bare await-mode invocation (no wrapper flags) — the common case + test helper.
    #[must_use]
    pub fn new(
        agent_type: String,
        constructor: Vec<(String, String)>,
        method: String,
        args: Vec<(String, String)>,
    ) -> Self {
        Self { agent_type, constructor, method, args, mode: InvokeMode::Await, phantom: None }
    }
}

/// The outcome of a non-await invocation: an opaque cancellation handle the caller retains so a later
/// `kill <pid>` can cancel it (README:850). Empty when there's nothing to cancel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InvokeHandle {
    /// An opaque token the invoker understands (e.g. an idempotency key / scheduled-invocation id).
    /// `None` when the mode isn't cancelable or the invoker doesn't support cancel.
    pub cancel_token: Option<String>,
    /// A short human description for the `ps`/output line (e.g. "triggered", "scheduled for …").
    pub note: String,
}

/// Injected Golem-agent invoker. `?Send` (wasip2 is single-threaded), `dyn`-compatible as
/// `Box<dyn AgentInvoker>`.
#[async_trait::async_trait(?Send)]
pub trait AgentInvoker {
    /// Invoke `inv` in await mode and return the rendered result string, or an error message.
    async fn invoke(&self, inv: &AgentInvocation) -> Result<String, String>;

    /// Invoke `inv` in trigger or schedule mode (fire-and-forget / deferred). Returns an
    /// [`InvokeHandle`] carrying a cancel token (for `kill`) + a note. Default: not supported.
    async fn invoke_async(&self, _inv: &AgentInvocation) -> Result<InvokeHandle, String> {
        Err("this invoker does not support trigger/schedule mode".to_string())
    }

    /// Cancel a previously-triggered/scheduled invocation by its token (README:850). Returns `Ok(true)`
    /// if the cancel took effect, `Ok(false)` if it was already in-progress/completed (a no-op), or an
    /// error. Default: not supported.
    async fn cancel(&self, _token: &str) -> Result<bool, String> {
        Err("this invoker does not support cancel".to_string())
    }
}
