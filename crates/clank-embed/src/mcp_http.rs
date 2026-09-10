//! The durable WASI-HTTP transport backing MCP on the Golem agent.
//!
//! `clank-core` defines the [`McpHttp`](clank_core::mcp::client::McpHttp) seam but is dual-target and
//! can't link a Golem-host-only HTTP client. This module (in `clank-embed`, for any Golem agent
//! embedding the shell) implements it with `wasi-fetch` over the wasip3 WASI-HTTP bindings,
//! mirroring `wcurl`'s wasm `fetch` and additionally collecting response headers (MCP needs the
//! `Mcp-Session-Id`). The Golem runtime records the HTTP call in the oplog and replays it on
//! recovery, so the `mcp add`/`tools/list` install flow is durable and replay-deterministic.
//!
//! A response `Content-Type: text/event-stream` (SSE) body is read to EOF like any other — MCP-lite
//! issues one request/response per call (no subscriptions), so the server closes the stream after
//! answering. A body cap bounds a misbehaving server.

use clank_core::config::limits::MAX_HTTP_BODY as MAX_BODY;
use clank_core::mcp::client::{HttpResponse, McpHttp};

/// An [`McpHttp`] backed by the durable `wasi-fetch` client.
pub(crate) struct WasiFetchMcpHttp;

/// Every failure this transport can diagnose is a transport failure — it either could not send the
/// request or could not read the response. Anything about the *content* of a well-formed response
/// is the MCP client's to classify (`Protocol`), not ours.
fn transport(msg: impl Into<String>) -> clank_core::mcp::Error {
    clank_core::mcp::Error::transport(msg.into())
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
        // MCP speaks to one endpoint and expects to see a 3xx itself (the caller decides what a
        // redirect means for a session); the previous transport followed none, so neither does this.
        let mut builder = wasi_fetch::Client::new()
            .request(parsed, url)
            .redirect_limit(0);
        // Build the header parts explicitly: `wasi-fetch` SILENTLY DROPS a header whose name or
        // value does not parse, which for MCP would mean quietly omitting the session id or the
        // auth header and getting an opaque 4xx back. Reject it with a real message instead.
        for (k, v) in headers {
            let name = http::HeaderName::try_from(k.as_str())
                .map_err(|e| transport(format!("bad request header '{k}': {e}")))?;
            let value = http::HeaderValue::try_from(v.as_str())
                .map_err(|e| transport(format!("bad value for request header '{k}': {e}")))?;
            builder = builder.header(name, value);
        }
        if let Some(bytes) = body {
            builder = builder.body(bytes);
        }
        // Bound the exchange. Without this an MCP server that accepts the connection and never
        // answers parks the invocation forever — and Golem serializes invocations per instance, so
        // everything queued behind it is stuck too. This client exposes ONE deadline covering both
        // connect and first-byte, so the request budget is the one to spend it on.
        builder = builder.timeout(clank_core::config::net::REQUEST_TIMEOUT);

        let response = builder
            .send()
            .await
            .map_err(|e| transport(format!("request failed: {e}")))?;

        let status = response.status().as_u16();
        let resp_headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        // Reject on the advertised length BEFORE reading. A post-read check alone bounds the value
        // returned but not peak allocation, so a hostile or misconfigured server could OOM the
        // durable worker before the cap was ever consulted (audit P1-4).
        if let Some(len) = resp_headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
            && len > MAX_BODY
        {
            return Err(transport(format!(
                "response Content-Length {len} exceeds {MAX_BODY} bytes"
            )));
        }
        // A chunked response advertises no length, so stream it and stop the moment the cap is
        // crossed. main's wstd arm had to document this as a residual — its `contents()` was
        // all-or-nothing, so such a body was fully buffered before the post-read check could fire.
        // `wasi-fetch`'s `Body::chunk` reads incrementally, so nothing past the cap is ever held.
        let mut stream = response.into_body();
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.chunk().await {
            if bytes.len() + chunk.len() > MAX_BODY {
                return Err(transport(format!(
                    "response body exceeded {MAX_BODY} bytes"
                )));
            }
            bytes.extend_from_slice(&chunk);
        }

        Ok(HttpResponse {
            status,
            headers: resp_headers,
            body: bytes,
        })
    }
}
