//! `ClankSessionExt`: the clank-specific setters and `ask repl` entry points on a `bash::Session`.
//!
//! The shell core cannot name [`Clank`] — it lives in a crate that does not depend on this one — so
//! the provider injection and the REPL driver calls that used to be inherent `Session` methods are
//! an extension trait here, on the side that does. `bash::Session::new` installs no plug-in; an
//! embedder calls [`ClankSessionExt::install_clank`] to get clank's command families.

use bash::session::Session;

use super::Clank;

/// Clank's provider setters and `ask repl` driver calls, on any session with `Clank` installed.
/// Each is a no-op (or an honest error) when no `Clank` is installed.
pub trait ClankSessionExt {
    /// Install `Clank` (grease loaded, MCP reconstructed) on this session.
    fn install_clank(&mut self);
    /// Install the LLM provider that backs `ask`. The agent build injects a durable Anthropic
    /// provider here after constructing the session; without one, `ask` reports "not configured".
    fn set_ask_provider(&mut self, provider: Box<dyn crate::ai::ask::AskProvider>);
    /// Install the Golem-agent invoker (a durable `WasmRpc` binding on the agent). Without one, an
    /// installed agent command reports "needs a cluster" (README:895). Injected after construction.
    fn set_agent_invoker(&mut self, invoker: Box<dyn crate::golem::agent::AgentInvoker>);
    /// Install the Golem cluster interface backing the `golem` command + agent oplog/status (durable
    /// `golem:api` bindings on the agent). Without one, `golem` reports "needs a cluster".
    fn set_golem_cluster(&mut self, cluster: Box<dyn crate::golem::cluster::GolemCluster>);
    /// Install the MCP HTTP transport (a durable WASI-HTTP client on the agent). Without one, MCP
    /// commands report "not configured" (exit 4). Injected after construction like the ask provider.
    fn set_mcp_http(&mut self, http: Box<dyn crate::mcp::client::McpHttp>);

    /// Start an `ask repl` session (see [`Clank::repl_start`]).
    ///
    /// # Errors
    /// Returns `Err` when no plug-in owns `ask`, no provider is configured, or the resolved model
    /// carries an unknown `provider/` prefix.
    fn repl_start(&mut self, args: &crate::ai::ask::ReplArgs) -> crate::ai::error::Result<String>;
    /// The active REPL's model id, for the `[model]>` prompt. `None` if no REPL is active.
    fn repl_model(&self) -> Option<String>;
    /// Handle a REPL meta-command (see [`Clank::repl_meta`]).
    fn repl_meta(&mut self, line: &str) -> Option<(String, bool)>;
    /// Run one REPL turn (see [`Clank::repl_turn`]).
    fn repl_turn(&mut self, prompt: &str) -> impl std::future::Future<Output = String>;
    /// End the REPL session and return its rendered transcript (see [`Clank::repl_end`]).
    fn repl_end(&mut self) -> Vec<u8>;
}

impl ClankSessionExt for Session {
    fn install_clank(&mut self) {
        let mut clank = Clank::new();
        clank.reconstruct_mcp();
        self.set_plugin(Box::new(clank));
    }

    fn set_ask_provider(&mut self, provider: Box<dyn crate::ai::ask::AskProvider>) {
        if let Some(clank) = self.plugin_mut::<Clank>() {
            // Wrap so each LLM turn is logged to http.log (the outbound Anthropic call).
            clank.ask_provider = Some(Box::new(crate::ai::ask::LoggingAskProvider::new(provider)));
        }
    }

    fn set_agent_invoker(&mut self, invoker: Box<dyn crate::golem::agent::AgentInvoker>) {
        if let Some(clank) = self.plugin_mut::<Clank>() {
            clank.agent_invoker = Some(invoker);
        }
    }

    fn set_golem_cluster(&mut self, cluster: Box<dyn crate::golem::cluster::GolemCluster>) {
        if let Some(clank) = self.plugin_mut::<Clank>() {
            clank.golem_cluster = Some(cluster);
        }
    }

    fn set_mcp_http(&mut self, http: Box<dyn crate::mcp::client::McpHttp>) {
        if let Some(clank) = self.plugin_mut::<Clank>() {
            // Wrap the transport so every MCP + grease-registry request is logged to http.log (redacted).
            clank.mcp_http = Some(Box::new(crate::mcp::client::LoggingMcpHttp::new(http)));
        }
    }

    fn repl_start(&mut self, args: &crate::ai::ask::ReplArgs) -> crate::ai::error::Result<String> {
        self.with_plugin_sync::<Clank, _>(|clank, ctx| clank.repl_start(ctx, args))
            .unwrap_or_else(|| Err(crate::ai::Error::not_configured("ask repl")))
    }

    fn repl_model(&self) -> Option<String> {
        self.plugin_ref::<Clank>()?.repl_model()
    }

    fn repl_meta(&mut self, line: &str) -> Option<(String, bool)> {
        self.plugin_mut::<Clank>()?.repl_meta(line)
    }

    async fn repl_turn(&mut self, prompt: &str) -> String {
        self.with_plugin::<Clank, _>(async |clank, ctx| clank.repl_turn(ctx, prompt).await)
            .await
            .unwrap_or_else(|| "ask repl: no active session\n".to_string())
    }

    fn repl_end(&mut self) -> Vec<u8> {
        self.plugin_mut::<Clank>()
            .map(Clank::repl_end)
            .unwrap_or_default()
    }
}
