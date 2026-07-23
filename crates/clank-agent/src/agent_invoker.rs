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

use clank_core::golem::agent::{AgentInvocation, AgentInvoker, InvokeHandle, InvokeMode};
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

/// Parse a `--phantom <uuid>` string into the WIT `uuid` (two u64 halves, `golem:core/types`).
/// Best-effort: on a malformed UUID we return `None` (the canonical, non-phantom instance).
fn parse_phantom(phantom: &Option<String>) -> Option<golem_rust::golem_wasm::golem_core_1_5_x::types::Uuid> {
    use golem_rust::golem_wasm::golem_core_1_5_x::types::Uuid;
    let s = phantom.as_ref()?;
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return None;
    }
    let high = u64::from_str_radix(&hex[..16], 16).ok()?;
    let low = u64::from_str_radix(&hex[16..], 16).ok()?;
    Some(Uuid { high_bits: high, low_bits: low })
}

/// Build the `WasmRpc` client for an invocation (agent type + constructor tuple + optional phantom).
fn build_client(inv: &AgentInvocation) -> WasmRpc {
    let ctor = encode_args(&inv.constructor);
    WasmRpc::new(&inv.agent_type, &ctor, parse_phantom(&inv.phantom), &[])
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
        // Build the RPC client for the target agent type (the runtime upserts: finds-or-creates the
        // agent — README:803), then await the method result.
        let client = build_client(inv);
        let input = encode_args(&inv.args);
        match client.invoke_and_await(&inv.method, &input) {
            Ok(result) => Ok(render_result(result)),
            Err(e) => Err(format!("agent invocation failed: {e:?}")),
        }
    }

    async fn invoke_async(&self, inv: &AgentInvocation) -> Result<InvokeHandle, String> {
        let client = build_client(inv);
        let input = encode_args(&inv.args);
        match &inv.mode {
            InvokeMode::Trigger => {
                // Fire-and-forget: `invoke` returns immediately. No cancel token (a queued invocation
                // could be cancelled via async-invoke-and-await's future, but plain trigger has none).
                client
                    .invoke(&inv.method, &input)
                    .map_err(|e| format!("trigger failed: {e:?}"))?;
                Ok(InvokeHandle { cancel_token: None, note: "triggered (fire-and-forget)".to_string() })
            }
            InvokeMode::Schedule(when) => {
                // `schedule-cancelable-invocation` needs a `wall-clock/datetime`; we build one from the
                // parsed epoch seconds. The returned cancellation-token is a host resource that can't
                // survive across the durable agent's serialized invocations (Golem parks between
                // invocations), so it can't be re-acquired for a later `kill` — the invocation IS
                // scheduled, but cancel-after-return isn't supported (documented, honest handle).
                let secs = parse_epoch_secs(when)?;
                let dt = golem_rust::wasip2::clocks::wall_clock::Datetime { seconds: secs, nanoseconds: 0 };
                let _token = client.schedule_cancelable_invocation(dt, &inv.method, &input);
                Ok(InvokeHandle { cancel_token: None, note: format!("scheduled for {when}") })
            }
            InvokeMode::Await => Err("invoke_async called with Await mode".to_string()),
        }
    }

    async fn cancel(&self, _token: &str) -> Result<bool, String> {
        // A scheduled invocation's cancellation-token is a host resource that doesn't survive across
        // the durable agent's serialized invocations, so we can't re-acquire it here to cancel. Honest:
        // cancel-after-return isn't supported on this SDK surface for scheduled invocations.
        Err("cancel of a scheduled invocation is not supported across invocations on this build".to_string())
    }
}

/// Parse an ISO-8601 / RFC-3339 timestamp (e.g. `2026-06-01T09:00:00Z`) to Unix epoch seconds. A small
/// dependency-free parser (clank-agent has no chrono): handles `YYYY-MM-DDThh:mm:ss[Z]`, UTC only.
fn parse_epoch_secs(s: &str) -> Result<u64, String> {
    let err = || format!("invalid --schedule time '{s}' (expected YYYY-MM-DDThh:mm:ssZ)");
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T').ok_or_else(err)?;
    let mut d = date.split('-');
    let year: i64 = d.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let month: i64 = d.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let day: i64 = d.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let mut t = time.split(':');
    let hh: i64 = t.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let mm: i64 = t.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let ss: i64 = t.next().unwrap_or("0").parse().map_err(|_| err())?;
    // Days from the civil calendar (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + hh * 3600 + mm * 60 + ss;
    if secs < 0 {
        return Err("scheduled time is before the epoch".to_string());
    }
    Ok(secs as u64)
}
