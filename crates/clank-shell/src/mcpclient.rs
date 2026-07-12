//! The MCP (Model Context Protocol) client protocol layer: JSON-RPC 2.0 over Streamable HTTP.
//!
//! MCP-lite scope: `initialize` → `notifications/initialized` → `tools/list` (paginated) →
//! `tools/call` → session `close` (HTTP DELETE). HTTPS-only (wasm can't spawn stdio servers). Resources,
//! prompts, `watch`, and OAuth are deferred (see [`crate::mcpcmd`]).
//!
//! The HTTP transport is a **trait seam** ([`McpHttp`]) — unlike curl/wget's cfg-gated `fetch`, MCP
//! needs response *headers* (the `Mcp-Session-Id`) and scriptable multi-step fakes for tests. The
//! concrete `wstd` implementation lives in `clank-agent` (`WstdMcpHttp`) and is injected into the
//! `Session`; on native, no transport is installed and MCP degrades to an honest error.
//!
//! Servers may answer `application/json` or `text/event-stream` (SSE). The Streamable HTTP spec
//! requires the client to `Accept` both, so SSE bodies must be handled — [`extract_sse_json_rpc`]
//! pulls the matching JSON-RPC response out of a complete SSE body.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The MCP protocol version clank advertises (Streamable HTTP).
pub const PROTOCOL_VERSION: &str = "2025-03-26";

/// Max `tools/list` pages to follow (bounds a misbehaving server's pagination).
const MAX_TOOL_PAGES: usize = 16;

// ---- transport seam -------------------------------------------------------------------------------

/// One HTTP response: status, headers (lowercased names), and the full body.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The first header whose (case-insensitive) name matches `name`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Whether the response body is SSE (`Content-Type: text/event-stream`).
    fn is_sse(&self) -> bool {
        self.header("content-type")
            .map(|ct| ct.to_ascii_lowercase().contains("text/event-stream"))
            .unwrap_or(false)
    }
}

/// The MCP HTTP transport. Implemented in `clank-agent` by the durable `wstd` client and injected into
/// the `Session`; absent on native (MCP degrades to an honest error there).
#[async_trait::async_trait(?Send)]
pub trait McpHttp {
    /// Perform one HTTP request. `headers` are name/value pairs to send; `body` is the request body
    /// (`None` for GET/DELETE). Returns the full response or a transport-error message.
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, String>;
}

// ---- JSON-RPC + MCP types -------------------------------------------------------------------------

#[derive(Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Serialize)]
struct JsonRpcNotification<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    #[serde(default)]
    id: Value,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

/// The result of a successful `initialize`.
#[derive(Clone, Debug)]
pub struct InitializeResult {
    pub protocol_version: String,
    pub server_name: String,
    pub server_version: String,
    /// The session id from the `Mcp-Session-Id` response header, if the server issued one.
    pub session_id: Option<String>,
    /// The raw capabilities object, kept for `mcp session info`.
    pub capabilities: Value,
}

/// One tool advertised by a server (`tools/list`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: Option<String>,
    /// The tool's JSON-schema input, kept lossless (drives arg mapping + the ask ToolDefinition).
    pub input_schema: Value,
}

/// One prompt advertised by a server (`prompts/list`). `arguments` are the declared prompt arguments
/// (name + required flag), used to build a standalone prompt package at grease install time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptSpec {
    pub name: String,
    pub description: Option<String>,
    /// `(arg-name, description, required)` for each declared argument.
    pub arguments: Vec<(String, String, bool)>,
}

/// One resource advertised by a server (`resources/list`).
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceSpec {
    pub uri: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    /// Byte size, if the server advertises it.
    pub size: Option<u64>,
    /// `annotations.lastModified` (ISO-8601), surfaced by `stat`/`mcp resource info`.
    pub last_modified: Option<String>,
    /// `annotations.audience` (e.g. `["user","assistant"]`), joined for display.
    pub audience: Option<String>,
    /// `annotations.priority` (0.0–1.0).
    pub priority: Option<f64>,
}

/// One resource TEMPLATE advertised by a server (`resources/templates/list`) — a parameterized URI
/// pattern that can't be enumerated. Installed as an executable in `/usr/lib/mcp/bin/` (README:767).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceTemplateSpec {
    /// The RFC-6570 URI template, e.g. `github://repo/{path}`.
    pub uri_template: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

/// The result of a `tools/call`.
#[derive(Clone, Debug)]
pub struct CallToolResult {
    /// The text content items joined; non-text items render as `[non-text content: <type>]`.
    pub text: String,
    /// The raw `result` object, for `--json`.
    pub raw: Value,
    /// Whether the tool itself reported an error (`isError: true`) — protocol succeeded, tool failed.
    pub is_error: bool,
}

/// An MCP client error, carrying the clank exit code the caller should surface.
#[derive(Clone, Debug)]
pub struct McpError {
    pub message: String,
    pub exit_code: u8,
}

impl McpError {
    fn transport(msg: impl Into<String>) -> Self {
        Self { message: msg.into(), exit_code: 4 }
    }
    fn usage(msg: impl Into<String>) -> Self {
        Self { message: msg.into(), exit_code: 2 }
    }
    fn tool(msg: impl Into<String>) -> Self {
        Self { message: msg.into(), exit_code: 1 }
    }
}

type McpResult<T> = Result<T, McpError>;

// ---- SSE extraction (pure) ------------------------------------------------------------------------

/// Extract the JSON-RPC *response* (an object with a `result` or `error`, not a notification) from a
/// complete `text/event-stream` body. Events are separated by blank lines; `data:` lines within an
/// event are concatenated and parsed as JSON. Returns the first parseable response object.
pub fn extract_sse_json_rpc(body: &str) -> Result<Value, String> {
    for event in body.split("\n\n") {
        let mut data = String::new();
        for line in event.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest.trim_start());
            }
        }
        if data.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&data) {
            // A JSON-RPC response carries result or error; skip notifications (method, no id).
            if v.get("result").is_some() || v.get("error").is_some() {
                return Ok(v);
            }
        }
    }
    Err("server replied with an event stream that did not contain a JSON-RPC response".into())
}

/// Parse the JSON-RPC response body — either a plain JSON object or an SSE-wrapped one.
fn parse_response_body(resp: &HttpResponse) -> McpResult<JsonRpcResponse> {
    let text = String::from_utf8_lossy(&resp.body);
    let value: Value = if resp.is_sse() {
        extract_sse_json_rpc(&text).map_err(McpError::transport)?
    } else {
        serde_json::from_str(&text).map_err(|e| McpError::transport(format!("bad JSON response: {e}")))?
    };
    serde_json::from_value(value)
        .map_err(|e| McpError::transport(format!("malformed JSON-RPC response: {e}")))
}

/// Map a JSON-RPC error object to an [`McpError`] with the right exit code.
fn rpc_error_to_mcp(err: JsonRpcError) -> McpError {
    match err.code {
        // Method-not-found / invalid-params are usage-shaped (exit 2).
        -32601 | -32602 => McpError::usage(format!("MCP error {}: {}", err.code, err.message)),
        _ => McpError::transport(format!("MCP error {}: {}", err.code, err.message)),
    }
}

// ---- the client -----------------------------------------------------------------------------------

/// Auth to attach to MCP requests: a header name and the *value* already resolved from the env var.
#[derive(Clone, Debug, Default)]
pub struct McpAuth {
    pub header: String,
    pub value: String,
}

/// An MCP client bound to one server URL, driving requests through an [`McpHttp`] transport. Holds a
/// monotonic JSON-RPC id counter (deterministic under replay).
pub struct McpClient<'a> {
    http: &'a dyn McpHttp,
    url: String,
    auth: Option<McpAuth>,
    next_id: u64,
}

impl<'a> McpClient<'a> {
    pub fn new(http: &'a dyn McpHttp, url: impl Into<String>, auth: Option<McpAuth>) -> Self {
        Self { http, url: url.into(), auth, next_id: 1 }
    }

    fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// The common request headers (Accept both content types per the spec, plus optional auth and an
    /// optional session id).
    fn headers(&self, session_id: Option<&str>) -> Vec<(String, String)> {
        let mut h = vec![
            ("Content-Type".into(), "application/json".into()),
            ("Accept".into(), "application/json, text/event-stream".into()),
            ("MCP-Protocol-Version".into(), PROTOCOL_VERSION.into()),
        ];
        if let Some(auth) = &self.auth {
            h.push((auth.header.clone(), auth.value.clone()));
        }
        if let Some(sid) = session_id {
            h.push(("Mcp-Session-Id".into(), sid.to_string()));
        }
        h
    }

    /// POST a JSON-RPC request and return its parsed response value (result), erroring on transport,
    /// HTTP status, or a JSON-RPC error object.
    async fn call(&mut self, method: &str, params: Option<Value>, session_id: Option<&str>) -> McpResult<Value> {
        let id = self.id();
        let req = JsonRpcRequest { jsonrpc: "2.0", id, method, params };
        let body = serde_json::to_vec(&req).map_err(|e| McpError::usage(format!("encode: {e}")))?;
        let resp = self
            .http
            .request("POST", &self.url, &self.headers(session_id), Some(body))
            .await
            .map_err(|e| McpError::transport(format!("request failed: {e}")))?;
        if resp.status >= 400 {
            return Err(McpError::transport(format!("server returned status {}", resp.status)));
        }
        let parsed = parse_response_body(&resp)?;
        if let Some(err) = parsed.error {
            return Err(rpc_error_to_mcp(err));
        }
        parsed
            .result
            .ok_or_else(|| McpError::transport("response had neither result nor error"))
    }

    /// Send a JSON-RPC notification (no response expected).
    async fn notify(&self, method: &str, session_id: Option<&str>) -> McpResult<()> {
        let notif = JsonRpcNotification { jsonrpc: "2.0", method, params: None };
        let body = serde_json::to_vec(&notif).map_err(|e| McpError::usage(format!("encode: {e}")))?;
        self.http
            .request("POST", &self.url, &self.headers(session_id), Some(body))
            .await
            .map_err(|e| McpError::transport(format!("request failed: {e}")))?;
        Ok(())
    }

    /// `initialize` + `notifications/initialized`. Captures the session id from the response header.
    pub async fn initialize(&mut self) -> McpResult<InitializeResult> {
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "clank.sh", "version": env!("CARGO_PKG_VERSION") },
        });
        let id = self.id();
        let req = JsonRpcRequest { jsonrpc: "2.0", id, method: "initialize", params: Some(params) };
        let body = serde_json::to_vec(&req).map_err(|e| McpError::usage(format!("encode: {e}")))?;
        let resp = self
            .http
            .request("POST", &self.url, &self.headers(None), Some(body))
            .await
            .map_err(|e| McpError::transport(format!("request failed: {e}")))?;
        if resp.status >= 400 {
            return Err(McpError::transport(format!("server returned status {}", resp.status)));
        }
        let session_id = resp.header("mcp-session-id").map(String::from);
        let parsed = parse_response_body(&resp)?;
        if let Some(err) = parsed.error {
            return Err(rpc_error_to_mcp(err));
        }
        let result = parsed
            .result
            .ok_or_else(|| McpError::transport("initialize had no result"))?;

        let info = InitializeResult {
            protocol_version: result
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION)
                .to_string(),
            server_name: result
                .get("serverInfo")
                .and_then(|s| s.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            server_version: result
                .get("serverInfo")
                .and_then(|s| s.get("version"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            session_id: session_id.clone(),
            capabilities: result.get("capabilities").cloned().unwrap_or(Value::Null),
        };

        // Best-effort initialized notification (non-fatal if it fails).
        let _ = self.notify("notifications/initialized", session_id.as_deref()).await;
        Ok(info)
    }

    /// `tools/list`, following pagination up to [`MAX_TOOL_PAGES`].
    pub async fn list_tools(&mut self, session_id: Option<&str>) -> McpResult<Vec<ToolSpec>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = cursor.as_ref().map(|c| serde_json::json!({ "cursor": c }));
            let result = self.call("tools/list", params, session_id).await?;
            if let Some(arr) = result.get("tools").and_then(Value::as_array) {
                for t in arr {
                    let Some(name) = t.get("name").and_then(Value::as_str) else { continue };
                    tools.push(ToolSpec {
                        name: name.to_string(),
                        description: t.get("description").and_then(Value::as_str).map(String::from),
                        input_schema: t.get("inputSchema").cloned().unwrap_or(serde_json::json!({})),
                    });
                }
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => return Ok(tools),
            }
        }
        Ok(tools)
    }

    /// `tools/call` with the given arguments object.
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        session_id: Option<&str>,
    ) -> McpResult<CallToolResult> {
        let params = serde_json::json!({ "name": name, "arguments": arguments });
        let result = self.call("tools/call", Some(params), session_id).await?;
        let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let text = render_content(result.get("content"));
        if is_error {
            return Err(McpError::tool(if text.is_empty() {
                format!("tool '{name}' reported an error")
            } else {
                text
            }));
        }
        Ok(CallToolResult { text, raw: result, is_error })
    }

    /// `prompts/list` — the server's advertised prompts (name + description + declared arguments).
    /// Paginated like `tools/list`. A server without the prompts capability typically returns a
    /// method-not-found error, which the caller can treat as "no prompts".
    pub async fn list_prompts(&mut self, session_id: Option<&str>) -> McpResult<Vec<PromptSpec>> {
        let mut prompts = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = cursor.as_ref().map(|c| serde_json::json!({ "cursor": c }));
            let result = self.call("prompts/list", params, session_id).await?;
            if let Some(arr) = result.get("prompts").and_then(Value::as_array) {
                for p in arr {
                    let Some(name) = p.get("name").and_then(Value::as_str) else { continue };
                    let arguments = p
                        .get("arguments")
                        .and_then(Value::as_array)
                        .map(|args| {
                            args.iter()
                                .filter_map(|a| {
                                    let n = a.get("name").and_then(Value::as_str)?.to_string();
                                    let d = a
                                        .get("description")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string();
                                    let req = a.get("required").and_then(Value::as_bool).unwrap_or(false);
                                    Some((n, d, req))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    prompts.push(PromptSpec {
                        name: name.to_string(),
                        description: p.get("description").and_then(Value::as_str).map(String::from),
                        arguments,
                    });
                }
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => return Ok(prompts),
            }
        }
        Ok(prompts)
    }

    /// `prompts/get` — fetch a prompt's messages (with the given arguments), joining the text content
    /// into a single string (the body a standalone prompt package would carry / run through `ask`).
    pub async fn get_prompt(
        &mut self,
        name: &str,
        arguments: Value,
        session_id: Option<&str>,
    ) -> McpResult<String> {
        let params = serde_json::json!({ "name": name, "arguments": arguments });
        let result = self.call("prompts/get", Some(params), session_id).await?;
        // Join each message's text content (role prefixes dropped — grease stores a flat prompt body).
        // A message's `content` is a single `{type,text}` object (per the MCP prompt spec), not the
        // array `tools/call` returns, so read the text field directly.
        let mut out = String::new();
        if let Some(msgs) = result.get("messages").and_then(Value::as_array) {
            for m in msgs {
                let text = m
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !text.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
        }
        Ok(out)
    }

    /// `resources/list` — the server's advertised resources (uri + name + description + mimeType).
    /// Paginated like `tools/list`. A server without the resources capability returns method-not-found.
    pub async fn list_resources(&mut self, session_id: Option<&str>) -> McpResult<Vec<ResourceSpec>> {
        let mut resources = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = cursor.as_ref().map(|c| serde_json::json!({ "cursor": c }));
            let result = self.call("resources/list", params, session_id).await?;
            if let Some(arr) = result.get("resources").and_then(Value::as_array) {
                for r in arr {
                    let Some(uri) = r.get("uri").and_then(Value::as_str) else { continue };
                    let ann = r.get("annotations");
                    let audience = ann
                        .and_then(|a| a.get("audience"))
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str())
                                .collect::<Vec<_>>()
                                .join(",")
                        });
                    resources.push(ResourceSpec {
                        uri: uri.to_string(),
                        name: r.get("name").and_then(Value::as_str).map(String::from),
                        description: r.get("description").and_then(Value::as_str).map(String::from),
                        mime_type: r.get("mimeType").and_then(Value::as_str).map(String::from),
                        size: r.get("size").and_then(Value::as_u64),
                        last_modified: ann
                            .and_then(|a| a.get("lastModified"))
                            .and_then(Value::as_str)
                            .map(String::from),
                        audience,
                        priority: ann.and_then(|a| a.get("priority")).and_then(Value::as_f64),
                    });
                }
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => return Ok(resources),
            }
        }
        Ok(resources)
    }

    /// `resources/templates/list` — the server's parameterized-URI resource templates. Paginated like
    /// `resources/list`. A server without templates returns an empty list or method-not-found.
    pub async fn list_resource_templates(
        &mut self,
        session_id: Option<&str>,
    ) -> McpResult<Vec<ResourceTemplateSpec>> {
        let mut templates = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = cursor.as_ref().map(|c| serde_json::json!({ "cursor": c }));
            let result = self.call("resources/templates/list", params, session_id).await?;
            if let Some(arr) = result.get("resourceTemplates").and_then(Value::as_array) {
                for t in arr {
                    let Some(uri_template) = t.get("uriTemplate").and_then(Value::as_str) else {
                        continue;
                    };
                    templates.push(ResourceTemplateSpec {
                        uri_template: uri_template.to_string(),
                        name: t.get("name").and_then(Value::as_str).map(String::from),
                        description: t.get("description").and_then(Value::as_str).map(String::from),
                        mime_type: t.get("mimeType").and_then(Value::as_str).map(String::from),
                    });
                }
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => return Ok(templates),
            }
        }
        Ok(templates)
    }

    /// `resources/subscribe` — subscribe to change notifications for a resource URI. Best-effort (a
    /// server may not support it); `mcp watch` uses this before its poll loop.
    pub async fn subscribe_resource(&mut self, uri: &str, session_id: Option<&str>) -> McpResult<()> {
        let params = serde_json::json!({ "uri": uri });
        self.call("resources/subscribe", Some(params), session_id).await?;
        Ok(())
    }

    /// `resources/read` — read a resource's contents by URI. Joins the text of each returned content
    /// item (the common single-text case yields that text verbatim). Binary (`blob`) contents are
    /// returned base64-decoded when possible, else the raw base64 string.
    pub async fn read_resource(&mut self, uri: &str, session_id: Option<&str>) -> McpResult<String> {
        let params = serde_json::json!({ "uri": uri });
        let result = self.call("resources/read", Some(params), session_id).await?;
        let mut out = String::new();
        if let Some(items) = result.get("contents").and_then(Value::as_array) {
            for item in items {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                } else if let Some(b) = item.get("blob").and_then(Value::as_str) {
                    use base64::Engine;
                    match base64::engine::general_purpose::STANDARD.decode(b) {
                        Ok(bytes) => out.push_str(&String::from_utf8_lossy(&bytes)),
                        Err(_) => out.push_str(b),
                    }
                }
            }
        }
        Ok(out)
    }

    /// Close a session (HTTP DELETE with the session id). A 405 means the server refuses explicit
    /// close — reported as such (exit 1); the caller removes the local session anyway.
    pub async fn close_session(&self, session_id: &str) -> McpResult<()> {
        let resp = self
            .http
            .request("DELETE", &self.url, &self.headers(Some(session_id)), None)
            .await
            .map_err(|e| McpError::transport(format!("request failed: {e}")))?;
        match resp.status {
            200..=299 => Ok(()),
            405 => Err(McpError::tool("server refused session close (HTTP 405)")),
            s => Err(McpError::transport(format!("server returned status {s} on close"))),
        }
    }
}

/// Render an MCP `content` array to text: join `text` items; other item types render a placeholder.
fn render_content(content: Option<&Value>) -> String {
    let Some(arr) = content.and_then(Value::as_array) else {
        return String::new();
    };
    let mut parts = Vec::new();
    for item in arr {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
            Some(other) => parts.push(format!("[non-text content: {other}]")),
            None => {}
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// A scripted [`McpHttp`] fake: replays a queue of responses and records the requests it saw.
    struct FakeHttp {
        responses: RefCell<VecDeque<HttpResponse>>,
        seen: RefCell<Vec<(String, String, Vec<(String, String)>, Option<Vec<u8>>)>>,
    }

    impl FakeHttp {
        fn new(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: RefCell::new(responses.into()),
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl McpHttp for FakeHttp {
        async fn request(
            &self,
            method: &str,
            url: &str,
            headers: &[(String, String)],
            body: Option<Vec<u8>>,
        ) -> Result<HttpResponse, String> {
            self.seen.borrow_mut().push((
                method.to_string(),
                url.to_string(),
                headers.to_vec(),
                body,
            ));
            self.responses
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| "no scripted response".to_string())
        }
    }

    fn json_response(status: u16, value: Value) -> HttpResponse {
        HttpResponse {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: value.to_string().into_bytes(),
        }
    }

    fn block<F: std::future::Future>(f: F) -> F::Output {
        // A minimal executor for the tests (no tokio needed for these single-await futures).
        futures_executor_block_on(f)
    }

    // A tiny block_on without pulling a dep: poll to completion (the fake futures are ready quickly).
    fn futures_executor_block_on<F: std::future::Future>(mut f: F) -> F::Output {
        use std::pin::Pin;
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn noop_raw() -> RawWaker {
            fn no_op(_: *const ()) {}
            fn clone(_: *const ()) -> RawWaker {
                noop_raw()
            }
            RawWaker::new(std::ptr::null(), &RawWakerVTable::new(clone, no_op, no_op, no_op))
        }
        let waker = unsafe { Waker::from_raw(noop_raw()) };
        let mut cx = Context::from_waker(&waker);
        let mut f = unsafe { Pin::new_unchecked(&mut f) };
        loop {
            match f.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => continue,
            }
        }
    }

    #[test]
    fn initialize_captures_session_and_server_info() {
        let mut init = json_response(
            200,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "serverInfo": { "name": "demo", "version": "1.2.3" },
                    "capabilities": { "tools": {} }
                }
            }),
        );
        init.headers.push(("mcp-session-id".into(), "sess-abc".into()));
        // initialized notification response (ignored).
        let notif = json_response(202, serde_json::json!({}));
        let http = FakeHttp::new(vec![init, notif]);
        let mut client = McpClient::new(&http, "https://x/mcp", None);
        let info = block(client.initialize()).unwrap();
        assert_eq!(info.server_name, "demo");
        assert_eq!(info.session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn tools_list_paginates() {
        let page1 = json_response(
            200,
            serde_json::json!({"jsonrpc":"2.0","id":1,"result":{
                "tools":[{"name":"a","inputSchema":{"type":"object"}}], "nextCursor":"c2"}}),
        );
        let page2 = json_response(
            200,
            serde_json::json!({"jsonrpc":"2.0","id":2,"result":{
                "tools":[{"name":"b","description":"beta","inputSchema":{"type":"object"}}]}}),
        );
        let http = FakeHttp::new(vec![page1, page2]);
        let mut client = McpClient::new(&http, "https://x/mcp", None);
        let tools = block(client.list_tools(None)).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "a");
        assert_eq!(tools[1].description.as_deref(), Some("beta"));
    }

    #[test]
    fn call_tool_renders_text_and_flags_error() {
        let ok = json_response(
            200,
            serde_json::json!({"jsonrpc":"2.0","id":1,"result":{
                "content":[{"type":"text","text":"hello"}], "isError":false}}),
        );
        let http = FakeHttp::new(vec![ok]);
        let mut client = McpClient::new(&http, "https://x/mcp", None);
        let r = block(client.call_tool("t", serde_json::json!({}), None)).unwrap();
        assert_eq!(r.text, "hello");
        assert!(!r.is_error);
    }

    #[test]
    fn call_tool_is_error_becomes_exit_1() {
        let err = json_response(
            200,
            serde_json::json!({"jsonrpc":"2.0","id":1,"result":{
                "content":[{"type":"text","text":"boom"}], "isError":true}}),
        );
        let http = FakeHttp::new(vec![err]);
        let mut client = McpClient::new(&http, "https://x/mcp", None);
        let e = block(client.call_tool("t", serde_json::json!({}), None)).unwrap_err();
        assert_eq!(e.exit_code, 1);
        assert!(e.message.contains("boom"));
    }

    #[test]
    fn rpc_error_method_not_found_is_usage() {
        let err = json_response(
            200,
            serde_json::json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"no method"}}),
        );
        let http = FakeHttp::new(vec![err]);
        let mut client = McpClient::new(&http, "https://x/mcp", None);
        let e = block(client.list_tools(None)).unwrap_err();
        assert_eq!(e.exit_code, 2);
    }

    #[test]
    fn close_405_is_a_refusal() {
        let resp = HttpResponse { status: 405, headers: vec![], body: vec![] };
        let http = FakeHttp::new(vec![resp]);
        let client = McpClient::new(&http, "https://x/mcp", None);
        let e = block(client.close_session("s1")).unwrap_err();
        assert_eq!(e.exit_code, 1);
        assert!(e.message.contains("405"));
    }

    #[test]
    fn sse_response_body_is_parsed() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
        let resp = HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: body.as_bytes().to_vec(),
        };
        let http = FakeHttp::new(vec![resp]);
        let mut client = McpClient::new(&http, "https://x/mcp", None);
        let tools = block(client.list_tools(None)).unwrap();
        assert!(tools.is_empty());
    }

    #[test]
    fn extract_sse_skips_notifications_and_finds_response() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"note\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let v = extract_sse_json_rpc(body).unwrap();
        assert!(v.get("result").is_some());
    }

    #[test]
    fn auth_header_is_attached() {
        let init = {
            let mut r = json_response(
                200,
                serde_json::json!({"jsonrpc":"2.0","id":1,"result":{
                    "serverInfo":{"name":"x","version":"1"},"capabilities":{}}}),
            );
            r.headers.push(("mcp-session-id".into(), "s".into()));
            r
        };
        let notif = json_response(202, serde_json::json!({}));
        let http = FakeHttp::new(vec![init, notif]);
        let auth = McpAuth { header: "Authorization".into(), value: "Bearer tok".into() };
        let mut client = McpClient::new(&http, "https://x/mcp", Some(auth));
        block(client.initialize()).unwrap();
        let seen = http.seen.borrow();
        let (_m, _u, headers, _b) = &seen[0];
        assert!(headers.iter().any(|(k, v)| k == "Authorization" && v == "Bearer tok"));
    }
}
