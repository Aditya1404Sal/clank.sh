//! The clank plug-in: the command families that sit on top of the shell core — `ask`/`context`/
//! `model` (`crate::ai`), the MCP client (`crate::mcp`), the `grease` package manager
//! (`crate::grease`) and the `golem` cluster commands (`crate::golem`) — and the state they keep for
//! a session.
//!
//! During the crate split this is a concrete field of [`crate::session::Session`]; it moves behind
//! the `Plugin` slot once every family's glue is `impl Clank`.

pub mod agent;
pub mod mcp;

/// The per-session state of every clank command family.
#[derive(Default)]
pub struct Clank {
    /// The injected LLM provider for `ask`. Installed by the agent build (a durable Anthropic
    /// provider); `None` on native and until injected, in which case `ask` degrades to a clean
    /// "not configured" error. See [`crate::ai::ask`].
    pub(crate) ask_provider: Option<Box<dyn crate::ai::ask::AskProvider>>,
    /// The injected Golem-agent invoker (durable `WasmRpc` on the agent; a fake in tests). `None` on
    /// native / until injected, in which case an agent invocation degrades to a clean "needs a
    /// cluster" error. See [`crate::golem::agent`].
    pub(crate) agent_invoker: Option<Box<dyn crate::golem::agent::AgentInvoker>>,
    /// The injected Golem cluster interface backing the `golem` command + agent oplog/status
    /// (durable `golem:api` bindings on the agent). `None` on native / until injected → the honest
    /// no-cluster error. See [`crate::golem::cluster`].
    pub(crate) golem_cluster: Option<Box<dyn crate::golem::cluster::GolemCluster>>,
    /// Triggered/scheduled agent invocations awaiting a possible `kill`-cancel: PID → cancel token.
    pub(crate) pending_invocations: Vec<agent::PendingInvocation>,
    /// Out-of-band stdin for the next `ask` dispatch: the captured stdout of an upstream pipeline
    /// stage (`cat x | ask "…"`). Set by the pipe pre-extraction (or restored on a deferred-confirm
    /// resume) and `take()`n by `run_ask`. `None` for an ordinary `ask` line.
    pub(crate) next_ask_stdin: Option<String>,
    /// An active `ask repl` session's isolated transcript + model. `Some` only while the native
    /// driver is inside a REPL. Never set on the durable agent.
    pub(crate) repl: Option<crate::session::ReplState>,
    /// Installed MCP servers + open sessions. Reconstructed deterministically under Golem replay.
    pub(crate) mcp: crate::mcp::state::McpState,
    /// The injected MCP HTTP transport. `None` on native, in which case MCP degrades to a clean
    /// "not configured" error. Also used by `grease` for registry fetches.
    pub(crate) mcp_http: Option<Box<dyn crate::mcp::client::McpHttp>>,
    /// Installed grease packages. Reconstructed from the durable agent filesystem on boot.
    pub(crate) grease: crate::grease::state::GreaseState,
    /// Cached capability views (dynamic manifests / MCP resource index / system prompt), rebuilt
    /// only when `(mcp.version(), grease.version())` changes.
    pub(crate) cap_cache: Option<crate::session::CapabilityCache>,
}

impl Clank {
    /// The plug-in as a new session starts it: grease packages loaded from the filesystem, MCP empty
    /// until the session reconstructs it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            grease: crate::grease::state::GreaseState::load(),
            ..Self::default()
        }
    }
}
