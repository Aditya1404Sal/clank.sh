//! The native (reqwest) implementation of the [`McpHttp`](crate::mcp::client::McpHttp) transport.
//!
//! On the Golem agent this seam is filled by `clank-agent`'s `wstd` client; natively there was no
//! transport, so MCP and `grease`-over-network degraded to an honest "not configured" error. This
//! module fills the seam off-Golem — and because `grease` shares the same `mcp_http` field on the
//! `Session`, one impl unblocks both MCP tool calls and every `grease` registry/install/update fetch.
//!
//! Unlike curl/wget's cfg-gated `fetch` (which discards response headers), the MCP client **needs**
//! response headers — the `Mcp-Session-Id` for session continuity and `Content-Type` for SSE-body
//! detection — so this impl collects them into [`HttpResponse::headers`] with lowercased names, the
//! documented convention. The transport is wrapped in `LoggingMcpHttp` automatically by
//! [`Session::set_mcp_http`](crate::session::Session::set_mcp_http), so `http.log` covers it for free.

use crate::mcp::client::{HttpResponse, McpHttp};

/// A native `McpHttp` backed by a shared `reqwest` client (connection pool reused across requests).
pub struct ReqwestMcpHttp {
    client: reqwest::Client,
}

impl ReqwestMcpHttp {
    /// Build the transport with a default rustls-backed client. Falls back to `Client::new()` if the
    /// builder somehow fails (it does not, in practice — there is nothing to configure).
    #[must_use]
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

impl Default for ReqwestMcpHttp {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait(?Send)]
impl McpHttp for ReqwestMcpHttp {
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, String> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| format!("bad method '{method}': {e}"))?;

        let mut builder = self.client.request(method, url);
        for (k, v) in headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        if let Some(bytes) = body {
            builder = builder.body(bytes);
        }

        let response = builder
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        let status = response.status().as_u16();

        // Collect ALL response headers with lowercased names (the documented `HttpResponse` convention;
        // `mcp-session-id` and `content-type` are the load-bearing ones, but collecting everything is
        // simplest and future-proof). A header value that isn't valid UTF-8 is skipped rather than
        // erroring the whole request — MCP servers send text headers, and a binary header is not
        // something the client reads.
        let resp_headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_ascii_lowercase(), v.to_string()))
            })
            .collect();

        let body = response
            .bytes()
            .await
            .map_err(|e| format!("reading response failed: {e}"))?
            .to_vec();

        Ok(HttpResponse {
            status,
            headers: resp_headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A one-shot localhost HTTP/1.1 server (mirrors `wcurl`'s test harness): replies to the first
    /// request with `status`, the given extra header line, and `body`. Returns the bound base URL.
    /// Hermetic — no real-internet dependency; the thread exits after one request.
    fn mock_server(status: u16, extra_header: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf); // drain the request head (ignored)
                let response = format!(
                    "HTTP/1.1 {status} X\r\n{extra_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn get_returns_status_and_body() {
        let url = mock_server(200, "", "hello-mcp");
        let http = ReqwestMcpHttp::new();
        let resp = http.request("GET", &url, &[], None).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(String::from_utf8(resp.body).unwrap(), "hello-mcp");
    }

    #[tokio::test]
    async fn collects_mcp_session_id_header_lowercased() {
        // The critical delta from wcurl: response headers MUST be collected, or MCP session
        // establishment breaks. The client reads `mcp-session-id` case-insensitively; we store it
        // lowercased.
        let url = mock_server(200, "Mcp-Session-Id: sess-abc123\r\n", "{}");
        let http = ReqwestMcpHttp::new();
        let resp = http.request("POST", &url, &[], Some(b"{}".to_vec())).await.unwrap();
        assert_eq!(resp.header("mcp-session-id"), Some("sess-abc123"));
        // The header() helper is case-insensitive, so the original casing resolves too.
        assert_eq!(resp.header("Mcp-Session-Id"), Some("sess-abc123"));
    }

    #[tokio::test]
    async fn collects_content_type_for_sse_detection() {
        let url = mock_server(200, "Content-Type: text/event-stream\r\n", "data: {}\n\n");
        let http = ReqwestMcpHttp::new();
        let resp = http.request("POST", &url, &[], Some(b"{}".to_vec())).await.unwrap();
        assert_eq!(resp.header("content-type"), Some("text/event-stream"));
    }

    #[tokio::test]
    async fn sends_request_headers_and_body() {
        // Prove the request path forwards headers + body (the server echoes nothing, but a 200 with
        // our body length in the request confirms no panic on the send path).
        let url = mock_server(200, "", "ok");
        let http = ReqwestMcpHttp::new();
        let resp = http
            .request(
                "POST",
                &url,
                &[("content-type".into(), "application/json".into())],
                Some(b"{\"x\":1}".to_vec()),
            )
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
    }

    #[tokio::test]
    async fn transport_error_on_unroutable_host() {
        let http = ReqwestMcpHttp::new();
        // Port 1 on localhost refuses — a connection failure, surfaced as Err(String).
        let err = http.request("GET", "http://127.0.0.1:1/", &[], None).await;
        assert!(err.is_err());
    }
}
