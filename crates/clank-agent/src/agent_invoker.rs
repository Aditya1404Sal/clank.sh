//! The durable Golem-agent invoker backing installed agent commands on the Golem agent.
//!
//! `clank-core` defines the [`AgentInvoker`](clank_core::golem::agent::AgentInvoker) seam; this module
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

use clank_core::golem::Error;
use clank_core::golem::agent::{
    AgentInvocation, AgentInvoker, InvokeHandle, InvokeMode, parse_epoch_secs,
};
use golem_rust::agentic::Schema;
use golem_rust::golem_agentic::golem::agent::common::DataValue;
use golem_rust::golem_agentic::golem::agent::host::WasmRpc;

/// Encode CLI string args (in order) as a `DataValue::Tuple` of string `ComponentModel` elements.
///
/// Fails loudly on an element that won't encode. This used to `filter_map(..ok())`, silently dropping
/// it — which changed the tuple's ARITY after `order_agent_params` had already validated it, and a
/// wrong-arity wRPC call is a fatal host trap that wedges the durable instance (see the note in
/// `clank_core::session::agent`). Losing an argument is never the better outcome.
fn encode_args(args: &[(String, String)]) -> Result<DataValue, Error> {
    let mut elements = Vec::with_capacity(args.len());
    for (name, value) in args {
        // `Invalid`: the request never left, and the caller can fix it.
        let encoded = value
            .clone()
            .to_element_value()
            .map_err(|e| Error::Invalid(format!("cannot encode argument '{name}': {e:?}")))?;
        elements.push(encoded);
    }
    Ok(DataValue::Tuple(elements))
}

/// Parse a `--phantom <uuid>` string into the WIT `uuid` (two u64 halves, `golem:core/types`).
/// Best-effort: on a malformed UUID we return `None` (the canonical, non-phantom instance).
// signature mirrors the caller's `phantom: Option<String>` field, borrowed as `&inv.phantom`
#[allow(clippy::ref_option)]
fn parse_phantom(
    phantom: &Option<String>,
) -> Option<golem_rust::golem_wasm::golem_core_1_5_x::types::Uuid> {
    use golem_rust::golem_wasm::golem_core_1_5_x::types::Uuid;
    let s = phantom.as_ref()?;
    let hex: String = s.chars().filter(char::is_ascii_hexdigit).collect();
    if hex.len() != 32 {
        return None;
    }
    let high = u64::from_str_radix(&hex[..16], 16).ok()?;
    let low = u64::from_str_radix(&hex[16..], 16).ok()?;
    Some(Uuid {
        high_bits: high,
        low_bits: low,
    })
}

/// Build the `WasmRpc` client for an invocation (agent type + constructor tuple + optional phantom).
fn build_client(inv: &AgentInvocation) -> Result<WasmRpc, Error> {
    let ctor = encode_args(&inv.constructor)?;
    Ok(WasmRpc::new(
        &inv.agent_type,
        &ctor,
        parse_phantom(&inv.phantom),
        &[],
    ))
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
pub(crate) struct WasmRpcInvoker;

#[async_trait::async_trait(?Send)]
impl AgentInvoker for WasmRpcInvoker {
    async fn invoke(&self, inv: &AgentInvocation) -> clank_core::golem::error::Result<String> {
        // Build the RPC client for the target agent type (the runtime upserts: finds-or-creates the
        // agent — README:803), then await the method result.
        let client = build_client(inv)?;
        let input = encode_args(&inv.args)?;
        match client.invoke_and_await(&inv.method, &input) {
            Ok(result) => Ok(render_result(result)),
            // Classified as `Remote`: the call reached the host binding and the host reported a
            // failure. It is NOT split further into unreachable-vs-trap because the SDK's error is a
            // generated bindings type that exposes no kind — only a Debug rendering. Saying `Remote`
            // and meaning it beats inventing a distinction we cannot actually observe. What the
            // enum DOES buy here is separating this from the failures clank owns: a bad arity or an
            // unencodable argument is now `Invalid` (exit 2), and the honest stubs are `Unsupported`
            // (exit 2), where previously all three arrived as one exit-1 string.
            Err(e) => Err(Error::Remote(format!("{e:?}"))),
        }
    }

    async fn invoke_async(
        &self,
        inv: &AgentInvocation,
    ) -> clank_core::golem::error::Result<InvokeHandle> {
        let client = build_client(inv)?;
        let input = encode_args(&inv.args)?;
        match &inv.mode {
            InvokeMode::Trigger => {
                // Fire-and-forget: `invoke` returns immediately. No cancel token (a queued invocation
                // could be cancelled via async-invoke-and-await's future, but plain trigger has none).
                client
                    .invoke(&inv.method, &input)
                    .map_err(|e| Error::Remote(format!("trigger failed: {e:?}")))?;
                Ok(InvokeHandle {
                    cancel_token: None,
                    note: "triggered (fire-and-forget)".to_string(),
                })
            }
            InvokeMode::Schedule(when) => {
                // `schedule-cancelable-invocation` needs a `wall-clock/datetime`; we build one from the
                // parsed epoch seconds. The returned cancellation-token is a host resource that can't
                // survive across the durable agent's serialized invocations (Golem parks between
                // invocations), so it can't be re-acquired for a later `kill` — the invocation IS
                // scheduled, but cancel-after-return isn't supported (documented, honest handle).
                let secs = parse_epoch_secs(when)?;
                let dt = golem_rust::wasip2::clocks::wall_clock::Datetime {
                    seconds: secs,
                    nanoseconds: 0,
                };
                let _token = client.schedule_cancelable_invocation(dt, &inv.method, &input);
                Ok(InvokeHandle {
                    cancel_token: None,
                    note: format!("scheduled for {when}"),
                })
            }
            InvokeMode::Await => Err(Error::Invalid(
                "invoke_async called with Await mode".to_string(),
            )),
        }
    }

    async fn cancel(&self, _token: &str) -> clank_core::golem::error::Result<bool> {
        // A scheduled invocation's cancellation-token is a host resource that doesn't survive across
        // the durable agent's serialized invocations, so we can't re-acquire it here to cancel. Honest:
        // cancel-after-return isn't supported on this SDK surface for scheduled invocations.
        Err(Error::Unsupported(
            "cancel of a scheduled invocation is not supported across invocations on this build"
                .to_string(),
        ))
    }
}

// `parse_epoch_secs` lives in `clank_core::golem::agent` — target-agnostic arithmetic, and this crate
// is a wasm-only cdylib with no test target, so it could not be tested here.
