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

use clank_core::mcp::client::{HttpResponse, McpHttp};

/// Cap on a single response body (bounds a runaway/held-open server).
const MAX_BODY: usize = 4 * 1024 * 1024;

/// An [`McpHttp`] backed by the durable `wasi-fetch` client.
pub(crate) struct WasiFetchMcpHttp;

#[async_trait::async_trait(?Send)]
impl McpHttp for WasiFetchMcpHttp {
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, String> {
        let parsed = method
            .parse::<http::Method>()
            .map_err(|e| format!("bad method '{method}': {e}"))?;
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
                .map_err(|e| format!("bad request header '{k}': {e}"))?;
            let value = http::HeaderValue::try_from(v.as_str())
                .map_err(|e| format!("bad value for request header '{k}': {e}"))?;
            builder = builder.header(name, value);
        }
        if let Some(bytes) = body {
            builder = builder.body(bytes);
        }

        let response = builder
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

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
        // Reject an over-cap body BEFORE materializing it, when the (untrusted) server declares its
        // size — the post-hoc check alone only bounds the returned value, not peak allocation, so a
        // huge declared body would OOM the durable worker first (audit P1-4). A server that omits
        // Content-Length still hits the post-hoc backstop.
        if let Some(len) = resp_headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
            && len > MAX_BODY
        {
            return Err(format!(
                "response Content-Length {len} exceeds {MAX_BODY} bytes"
            ));
        }
        let bytes = response.into_body().bytes().await.to_vec();
        if bytes.len() > MAX_BODY {
            return Err(format!("response body exceeded {MAX_BODY} bytes"));
        }

        Ok(HttpResponse {
            status,
            headers: resp_headers,
            body: bytes,
        })
    }
}
