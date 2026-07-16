//! The durable Golem-agent invoker backing installed agent commands on the Golem agent.
//!
//! `clank-shell` defines the [`AgentInvoker`](clank_shell::golem::agent::AgentInvoker) seam; this module
//! (in `clank-embed`, for any Golem agent embedding the shell) implements it with `golem-rust`'s
//! generic `WasmRpc` host resource, which
//! invokes an arbitrary agent type by name in the configured Golem cluster. Mirrors the
//! `AskProvider`→`DurableAnthropicProvider` and `McpHttp`→`WstdMcpHttp` seams.
//!
//! **Argument encoding (v1):** constructor + method arguments are CLI strings, packed positionally into
//! a `SchemaValue::Record` and lowered with `encode_schema_value`. The value side carries no field
//! names — declaration order IS the wire contract (`golem-schema`'s `SchemaValue` docs; the schema
//! supplies the names) — which is exactly clank's positional-CLI model. This covers string-typed agent
//! parameters, the common case for a shell-driven invocation. Rich/typed parameters are a future
//! refinement that would consult the agent's reflected `data-schema`.
//!
//! The invocation is **await mode**: `invoke-and-await` blocks (under the Golem reactor) until the
//! remote agent returns, and the result tree is rendered to text.

use clank_shell::golem::agent::{AgentInvocation, AgentInvoker, InvokeHandle, InvokeMode};
use golem_rust::bindings::golem::agent::host::WasmRpc;
use golem_rust::schema::wit::wire::{SchemaValueTree, Uuid};
use golem_rust::{SchemaValue, decode_schema_value, encode_schema_value};

/// Encode CLI string args (in order) as the wire tree for a parameter list: a positional
/// `SchemaValue::Record` whose fields are the argument values.
///
/// Per-argument lowering is infallible (`SchemaValue::String` is a direct variant), so an argument can
/// never be silently dropped — which would shift every later positional arg and call the remote agent
/// with the wrong parameters. Only the single whole-tree encode can fail, and that error is propagated.
fn encode_args(args: &[(String, String)]) -> Result<SchemaValueTree, String> {
    let fields = args.iter().map(|(_name, value)| SchemaValue::String(value.clone())).collect();
    encode_schema_value(&SchemaValue::Record { fields })
        .map_err(|e| format!("failed to encode agent arguments: {e:?}"))
}

/// Parse a `--phantom <uuid>` string into the WIT `uuid` (two u64 halves, `golem:core/types`).
/// Best-effort: on a malformed UUID we return `None` (the canonical, non-phantom instance).
fn parse_phantom(phantom: &Option<String>) -> Option<Uuid> {
    let s = phantom.as_ref()?;
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return None;
    }
    let high = u64::from_str_radix(&hex[..16], 16).ok()?;
    let low = u64::from_str_radix(&hex[16..], 16).ok()?;
    Some(Uuid { high_bits: high, low_bits: low })
}

/// Build the `WasmRpc` client for an invocation (agent type + constructor tree + optional phantom).
/// The constructor tree is passed **by value**: a value tree is affine (it can carry owned handles), so
/// the host takes ownership rather than borrowing.
fn build_client(inv: &AgentInvocation) -> Result<WasmRpc, String> {
    let ctor = encode_args(&inv.constructor)?;
    Ok(WasmRpc::new(&inv.agent_type, ctor, parse_phantom(&inv.phantom), Vec::new()))
}

/// Render an `invoke-and-await` result to a display string.
///
/// `None` is the remote method's **`unit` return** — a success with no value, not an error (`host.wit`:
/// "the result is `none` for a `unit` output and `some(value)` for a `single` output"), so it renders
/// empty. A `single` output is the **bare value**, NOT wrapped in a record (only inputs are): a string
/// renders as itself, anything richer falls back to the debug form (honest, not lossy-silent).
fn render_result(result: Option<SchemaValueTree>) -> Result<String, String> {
    let Some(tree) = result else {
        return Ok(String::new());
    };
    match decode_schema_value(tree) {
        Ok(SchemaValue::String(s)) => Ok(s),
        Ok(other) => Ok(format!("{other:?}")),
        Err(e) => Err(format!("failed to decode the agent's result: {e:?}")),
    }
}

/// An [`AgentInvoker`] backed by the durable `WasmRpc` client.
pub struct WasmRpcInvoker;

#[async_trait::async_trait(?Send)]
impl AgentInvoker for WasmRpcInvoker {
    async fn invoke(&self, inv: &AgentInvocation) -> Result<String, String> {
        // Build the RPC client for the target agent type (the runtime upserts: finds-or-creates the
        // agent — README:803), then await the method result.
        let client = build_client(inv)?;
        let input = encode_args(&inv.args)?;
        match client.invoke_and_await(&inv.method, input) {
            Ok(result) => render_result(result),
            Err(e) => Err(format!("agent invocation failed: {e:?}")),
        }
    }

    async fn invoke_async(&self, inv: &AgentInvocation) -> Result<InvokeHandle, String> {
        let client = build_client(inv)?;
        let input = encode_args(&inv.args)?;
        match &inv.mode {
            InvokeMode::Trigger => {
                // Fire-and-forget: `invoke` returns immediately. No cancel token (a queued invocation
                // could be cancelled via async-invoke-and-await's future, but plain trigger has none).
                client
                    .invoke(&inv.method, input)
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
                let _token = client.schedule_cancelable_invocation(dt, &inv.method, input);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a tree back to a `SchemaValue` (the inverse of `encode_args`), for round-trip asserts.
    fn decode(tree: SchemaValueTree) -> SchemaValue {
        decode_schema_value(tree).expect("decode")
    }

    fn args(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect()
    }

    #[test]
    fn encode_args_packs_values_positionally_in_order() {
        // The wire contract: a positional Record of the VALUES; names never travel. Order is the only
        // thing binding an argument to a parameter, so it must survive the round-trip exactly.
        let tree = encode_args(&args(&[("who", "world"), ("greeting", "hi")])).expect("encode");
        assert_eq!(
            decode(tree),
            SchemaValue::Record {
                fields: vec![
                    SchemaValue::String("world".to_string()),
                    SchemaValue::String("hi".to_string()),
                ],
            },
        );
    }

    #[test]
    fn encode_args_of_no_args_is_an_empty_record() {
        // A zero-arg method must still send a record (an empty parameter list), not a unit/absent tree.
        let tree = encode_args(&[]).expect("encode");
        assert_eq!(decode(tree), SchemaValue::Record { fields: Vec::new() });
    }

    #[test]
    fn encode_args_preserves_duplicate_and_empty_values() {
        // Positional encoding must not dedupe or drop empties — either would shift later args.
        let tree = encode_args(&args(&[("a", ""), ("b", "x"), ("c", "x")])).expect("encode");
        let SchemaValue::Record { fields } = decode(tree) else {
            panic!("expected a record");
        };
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0], SchemaValue::String(String::new()));
    }

    #[test]
    fn render_result_maps_none_to_the_unit_return() {
        // `none` is a `unit` output — a SUCCESS with no value, not a failure.
        assert_eq!(render_result(None), Ok(String::new()));
    }

    #[test]
    fn render_result_unwraps_a_bare_string() {
        // A `single` output is the bare value, NOT wrapped in a record (only inputs are).
        let tree = encode_schema_value(&SchemaValue::String("Hello, world".to_string())).expect("encode");
        assert_eq!(render_result(Some(tree)), Ok("Hello, world".to_string()));
    }

    #[test]
    fn render_result_falls_back_to_debug_for_a_richer_value() {
        // Honest rather than lossy-silent: a non-string result still shows something.
        let tree = encode_schema_value(&SchemaValue::U64(42)).expect("encode");
        let rendered = render_result(Some(tree)).expect("render");
        assert!(rendered.contains("42"), "expected the debug form to carry the value, got {rendered:?}");
    }

    #[test]
    fn parse_phantom_accepts_a_canonical_uuid() {
        let uuid = parse_phantom(&Some("936DA01F-9ABD-4D9D-80C7-02AF85C822A8".to_string()))
            .expect("a canonical uuid parses");
        assert_eq!(uuid.high_bits, 0x936DA01F9ABD4D9D);
        assert_eq!(uuid.low_bits, 0x80C702AF85C822A8);
    }

    #[test]
    fn parse_phantom_rejects_malformed_and_absent() {
        assert!(parse_phantom(&None).is_none());
        assert!(parse_phantom(&Some("not-a-uuid".to_string())).is_none(), "too few hex digits");
        assert!(parse_phantom(&Some("f".repeat(33))).is_none(), "too many hex digits");
    }

    #[test]
    fn parse_epoch_secs_parses_an_iso_timestamp() {
        // 2026-06-01T09:00:00Z — cross-checked against the Unix epoch.
        assert_eq!(parse_epoch_secs("2026-06-01T09:00:00Z"), Ok(1_780_304_400));
        // The epoch itself, and a Z-less form.
        assert_eq!(parse_epoch_secs("1970-01-01T00:00:00Z"), Ok(0));
        assert_eq!(parse_epoch_secs("1970-01-01T00:00:01"), Ok(1));
    }

    #[test]
    fn parse_epoch_secs_rejects_garbage_and_pre_epoch() {
        assert!(parse_epoch_secs("tomorrow").is_err(), "no date/time separator");
        assert!(parse_epoch_secs("1969-12-31T23:59:59Z").is_err(), "before the epoch");
    }
}
