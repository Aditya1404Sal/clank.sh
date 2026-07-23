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

/// Cap on a single response body (bounds a runaway/held-open server).
const MAX_BODY: usize = 4 * 1024 * 1024;

/// An [`McpHttp`] backed by the durable `wstd` client.
pub struct WstdMcpHttp;

#[async_trait::async_trait(?Send)]
impl McpHttp for WstdMcpHttp {
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, String> {
        use wstd::http::{Body, Client, Method, Request};

        let method = method
            .parse::<Method>()
            .map_err(|e| format!("bad method '{method}': {e}"))?;
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
            .map_err(|e| format!("bad request: {e}"))?;

        let mut response = Client::new()
            .send(request)
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
        let bytes = response
            .body_mut()
            .contents()
            .await
            .map_err(|e| format!("reading response failed: {e}"))?
            .to_vec();
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
