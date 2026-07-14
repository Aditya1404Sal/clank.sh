//! The durable Golem cluster interface backing the `golem` command on the Golem agent.
//!
//! `clank-shell` defines the [`GolemCluster`](clank_shell::golem::cluster::GolemCluster) seam; this module
//! (wasm-only agent crate) implements it with golem-rust's `golem:api` host bindings. Mirrors the
//! `AgentInvoker`→`WasmRpcInvoker` seam.
//!
//! NB: the `golem_rust::bindings::golem::api::host::*` path is deliberate — golem-rust **2.1.0** (the
//! resolved dependency) has no crate-root `get_self_metadata`/`fork` wrapper; `pub mod bindings` is the
//! public API there. (A later SDK adds crate-root wrappers, and the vendored `golem-stuff` checkout shows
//! them — but migrating to them breaks the 2.1.0 build. Only `ForkResult` is re-exported at the crate root.)
//!
//! Scope: the operations with a clean host primitive are wired — `fork` (shell self-fork),
//! `get-self-metadata` (the shell instance's own status/oplog anchor), `get-agents` (list). Operations
//! that need an `agent-id` constructed from a type + constructor tuple (`get-agent-metadata`,
//! `get-oplog`, `revert-agent`) require the caller's reflected constructor schema to build the id; v1
//! reports an honest "needs constructor schema" for those rather than guessing the encoding.
//! `interrupt`/`resume` are honest-stubbed in `clank-shell` (no host func at all).

use clank_shell::golem::cluster::GolemCluster;

/// A [`GolemCluster`] backed by the durable `golem:api` host bindings.
pub struct GolemApiCluster;

/// Render this instance's own metadata (used for `golem oplog`/`status` anchoring + a liveness check).
fn self_metadata_text() -> String {
    // SPIKE (dev SDK): `get_self_metadata` moved from the now-private `bindings::golem::api::host`
    // to the crate root; `AgentMetadata.agent_id` is a struct (no Display) whose `.agent_id` is the
    // instance name string.
    let md = golem_rust::get_self_metadata();
    format!(
        "agent-id: {}\nstatus: {:?}\ncomponent-revision: {}\n",
        md.agent_id.agent_id, md.status, md.component_revision
    )
}

#[async_trait::async_trait(?Send)]
impl GolemCluster for GolemApiCluster {
    async fn agent_list(&self) -> Result<String, String> {
        // `get-agents` enumerates agents for the current component. It's a paged resource; we render
        // the first page's agent ids. The constructor needs a component-id filter/precise flag — v1
        // lists the current component's agents via self metadata's component context.
        let md = golem_rust::get_self_metadata();
        // Without a component-id + filter we can't page arbitrary agents here; report the self view as
        // the anchor + note that full enumeration needs a component filter (honest partial).
        Ok(format!(
            "this instance:\n{}\n(full `golem agent list` enumeration needs a component filter — \
             not wired in v1)\n",
            format_args!("agent-id: {}", md.agent_id.agent_id)
        ))
    }

    async fn agent_oplog(&self, agent_type: &str, _ctor: &[(String, String)]) -> Result<String, String> {
        Err(format!(
            "agent oplog for '{agent_type}' needs the agent's constructor schema to build its \
             agent-id (not wired in v1); use `golem oplog` for this instance's own oplog"
        ))
    }

    async fn agent_status(&self, agent_type: &str, _ctor: &[(String, String)]) -> Result<String, String> {
        Err(format!(
            "agent status for '{agent_type}' needs the agent's constructor schema to build its \
             agent-id (not wired in v1); use `golem agent status` on this instance via `golem oplog`"
        ))
    }

    async fn connect(&self, identity: &str) -> Result<String, String> {
        Err(format!(
            "golem connect '{identity}' needs an agent-id + streaming inspection (not wired in v1)"
        ))
    }

    async fn self_oplog(&self) -> Result<String, String> {
        // Anchor the shell instance's own oplog view on its self metadata (a full oplog paging read
        // needs the self agent-id + a get-oplog resource; v1 surfaces the metadata anchor).
        Ok(self_metadata_text())
    }

    async fn rollback(&self) -> Result<String, String> {
        // `revert-agent` on self would rewind state, but choosing the revert target (oplog index vs
        // last-N invocations) is a destructive decision that needs an explicit argument; v1 refuses a
        // bare rollback rather than silently reverting.
        Err("golem rollback needs an explicit revert target (oplog index or last-N) — not wired in v1".to_string())
    }

    async fn fork(&self) -> Result<String, String> {
        // `fork()` forks the current shell instance; the result tells which side we're on.
        // SPIKE (dev SDK): `fork` moved to the crate root (host module is now private).
        use golem_rust::ForkResult;
        match golem_rust::fork() {
            ForkResult::Original(_) => Ok("forked (this is the original instance)".to_string()),
            ForkResult::Forked(_) => Ok("forked (this is the new instance)".to_string()),
        }
    }
}
