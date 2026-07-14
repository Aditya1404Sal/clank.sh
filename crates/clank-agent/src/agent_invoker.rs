//! The Golem-agent invoker backing installed agent commands on the Golem agent.
//!
//! **SPIKE (spike/dev-golem-sdk): STUBBED.** The real invoker used `golem-rust`'s generic `WasmRpc`
//! host resource with the `DataValue`/`ElementValue` value model. The dev SDK (`golem:agent@2.0.0`)
//! deleted that model, replacing it with `schema-value-tree`/`SchemaValue`, so the real invoker is a
//! near-total rewrite. This spike proves clank's SHELL builds+runs on the dev SDK; wRPC invocation of
//! OTHER agents is out of scope, so `WasmRpcInvoker` is stubbed to an honest error. Grease-installed
//! agent executables report "not available on this spike build"; the shell is unaffected.

use clank_shell::golem::agent::{AgentInvocation, AgentInvoker, InvokeHandle};

const STUB: &str =
    "agent invocation is not available on this spike build (clank on the dev golem SDK); the wRPC \
     invoker awaits a rewrite onto the new schema-value-tree model";

/// An [`AgentInvoker`] stub for the dev-SDK spike (see module docs).
pub struct WasmRpcInvoker;

#[async_trait::async_trait(?Send)]
impl AgentInvoker for WasmRpcInvoker {
    async fn invoke(&self, _inv: &AgentInvocation) -> Result<String, String> {
        Err(STUB.to_string())
    }

    async fn invoke_async(&self, _inv: &AgentInvocation) -> Result<InvokeHandle, String> {
        Err(STUB.to_string())
    }

    async fn cancel(&self, _token: &str) -> Result<bool, String> {
        Err(STUB.to_string())
    }
}
