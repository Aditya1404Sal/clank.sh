//! The durable `wstd` HTTP transport backing MCP on the Golem agent.
//!
//! `clank-core` defines the [`McpHttp`](clank_core::mcp::client::McpHttp) seam but is dual-target and
//! can't link the Golem-host-only `wstd` client. This module (wasm-only agent crate) implements it with
//! `wstd::http`, mirroring `wcurl`'s wasm `fetch` and additionally collecting response headers (MCP
//! needs the `Mcp-Session-Id`). The Golem runtime records the HTTP call in the oplog and replays it on
//! recovery, so the `mcp add`/`tools/list` install flow is durable and replay-deterministic.
//!
//! A response `Content-Type: text/event-stream` (SSE) body is read to EOF like any other — MCP-lite
//! issues one request/response per call (no subscriptions), so the server closes the stream after
//! answering. A body cap bounds a misbehaving server.

use clank_core::mcp::client::{HttpResponse, McpHttp};

use clank_core::config::limits::MAX_HTTP_BODY as MAX_BODY;

/// An [`McpHttp`] backed by the durable `wstd` client.
pub(crate) struct WstdMcpHttp;

#[async_trait::async_trait(?Send)]
impl McpHttp for WstdMcpHttp {
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> clank_core::mcp::error::Result<HttpResponse> {
        use wstd::http::{Body, Client, Method, Request};

        let method = method.parse::<Method>().map_err(|e| {
            clank_core::mcp::Error::transport(format!("bad method '{method}': {e}"))
        })?;
        let mut builder = Request::builder().method(method).uri(url);
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
        let wstd_body = match body {
            Some(bytes) => Body::from(bytes),
            None => Body::empty(),
        };
        let request = builder
            .body(wstd_body)
            .map_err(|e| clank_core::mcp::Error::transport(format!("bad request: {e}")))?;

        // Bound the exchange. Without this an MCP server that accepts the connection and never
        // answers parks the invocation forever — and Golem serializes invocations per instance, so
        // everything queued behind it is stuck too.
        let mut client = Client::new();
        client.set_connect_timeout(clank_core::config::net::CONNECT_TIMEOUT);
        client.set_first_byte_timeout(clank_core::config::net::REQUEST_TIMEOUT);
        let mut response = client
            .send(request)
            .await
            .map_err(|e| clank_core::mcp::Error::transport(format!("request failed: {e}")))?;

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
        // Reject on the advertised length BEFORE reading. The post-read check below used to be the
        // only one, which meant `MAX_BODY` bounded the value we returned but not peak allocation —
        // a hostile or misconfigured server could OOM the durable worker before the cap was ever
        // consulted. (Residual: wstd's `contents()` is all-or-nothing, so a chunked response with no
        // Content-Length is still buffered first; the post-read check is what catches that case.)
        if response
            .body()
            .content_length()
            .is_some_and(|len| len > MAX_BODY as u64)
        {
            return Err(clank_core::mcp::Error::transport(format!(
                "response body exceeded {MAX_BODY} bytes"
            )));
        }
        let bytes = response
            .body_mut()
            .contents()
            .await
            .map_err(|e| {
                clank_core::mcp::Error::transport(format!("reading response failed: {e}"))
            })?
            .to_vec();
        if bytes.len() > MAX_BODY {
            return Err(clank_core::mcp::Error::transport(format!(
                "response body exceeded {MAX_BODY} bytes"
            )));
        }

        Ok(HttpResponse {
            status,
            headers: resp_headers,
            body: bytes,
        })
    }
}
