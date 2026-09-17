//! The clank plug-in: the command families that sit on top of the shell core — `ask`/`context`/
//! `model` (`crate::ai`), the MCP client (`crate::mcp`), the `grease` package manager
//! (`crate::grease`) and the `golem` cluster commands (`crate::golem`) — and the state they keep for
//! a session.
//!
//! It reaches the shell through the [`crate::plugin::Plugin`] seam — [`Session`] holds it in an
//! anonymous slot, hands it out at the public entry points, and passes it down dispatch, so an
//! `ask` tool call can re-enter through [`SessionCtx::run_command`] by handing the plug-in back.
//!
//! [`Session`]: crate::session::Session

pub(crate) mod agent;
pub(crate) mod ask;
pub mod ext;
pub(crate) mod grease;
pub(crate) mod mcp;

#[cfg(test)]
mod tests;

use crate::builtins::promptuser::Resolution;
use crate::plugin::{Capabilities, LineAction, LinePhase, PluginPending, Route};
use crate::session::{LineResult, SessionCtx};

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
    pub(crate) repl: Option<ask::ReplState>,
    /// Installed MCP servers + open sessions. Reconstructed deterministically under Golem replay.
    pub(crate) mcp: crate::mcp::state::McpState,
    /// The injected MCP HTTP transport. `None` on native, in which case MCP degrades to a clean
    /// "not configured" error. Also used by `grease` for registry fetches.
    pub(crate) mcp_http: Option<Box<dyn crate::mcp::client::McpHttp>>,
    /// Installed grease packages. Reconstructed from the durable agent filesystem on boot.
    pub(crate) grease: crate::grease::state::GreaseState,
}

impl Clank {
    /// The family commands the shell must route to the plug-in rather than to Brush: `type` reports
    /// them as builtins, `--help` serves their manifest help, and each gets an honest-error stub
    /// for the nested contexts Brush does dispatch.
    const INTERCEPTED: &'static [&'static str] = &["ask", "mcp", "grease", "golem"];

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

/// A routed clank command, carried opaquely through the shell's dispatch.
///
/// The variants keep the order — and the doc comments — of the `LineRoute`/`CommandRoute` arms they
/// were lifted from, because that order is the interception ladder and reordering it changes what
/// the shell does. See `session/mod.rs`'s `LineRoute` doc.
pub(crate) enum ClankRoute {
    /// A top-level `context summarize` — needs the model, so it's routed through the authz gate to
    /// the async `run_context_summarize` instead of the sync `context` engine.
    ContextSummarize,
    /// A deferred-confirm re-run of a top-level `context summarize`.
    ContextSummarizeRerun,
    /// `ask repl` reaching the durable-agent path (the interactive REPL is native-only there).
    AskReplOnAgent,
    /// `… | ask "…"` — a stdin-as-context pipeline; carries the parsed upstream/tail split.
    AskPipe(crate::ai::ask::AskTailPipe),
    /// `ask ...`, parsed.
    Ask(crate::ai::ask::AskArgs),
    /// `mcp ...` management, parsed (or a parse error to report).
    Mcp(crate::mcp::error::Result<crate::mcp::cmd::McpCommand>),
    /// `grease ...` package management, parsed (or a parse error to report).
    Grease(crate::grease::error::Result<crate::grease::cmd::GreaseCommand>),
    /// `golem ...` cluster command, parsed (or a parse error to report).
    Golem(crate::golem::error::Result<crate::golem::cluster::GolemCommand>),
    /// `<server> <tool> …` for an installed MCP server.
    McpToolLine,
    /// A grease-installed prompt invocation.
    PromptLine,
    /// A grease-installed script invocation.
    ScriptLine,
    /// A grease-installed Golem agent invocation.
    AgentLine,
    /// A grease-installed MCP resource-template invocation.
    McpTemplateLine,
    /// A top-level `cat /mnt/mcp/<server>/<dynamic>` read target; carries `(server, uri)`.
    McpResourceRead(String, String),
}

#[async_trait::async_trait(?Send)]
impl crate::plugin::Plugin for Clank {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn builtins(
        &self,
    ) -> Vec<(
        String,
        brush_core::builtins::Registration<brush_core::extensions::DefaultShellExtensions>,
    )> {
        let mut builtins = crate::ai::model::builtins();
        // `ask`/`mcp`/`grease`/`golem` are Session-layer commands: a top-level line never reaches
        // Brush for them, but `$(...)`, a pipeline stage, `xargs` and `eval` dispatch straight to
        // Brush — where the stub gives the honest "top-level only" error instead of an external-exec
        // failure. See [`crate::builtins::interceptstub`].
        for name in Self::INTERCEPTED {
            builtins.push((
                (*name).to_string(),
                crate::builtins::interceptstub::session_stub(),
            ));
        }
        builtins
    }

    /// Every family's static manifests, merged into the session registry by `set_plugin`.
    ///
    /// `ask`, `mcp`, `grease` and `golem` have a manifest but no `SimpleCommand` of their own:
    /// they are intercepted at the Session layer (an `ask` LLM call must run where the Golem
    /// durable context is live, not inside a synchronous Brush builtin under the nested runtime),
    /// and what IS registered for them is the honest-error stub above. The `registry_guard` test
    /// below pins that pairing.
    fn manifests(&self) -> Vec<crate::manifest::Manifest> {
        let mut manifests = crate::ai::model::manifests();
        manifests.extend(crate::ai::ask::manifests());
        manifests.extend(crate::mcp::cmd::manifests());
        manifests.extend(crate::grease::cmd::manifests());
        manifests.extend(crate::golem::cluster::manifests());
        manifests
    }

    fn intercepted(&self) -> &'static [&'static str] {
        Self::INTERCEPTED
    }

    fn path_dirs(&self) -> Vec<std::path::PathBuf> {
        vec![
            crate::grease::config::script_bin_dir(), // default /usr/bin
            crate::mcp::config::bin_dir(),           // default /usr/lib/mcp/bin
            crate::grease::config::agent_bin_dir(),  // default /usr/lib/agents/bin
            crate::grease::config::bin_dir(),        // default /usr/lib/prompts/bin
            // A glob, not a directory: each installed skill's own `bin`.
            std::path::PathBuf::from(format!(
                "{}/*/bin",
                crate::grease::config::skills_dir().display()
            )),
        ]
    }

    /// The package layout `mcp add` / `grease install` write into.
    ///
    /// Native creates ONLY the dirs the operator explicitly pointed somewhere writable via a
    /// `CLANK_*` env override — never absolute system paths on the host (`/usr/lib/...` on macOS is
    /// not clank's to create). On the agent, whose per-instance VFS starts empty, all of them.
    fn layout_dirs(&self) -> Vec<std::path::PathBuf> {
        let dirs = [
            ("CLANK_MCP_ETC", crate::mcp::config::etc_dir()),
            ("CLANK_MCP_BIN", crate::mcp::config::bin_dir()),
            ("CLANK_GREASE_ETC", crate::grease::config::etc_dir()),
            ("CLANK_GREASE_STORE", crate::grease::config::store_dir()),
            ("CLANK_GREASE_BIN", crate::grease::config::bin_dir()),
            (
                "CLANK_GREASE_SCRIPT_BIN",
                crate::grease::config::script_bin_dir(),
            ),
            ("CLANK_GREASE_SKILLS", crate::grease::config::skills_dir()),
            (
                "CLANK_GREASE_AGENT_BIN",
                crate::grease::config::agent_bin_dir(),
            ),
        ];
        #[cfg(target_arch = "wasm32")]
        {
            dirs.into_iter().map(|(_var, dir)| dir).collect()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            dirs.into_iter()
                .filter(|(var, _dir)| std::env::var_os(var).is_some())
                .map(|(_var, dir)| dir)
                .collect()
        }
    }

    fn version(&self) -> u64 {
        // The old `(mcp.version(), grease.version())` cache key, folded into one number: a large odd
        // multiplier keeps the two counters from aliasing onto the same product.
        self.mcp
            .version()
            .wrapping_mul(1_000_003)
            .wrapping_add(self.grease.version())
    }

    fn capabilities(&self, registry: &crate::registry::CommandRegistry) -> Capabilities {
        let mut manifests = self.mcp.all_manifests();
        manifests.extend(self.grease.all_manifests());
        Capabilities {
            manifests,
            resources: self.grease.mcp_resource_index(),
            system_prompt: Some(crate::ai::ask::build_system_prompt_with_capabilities(
                registry,
                &self.mcp,
                &self.grease,
            )),
        }
    }

    fn authz_manifest(&self, name: &str) -> Option<crate::manifest::Manifest> {
        self.mcp
            .manifest_for(name)
            // An installed grease prompt: running it is an outbound LLM call ⇒ Confirm.
            .or_else(|| self.grease.manifest_for(name))
    }

    fn confirm_question(&self, gated_command: &str, sudo_grant: bool) -> Option<String> {
        self.grease_install_disclosure(gated_command, sudo_grant)
    }

    fn is_inspection(&self, line: &str) -> bool {
        ask::is_context_summarize(line)
    }

    fn classify_line(&self, line: &str, phase: LinePhase) -> Option<LineAction> {
        match phase {
            // `context summarize` needs the model, so it's detected here — before the generic
            // `context` dispatch — and routed (in [`Self::run`]'s match arm) through the authz gate
            // to the async Session layer instead of the sync `dispatch_context`/`apply_context`
            // engine. A nested `$(context summarize)`/pipe stays with Brush and hits the honest
            // error in `apply_context`.
            LinePhase::BeforeContext => ask::is_context_summarize(line)
                .then(|| LineAction::Intercept(Route(Box::new(ClankRoute::ContextSummarize)))),
            LinePhase::BeforeGate => {
                // `<server> --help` / `<server> <tool> --help` for an installed MCP server (before
                // the authz gate — help never confirms).
                if let Some(help) = self.mcp_help_for(line) {
                    return Some(LineAction::Help(help));
                }
                // `<name> --help` for an installed grease command package (prompt or script), same rule.
                if let Some(help) = self.pkg_help_for(line) {
                    return Some(LineAction::Help(help));
                }
                // `ask repl` reaching `eval_line` is the durable-agent path (the native driver
                // intercepts it before `eval_line` and runs the interactive loop).
                if crate::ai::ask::classify_repl(line).is_some() {
                    return Some(LineAction::Intercept(Route(Box::new(
                        ClankRoute::AskReplOnAgent,
                    ))));
                }
                // stdin-as-context: `cat x | ask "…"`. The LLM call can't run inside Brush's pipeline
                // (the reactor isn't live there — the "Wall C" wall), so the Session pre-extracts it:
                // run the upstream, capture its stdout, and dispatch the `ask` tail at the session
                // layer with those bytes as stdin. `ask` must be the FINAL stage; anywhere else it
                // stays the honest stub error.
                if let Some(pipe) = crate::ai::ask::split_ask_tail(line) {
                    return Some(LineAction::Intercept(Route(Box::new(ClankRoute::AskPipe(
                        pipe,
                    )))));
                }
                None
            }
        }
    }

    fn classify_command(&self, line: &str) -> Option<Route> {
        let route = if ask::is_context_summarize(line) {
            ClankRoute::ContextSummarizeRerun
        } else if let Some(args) = crate::ai::ask::classify(line) {
            ClankRoute::Ask(args)
        } else if let Some(parsed) = crate::mcp::cmd::classify(line) {
            ClankRoute::Mcp(parsed)
        } else if let Some(parsed) = crate::grease::cmd::classify(line) {
            ClankRoute::Grease(parsed)
        } else if let Some(parsed) = crate::golem::cluster::classify(line) {
            ClankRoute::Golem(parsed)
        } else if self.is_mcp_tool_line(line) {
            ClankRoute::McpToolLine
        } else if self.is_prompt_line(line) {
            ClankRoute::PromptLine
        } else if self.is_script_line(line) {
            ClankRoute::ScriptLine
        } else if self.is_agent_line(line) {
            ClankRoute::AgentLine
        } else if self.is_mcp_template_line(line) {
            ClankRoute::McpTemplateLine
        } else if let Some((server, uri)) = self.dynamic_mcp_read_target(line) {
            ClankRoute::McpResourceRead(server, uri)
        } else {
            return None;
        };
        Some(Route(Box::new(route)))
    }

    async fn run(
        &mut self,
        route: Route,
        line: &str,
        pid: Option<u32>,
        blanket_authorized: bool,
        ctx: &mut SessionCtx<'_>,
    ) -> LineResult {
        let Ok(route) = route.0.downcast::<ClankRoute>() else {
            return LineResult::stderr("clank: internal error: foreign route\n");
        };
        match *route {
            ClankRoute::ContextSummarize => self.dispatch_context_summarize(ctx, line, pid).await,
            // Reached here only on a deferred-confirm re-run (top-level `context summarize` is
            // intercepted in `eval_line`). Route to the async summarizer; the caller
            // (`resolve_auth_confirm`) skips recording its inspection output.
            ClankRoute::ContextSummarizeRerun => self.run_context_summarize(ctx).await,
            ClankRoute::AskReplOnAgent => {
                // The native driver intercepts `ask repl` before `eval_line` and runs the
                // interactive loop; the durable agent can't own a blocking read-loop (Golem
                // serializes invocations). Return an honest pointer to the working forms.
                let msg = b"ask repl: interactive REPL is a native-terminal feature; on the durable \
                            agent, drive a conversation with repeated `ask` calls (each is one turn)\n";
                ctx.finish(pid, LineResult::from_outcome(Vec::new(), msg.to_vec(), 2))
            }
            ClankRoute::AskPipe(pipe) => self.run_ask_pipe(ctx, pipe, pid).await,
            // `ask` dispatches to the injected LLM provider — same "await at the Session layer, never
            // through `execute`'s nested runtime" rule as curl/wget. The provider's async `complete`
            // is awaited here, one level under the Golem SDK's executor, where the durable context
            // is live and WASI-HTTP futures actually complete. See `askcmd`.
            ClankRoute::Ask(args) => self.run_ask(ctx, args, blanket_authorized).await,
            // `mcp` management runs at the Session layer — its add/reload/session subcommands do
            // HTTP, which must await under the live reactor (same rule as curl/ask).
            ClankRoute::Mcp(Ok(cmd)) => self.run_mcp(ctx, cmd).await,
            ClankRoute::Mcp(Err(e)) => {
                LineResult::from_outcome(Vec::new(), format!("{e}\n").into_bytes(), 2)
            }
            // `grease` package management runs at the Session layer — install/search/update do HTTP.
            ClankRoute::Grease(Ok(cmd)) => self.run_grease(ctx, cmd).await,
            ClankRoute::Grease(Err(e)) => {
                LineResult::from_outcome(Vec::new(), format!("{e}\n").into_bytes(), 2)
            }
            // `golem` cluster command — runtime API calls await under the reactor (like mcp/ask).
            ClankRoute::Golem(Ok(cmd)) => self.run_golem(cmd).await,
            ClankRoute::Golem(Err(e)) => {
                LineResult::from_outcome(Vec::new(), format!("{e}\n").into_bytes(), 2)
            }
            // `<server> <tool> …` for an installed MCP server: an outbound HTTP tool call (its authz
            // Confirm was already resolved via the dynamic manifest at the gate).
            ClankRoute::McpToolLine => self.run_mcp_tool(line).await,
            // A grease-installed prompt: fill its body from args and run it through the model (its
            // Confirm was resolved via the dynamic manifest at the gate; sudo pre-authorizes).
            ClankRoute::PromptLine => self.run_prompt(ctx, line, blanket_authorized).await,
            // A grease-installed script: fill its body from args and run the shell source locally
            // (its Confirm was resolved via the dynamic manifest at the gate; sudo pre-authorizes).
            ClankRoute::ScriptLine => self.run_script(ctx, line).await,
            // A grease-installed Golem agent: parse the ctor/method/args and invoke it via wRPC in the
            // cluster (Confirm resolved at the gate; sudo pre-authorizes). Await mode only in v1.
            ClankRoute::AgentLine => self.run_agent(ctx, line).await,
            // A grease-installed MCP resource-template executable: substitute the args into the URI
            // template and read the constructed resource live (top-level only, Wall-C).
            ClankRoute::McpTemplateLine => self.run_mcp_template(line).await,
            // A top-level `cat /mnt/mcp/<server>/<dynamic>`: fetch the resource live via
            // `resources/read` (the read can't run in Brush's synchronous `cat` — the Wall-C wall — so
            // it's served here at the Session layer for top-level lines only; inside $()/pipes it falls
            // through to Brush and hits the honest "no such file").
            ClankRoute::McpResourceRead(server, uri) => {
                self.run_mcp_resource_read(&server, &uri).await
            }
        }
    }

    async fn resume(
        &mut self,
        pending: PluginPending,
        resolution: Resolution,
        pid: Option<u32>,
        ctx: &mut SessionCtx<'_>,
    ) -> LineResult {
        let Ok(paused) = pending.0.downcast::<ask::AgentLoopPause>() else {
            return LineResult::stderr("clank: internal error: foreign pending\n");
        };
        let ask::AgentLoopPause { state, pause } = *paused;
        self.resolve_agent_loop(ctx, resolution, state, pause, pid)
            .await
    }

    fn cancel(&mut self, pid: u32, ctx: &mut SessionCtx<'_>) -> Option<String> {
        self.cancel_invocation(ctx, pid)
    }

    async fn after_record(&mut self, ctx: &mut SessionCtx<'_>) {
        // If recording just evicted old entries to stay under the cap, upgrade the leading count
        // marker into a model-generated summary block (no-op when nothing was dropped or no
        // provider exists).
        self.compact_dropped_span(ctx).await;
    }
}

#[cfg(test)]
mod registry_guard {
    use crate::plugin::Plugin as _;

    /// The plug-in half of the registry drift guard (`registry::tests` owns the core half): every
    /// builtin the plug-in registers has exactly one manifest, and every manifest it contributes
    /// either has a builtin or is one of the Session-intercepted commands whose "builtin" is the
    /// honest-error stub.
    #[test]
    fn plugin_builtins_and_manifests_match() {
        let clank = super::Clank::default();
        let builtins: std::collections::BTreeSet<String> =
            clank.builtins().into_iter().map(|(n, _)| n).collect();
        let manifests: std::collections::BTreeSet<String> =
            clank.manifests().into_iter().map(|m| m.name).collect();
        let manual: std::collections::BTreeSet<String> = super::Clank::INTERCEPTED
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert!(
            builtins.is_subset(&manifests),
            "a plug-in builtin has no manifest: {:?}",
            builtins.difference(&manifests)
        );
        assert_eq!(
            &manifests - &builtins,
            &manual - &builtins,
            "a plug-in manifest has no builtin or manual entry"
        );
    }
}
