//! The plug-in seam: how a command family attaches to a shell [`Session`](crate::session::Session)
//! without the shell naming it. See `dev-docs/designs/proposed/bash-crate-split.md` §2.

use std::any::Any;

use crate::builtins::promptuser::Resolution;
use crate::manifest::Manifest;
use crate::registry::CommandRegistry;
use crate::session::{LineResult, SessionCtx};

/// A plug-in's own routing decision, opaque to the shell; the plug-in downcasts it back in `run`.
pub struct Route(pub Box<dyn Any>);

/// A pause a plug-in surfaced, carried by the shell until `answer_prompt` hands it back to `resume`.
pub struct PluginPending(pub Box<dyn Any>);

/// What a plug-in contributes to the per-line capability view.
#[derive(Default)]
pub struct Capabilities {
    /// Manifests for commands installed at run time (read by `man`, `type` and authorization).
    pub manifests: Vec<Manifest>,
    /// The `/mnt/mcp` resource index `ls`, `cat` and `stat` read.
    pub resources: Vec<crate::runtime::mcpfs::ResourceEntry>,
    /// The rendered system prompt served at `/proc/clank/system-prompt`.
    pub system_prompt: Option<String>,
}

/// Where in the pre-gate ladder `classify_line` is being asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinePhase {
    /// After core `--help`, before `context show|clear|trim`.
    BeforeContext,
    /// After `type`, as the last check before the authorization gate.
    BeforeGate,
}

/// What `classify_line` found.
pub enum LineAction {
    /// `--help` text for a plug-in command; printed without authorization.
    Help(String),
    /// A line the plug-in authorizes and runs itself.
    Intercept(Route),
}

/// A command family plugged into a shell session.
///
/// Every hook has a default except `run` and `resume`, so a plug-in implements only what it uses.
#[async_trait::async_trait(?Send)]
pub trait Plugin: Any {
    /// `self` as `Any`, for [`Session::plugin_ref`](crate::session::Session::plugin_ref).
    fn as_any(&self) -> &dyn Any;
    /// `self` as mutable `Any`, for [`Session::plugin_mut`](crate::session::Session::plugin_mut).
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Brush builtins the plug-in owns.
    fn builtins(
        &self,
    ) -> Vec<(
        String,
        brush_core::builtins::Registration<brush_core::extensions::DefaultShellExtensions>,
    )> {
        Vec::new()
    }
    /// Static manifests merged into the session registry.
    fn manifests(&self) -> Vec<Manifest> {
        Vec::new()
    }
    /// Command names `type` reports as intercepted.
    fn intercepted(&self) -> &'static [&'static str] {
        &[]
    }
    /// Directories appended to `$PATH`, in order.
    fn path_dirs(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
    /// Called once when the plug-in is installed on a session.
    fn on_start(&mut self) {}

    /// Changes whenever [`capabilities`](Self::capabilities) would render differently.
    fn version(&self) -> u64 {
        0
    }
    /// The per-line capability view.
    fn capabilities(&self, _registry: &CommandRegistry) -> Capabilities {
        Capabilities::default()
    }
    /// A run-time manifest for `name`, consulted by authorization after the static registry.
    fn authz_manifest(&self, _name: &str) -> Option<Manifest> {
        None
    }
    /// A custom confirmation question for `gated_command`, or `None` for the generic one.
    fn confirm_question(&self, _gated_command: &str, _sudo_grant: bool) -> Option<String> {
        None
    }
    /// Whether `line`'s output is inspection-only and must not be recorded.
    fn is_inspection(&self, _line: &str) -> bool {
        false
    }

    /// Pre-gate routing, asked at two points of the ladder (see [`LinePhase`]).
    fn classify_line(&self, _line: &str, _phase: LinePhase) -> Option<LineAction> {
        None
    }
    /// Post-gate routing, asked where the family checks sat in `classify_command`.
    fn classify_command(&self, _line: &str) -> Option<Route> {
        None
    }
    /// Run a routed line. `ctx.run_command(self, …)` re-enters dispatch.
    async fn run(
        &mut self,
        route: Route,
        line: &str,
        pid: Option<u32>,
        blanket_authorized: bool,
        ctx: &mut SessionCtx<'_>,
    ) -> LineResult;
    /// Resolve a pause this plug-in surfaced.
    async fn resume(
        &mut self,
        pending: PluginPending,
        resolution: Resolution,
        pid: Option<u32>,
        ctx: &mut SessionCtx<'_>,
    ) -> LineResult;
    /// `kill <pid>` for a pid the plug-in tracks: `Some(message)` when it handled it.
    fn cancel(&mut self, _pid: u32, _ctx: &mut SessionCtx<'_>) -> Option<String> {
        None
    }
    /// After a line's output was recorded (the transcript may have evicted entries).
    async fn after_record(&mut self, _ctx: &mut SessionCtx<'_>) {}
}
