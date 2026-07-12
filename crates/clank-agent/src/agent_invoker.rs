//! The durable Golem-agent invoker backing installed agent commands on the Golem agent.
//!
//! `clank-shell` defines the [`AgentInvoker`](clank_shell::agentcmd::AgentInvoker) seam; this module
//! (wasm-only agent crate) implements it with `golem-rust`'s generic `WasmRpc` host resource, which
//! invokes an arbitrary agent type by name in the configured Golem cluster. Mirrors the
//! `AskProvider`→`DurableAnthropicProvider` and `McpHttp`→`WstdMcpHttp` seams.
//!
//! **Argument encoding (v1):** constructor + method arguments are CLI strings. Each is encoded as a
//! `String` element via the `Schema` trait (`ElementValue::ComponentModel`) and packed positionally
//! into a `DataValue::Tuple` (declaration order = CLI order). This covers string-typed agent
//! parameters — the common case for a shell-driven invocation. Rich/typed parameters are a future
//! refinement that would consult the agent's reflected `data-schema`.
//!
//! The invocation is **await mode**: `invoke-and-await` blocks (under the Golem reactor) until the
//! remote agent returns, and the result `DataValue` is rendered to text.

use clank_shell::agentcmd::{AgentInvocation, AgentInvoker};
use golem_rust::agentic::Schema;
use golem_rust::golem_agentic::golem::agent::common::DataValue;
use golem_rust::golem_agentic::golem::agent::host::WasmRpc;

/// Encode CLI string args (in order) as a `DataValue::Tuple` of string `ComponentModel` elements.
fn encode_args(args: &[(String, String)]) -> DataValue {
    let elements = args
        .iter()
        .filter_map(|(_name, value)| value.clone().to_element_value().ok())
        .collect();
    DataValue::Tuple(elements)
}

/// Render a returned `DataValue` to a display string (best-effort). A single string element renders as
/// that string; anything richer falls back to the debug form (honest, not lossy-silent).
fn render_result(value: DataValue) -> String {
    match value {
        DataValue::Tuple(mut elements) if elements.len() == 1 => {
            let ev = elements.remove(0);
            match <String as Schema>::from_element_value(ev.clone()) {
                Ok(s) => s,
                Err(_) => format!("{ev:?}"),
            }
        }
        other => format!("{other:?}"),
    }
}

/// An [`AgentInvoker`] backed by the durable `WasmRpc` client.
pub struct WasmRpcInvoker;

#[async_trait::async_trait(?Send)]
impl AgentInvoker for WasmRpcInvoker {
    async fn invoke(&self, inv: &AgentInvocation) -> Result<String, String> {
        // Build the RPC client for the target agent type, with the constructor tuple identifying the
        // instance (the runtime upserts: finds-or-creates the agent — README:803).
        let ctor = encode_args(&inv.constructor);
        let client = WasmRpc::new(&inv.agent_type, &ctor, None, &[]);

        // Await-mode invocation.
        let input = encode_args(&inv.args);
        match client.invoke_and_await(&inv.method, &input) {
            Ok(result) => Ok(render_result(result)),
            Err(e) => Err(format!("agent invocation failed: {e:?}")),
        }
    }
}
