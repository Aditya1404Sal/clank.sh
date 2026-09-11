//! The durable Anthropic provider backing `ask` on the Golem agent.
//!
//! **dev-SDK build.** The original provider used `golem-ai-llm` / `golem-ai-llm-anthropic` (crates.io),
//! which hard-pin `golem-rust = "=2.1.0"` — irreconcilable with the dev SDK this branch builds against.
//! Rather than wait for a dev-SDK-compatible golem-ai-llm, this provider now talks to Anthropic's
//! `POST /v1/messages` directly over [`whttp`] — the same shared transport
//! [`WasiFetchMcpHttp`](crate::mcp_http::WasiFetchMcpHttp) uses for MCP and `wcurl`/`waget` use for
//! `curl`/`wget` (and, post-consolidation, `clank-native`'s five reqwest-backed providers natively).
//! On Golem the runtime records the HTTP call in the oplog and replays it on recovery, so the LLM
//! response is not re-billed after a restart — the same durability guarantee `golem-ai-llm`'s
//! `DurableAnthropic` provided, obtained from the transport instead of a wrapper crate.
//!
//! The request/response wire mapping (neutral `AskTurn`/`AskTool`/… ↔ Anthropic JSON) is shared with
//! the native reqwest provider via [`clank_core::ai::anthropic_wire`], so the two can't drift. This
//! module is just the transport around it (build a [`whttp::Request`], call [`whttp::fetch`]) + key
//! resolution.
//!
//! **API key**: read from `ANTHROPIC_API_KEY` in the agent environment (supplied through golem.yaml at
//! deploy time). Absent/empty ⇒ an honest "not configured" [`AskResponse`], so `ask` degrades cleanly
//! rather than sending an unauthenticated request.

use clank_core::ai::anthropic_wire::{
    ANTHROPIC_VERSION, MESSAGES_URL, build_request, parse_error, parse_response_body,
    serialize_request,
};
use clank_core::ai::ask::{AskProvider, AskResponse, AskTool, AskTurn};
use clank_core::ai::error::Error;

/// Cap on a single response body (bounds a runaway/held-open server). An `ask` reply is at most a few
/// hundred KB of JSON; 8 MiB is generous headroom.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// An [`AskProvider`] that POSTs to the Anthropic Messages API over the durable `whttp` transport.
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
        // `provider/model` → (provider, bare). No prefix ⇒ the default provider. The Messages API
        // wants the BARE id: sending `anthropic/claude-…` verbatim is a 404 from Anthropic, and
        // `model default anthropic/claude-sonnet-4-5` writes exactly that prefixed form into
        // ask.toml. The native dispatcher (`clank_native::llm`) splits the same way.
        let (provider, bare) = match model.split_once('/') {
            Some((p, m)) => (p, m),
            None => (clank_core::config::model::DEFAULT_PROVIDER, model),
        };
        // `model` accepts every provider clank knows, but this transport speaks only Anthropic —
        // the multi-provider dispatcher depends on golem-ai-llm, which hard-pins golem-rust 2.1.0
        // and cannot be linked on this SDK track. Say so, rather than posting an openai model id to
        // Anthropic and surfacing whatever HTTP error comes back.
        if provider != clank_core::config::model::DEFAULT_PROVIDER {
            // Not a transport failure — the model id names a provider this transport plain cannot
            // serve. Same bucket `resolve_ask_model` uses for an unknown provider prefix: the caller
            // can fix it by picking a different model.
            return AskResponse::error(Error::Model(format!(
                "ask: provider '{provider}' is not available on this agent build (only \
                 '{}' is); choose an {} model, or run `ask` natively where every provider is \
                 wired\n",
                clank_core::config::model::DEFAULT_PROVIDER,
                clank_core::config::model::DEFAULT_PROVIDER,
            )));
        }

        // The API key comes from ANTHROPIC_API_KEY in the agent environment (golem.yaml). Empty/unset ⇒
        // report not-configured instead of sending an unauthenticated request.
        let api_key = match std::env::var("ANTHROPIC_API_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => {
                return AskResponse::error(Error::NotConfigured(
                    "ask: model provider not configured: set ANTHROPIC_API_KEY in the agent \
                     environment (golem.yaml passes it through at deploy time)\n"
                        .to_string(),
                ));
            }
        };

        let body_bytes = serialize_request(&build_request(system, history, tools, bare));

        match send(&api_key, body_bytes).await {
            Ok((status, text)) if (200..300).contains(&status) => parse_response_body(&text),
            Ok((status, text)) => {
                let message = format!(
                    "ask: model call failed: HTTP {status} — {}\n",
                    parse_error(&text)
                );
                // `send` returns only `(status, body)` — `whttp::Response`'s headers are discarded
                // before they reach `turn`, so a `Retry-After` value is not reachable here without
                // widening `send`'s return type. `retry_after: None` is honest about that, not a
                // guess; native's reqwest-based providers DO thread it through, since their headers
                // are still in scope at this point (see `clank-native::anthropic`/`openai`).
                match status {
                    401 | 403 => AskResponse::error(Error::Unauthorized(message)),
                    429 => AskResponse::error(Error::RateLimited {
                        message,
                        retry_after: None,
                    }),
                    _ => AskResponse::error(Error::Request(message)),
                }
            }
            // Folds a real transport failure together with `send`'s own pre-flight rejection (an
            // `ANTHROPIC_API_KEY` value that isn't a legal header value) — both arrive here as one
            // flat `String` with nothing to branch on, so both land in the catch-all bucket. The
            // header-value case is arguably closer to `NotConfigured`, but splitting it out would mean
            // giving `send` a structured error type for one caller; not done here.
            Err(e) => AskResponse::error(Error::Request(format!("ask: model call failed: {e}\n"))),
        }
    }
}

/// POST the request body to the Anthropic Messages API over `whttp` and return `(status, body_text)`.
/// Mirrors [`crate::mcp_http::WasiFetchMcpHttp::request`]'s transport (the Golem runtime records this
/// call in the oplog and replays it on recovery).
async fn send(api_key: &str, body: Vec<u8>) -> Result<(u16, String), String> {
    // Validate the key's header value explicitly, AHEAD of whttp's own (generic) header validation.
    // whttp rejects an unparseable header too, but as "bad value for request header 'x-api-key': …"
    // — this is the one header on this transport carrying operator-supplied secret material, and a
    // stray newline (a common golem.yaml env-literal copy-paste artifact) is a real, previously-seen
    // footgun worth naming directly rather than leaving to whttp's generic wording.
    if http::HeaderValue::try_from(api_key).is_err() {
        return Err(
            "ANTHROPIC_API_KEY contains characters that are not valid in an HTTP header \
             (a stray newline is the usual cause)"
                .to_string(),
        );
    }

    let mut req = whttp::Request::new(http::Method::POST, MESSAGES_URL);
    req.headers = vec![
        ("x-api-key".to_string(), api_key.to_string()),
        (
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string(),
        ),
        ("content-type".to_string(), "application/json".to_string()),
    ];
    req.body = Some(body);
    // The Messages API does not redirect; whttp's default is already no-follow — set it explicitly
    // anyway so the intent is visible here too.
    req.follow_redirects = false;
    req.max_body = MAX_BODY;
    // A model call legitimately takes minutes on a large completion — the same LLM_TIMEOUT bound the
    // native Anthropic provider (`clank_native::anthropic`) applies, so `ask` doesn't get a shorter
    // fuse just because it's running on the durable agent instead of natively. (The plain
    // REQUEST_TIMEOUT `whttp::client()`-backed sites use is too short for this workload.)
    req.timeout = Some(clank_core::config::net::LLM_TIMEOUT);

    // whttp::Error's own Display already distinguishes a body-cap rejection ("response body exceeded
    // N bytes") from a genuine transport failure ("request failed: …"), so no remapping is needed to
    // preserve that distinction — just carry the message over.
    let resp = whttp::fetch(&req).await.map_err(|e| e.to_string())?;

    Ok((
        resp.status,
        String::from_utf8_lossy(&resp.body).into_owned(),
    ))
}
