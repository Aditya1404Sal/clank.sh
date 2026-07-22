//! The durable Anthropic provider backing `ask` on the Golem agent.
//!
//! **dev-SDK build.** The original provider used `golem-ai-llm` / `golem-ai-llm-anthropic` (crates.io),
//! which hard-pin `golem-rust = "=2.1.0"` — irreconcilable with the dev SDK this branch builds against.
//! Rather than wait for a dev-SDK-compatible golem-ai-llm, this provider now talks to Anthropic's
//! `POST /v1/messages` directly over `wstd::http` — the same durable HTTP client `WstdMcpHttp` uses for
//! MCP and `wcurl`/`waget` use for `curl`/`wget`. On Golem the runtime records the HTTP call in the
//! oplog and replays it on recovery, so the LLM response is not re-billed after a restart — the same
//! durability guarantee `golem-ai-llm`'s `DurableAnthropic` provided, obtained from the transport
//! instead of a wrapper crate.
//!
//! The request/response wire mapping (neutral `AskTurn`/`AskTool`/… ↔ Anthropic JSON) is shared with
//! the native reqwest provider via [`clank_shell::ai::anthropic_wire`], so the two can't drift. This
//! module is just the `wstd` transport + key resolution around it.
//!
//! **API key**: read from `ANTHROPIC_API_KEY` in the agent environment (supplied through golem.yaml at
//! deploy time). Absent/empty ⇒ an honest "not configured" [`AskResponse`], so `ask` degrades cleanly
//! rather than sending an unauthenticated request.

use clank_shell::ai::anthropic_wire::{
    build_request, parse_error, parse_response_body, serialize_request, ANTHROPIC_VERSION,
    MESSAGES_URL,
};
use clank_shell::ai::ask::{AskProvider, AskResponse, AskTool, AskTurn};

/// Cap on a single response body (bounds a runaway/held-open server). An `ask` reply is at most a few
/// hundred KB of JSON; 8 MiB is generous headroom.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// An [`AskProvider`] that POSTs to the Anthropic Messages API over the durable `wstd` client.
pub struct DurableAnthropicProvider;

#[async_trait::async_trait(?Send)]
impl AskProvider for DurableAnthropicProvider {
    async fn turn(
        &self,
        system: Option<&str>,
        history: &[AskTurn],
        tools: &[AskTool],
        model: &str,
    ) -> AskResponse {
        // The API key comes from ANTHROPIC_API_KEY in the agent environment (golem.yaml). Empty/unset ⇒
        // report not-configured instead of sending an unauthenticated request.
        let api_key = match std::env::var("ANTHROPIC_API_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => {
                return AskResponse::error(
                    "ask: model provider not configured: set ANTHROPIC_API_KEY in the agent \
                     environment (golem.yaml passes it through at deploy time)\n",
                );
            }
        };

        let body_bytes = serialize_request(&build_request(system, history, tools, model));

        match send(&api_key, body_bytes).await {
            Ok((status, text)) if (200..300).contains(&status) => parse_response_body(&text),
            Ok((status, text)) => AskResponse::error(format!(
                "ask: model call failed: HTTP {status} — {}\n",
                parse_error(&text)
            )),
            Err(e) => AskResponse::error(format!("ask: model call failed: {e}\n")),
        }
    }
}

/// POST the request body to the Anthropic Messages API over the durable `wstd` client and return
/// `(status, body_text)`. Mirrors [`crate::mcp_http::WstdMcpHttp::request`]'s transport (the Golem
/// runtime records this call in the oplog and replays it on recovery).
async fn send(api_key: &str, body: Vec<u8>) -> Result<(u16, String), String> {
    use wstd::http::{Body, Client, Request};

    let request = Request::builder()
        .method(wstd::http::Method::POST)
        .uri(MESSAGES_URL)
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .map_err(|e| format!("bad request: {e}"))?;

    let mut response = Client::new()
        .send(request)
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = response.status().as_u16();
    // Reject an over-cap body BEFORE materializing it, when the (untrusted) LLM endpoint declares its
    // size — the post-hoc check alone bounds the returned value, not peak allocation (audit P1-4).
    if let Some(len) = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
    {
        if len > MAX_BODY {
            return Err(format!("response Content-Length {len} exceeds {MAX_BODY} bytes"));
        }
    }
    let bytes = response
        .body_mut()
        .contents()
        .await
        .map_err(|e| format!("reading response failed: {e}"))?
        .to_vec();
    if bytes.len() > MAX_BODY {
        return Err(format!("response body exceeded {MAX_BODY} bytes"));
    }

    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}
