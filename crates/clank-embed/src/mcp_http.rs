//! The durable WASI-HTTP transport backing MCP on the Golem agent.
//!
//! `clank-core` defines the [`McpHttp`](clank_core::mcp::client::McpHttp) seam but is dual-target and
//! can't link a Golem-host-only HTTP client. This module (in `clank-embed`, for any Golem agent
//! embedding the shell) implements it over [`whttp`] — the same shared transport `wcurl`/`waget` use
//! on both targets, and (post-consolidation) `clank-native`'s five reqwest-backed providers too.
//! `whttp` already does everything this transport needs — redirect policy, timeouts, and a body cap
//! that rejects an advertised-oversize response before it's read and stops a streamed one the
//! instant the cap is crossed — so this module is now just: build a [`whttp::Request`], call
//! [`whttp::fetch`], map the result onto [`HttpResponse`]. On wasm `whttp` is itself backed by
//! `wasi-fetch` over the wasip3 WASI-HTTP bindings, mirroring `wcurl`'s transport. The Golem runtime
//! records the HTTP call in the oplog and replays it on recovery, so the `mcp add`/`tools/list`
//! install flow is durable and replay-deterministic.
//!
//! A response `Content-Type: text/event-stream` (SSE) body is read to EOF like any other — MCP-lite
//! issues one request/response per call (no subscriptions), so the server closes the stream after
//! answering. `whttp`'s body cap bounds a misbehaving server.

use clank_core::config::limits::MAX_HTTP_BODY as MAX_BODY;
use clank_core::mcp::client::{HttpResponse, McpHttp};

/// An [`McpHttp`] backed by the durable `whttp` transport (`wasi-fetch` under the hood on wasm).
pub(crate) struct WasiFetchMcpHttp;

/// Every failure this transport can diagnose is a transport failure — it either could not send the
/// request or could not read the response. Anything about the *content* of a well-formed response
/// is the MCP client's to classify (`Protocol`), not ours.
fn transport(msg: impl Into<String>) -> clank_core::mcp::Error {
    clank_core::mcp::Error::transport(msg.into())
}

/// Map a [`whttp::Error`] onto this module's error type, preserving the DISTINCTION its `Display`
/// already carries: `whttp::Error::BodyTooLarge` renders as "response body exceeded N bytes" (a
/// body-cap rejection), never folded into the generic "request failed: …" wording a real transport
/// failure gets. Collapsing both into one phrasing would lose exactly the information a caller — or
/// a human reading `http.log` — needs to tell a hung/unreachable server from one that answered
/// honestly with too much data, so this just carries whttp's own wording over unchanged.
fn map_err(e: &whttp::Error) -> clank_core::mcp::Error {
    transport(e.to_string())
}

#[async_trait::async_trait(?Send)]
impl McpHttp for WasiFetchMcpHttp {
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> clank_core::mcp::error::Result<HttpResponse> {
        let parsed = method
            .parse::<http::Method>()
            .map_err(|e| transport(format!("bad method '{method}': {e}")))?;

        let mut req = whttp::Request::new(parsed, url);
        req.headers = headers.to_vec();
        req.body = body;
        // MCP speaks to one endpoint and expects to see a 3xx itself (the caller decides what a
        // redirect means for a session) — whttp's default is already no-follow; set it explicitly
        // so the intent is visible here too, not just three modules away.
        req.follow_redirects = false;
        // MCP replies are JSON-RPC envelopes destined for a model's context, held to a tighter bound
        // than an ordinary curl/wget fetch (whttp's own `DEFAULT_MAX_BODY` is far larger).
        req.max_body = MAX_BODY;
        // Bound the exchange. Without this an MCP server that accepts the connection and never
        // answers parks the invocation forever — and Golem serializes invocations per instance, so
        // everything queued behind it is stuck too.
        req.timeout = Some(clank_core::config::net::REQUEST_TIMEOUT);

        let resp = whttp::fetch(&req).await.map_err(|e| map_err(&e))?;

        Ok(HttpResponse {
            status: resp.status,
            // `whttp::Response::headers` are ALREADY lowercased in practice: both its transports
            // build them from `http::HeaderName`, which the `http` crate normalizes to lowercase on
            // construction (`HeaderName::as_str` "will always be lower case"). Normalizing again
            // here is belt-and-suspenders — it keeps `HttpResponse`'s documented "names lowercased"
            // contract enforced by THIS module, rather than resting on an upstream implementation
            // detail this module doesn't own and whttp makes no API promise about.
            headers: resp
                .headers
                .into_iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v))
                .collect(),
            body: resp.body,
        })
    }
}
