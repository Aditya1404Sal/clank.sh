//! The native+cluster Golem implementations: an [`AgentInvoker`] and a [`GolemCluster`] that talk to
//! a Golem cluster over its HTTP REST API (the off-Golem mirror of `clank-agent`'s `WasmRpcInvoker` /
//! `GolemApiCluster`, which use in-process Golem host bindings).
//!
//! **Invoker** — the valuable, tractable path. Agent invocations POST to the agent-native endpoint
//! `POST {url}/v1/agents/invoke-agent` (Golem's `AgentInvocationRequest` → `AgentInvocationResult`),
//! addressed by application + environment + agent-type + parameters — NOT a constructed agent-id, so
//! it sidesteps the reflected-constructor-schema gap that blocks the read-ops below.
//!
//! **Argument encoding (v1):** constructor + method arguments are CLI strings, encoded as a JSON
//! array of strings in `parameters` / `methodParameters` — the same string-only limitation the durable
//! `WasmRpcInvoker` documents (rich/typed params await the reflected `data-schema`). Documented, not
//! hidden.
//!
//! **Cluster read-ops** (`agent list`/`oplog`/`status`/`connect`) — honest "needs the agent's
//! constructor schema" stubs, matching the wasm `GolemApiCluster`'s own v1 parity (it stubs the same
//! ops for the same reason). `rollback`/`fork`/`self_oplog` are in-instance host operations with no
//! native+cluster equivalent, so they report an honest error too.

use crate::golem::agent::{AgentInvocation, AgentInvoker, InvokeHandle, InvokeMode};
use crate::golem::cluster::GolemCluster;
use crate::golem::config_native::ClusterConfig;

use serde_json::{json, Value};

/// The agent-native invoke endpoint path (Golem worker-service `AgentsApi`).
const INVOKE_PATH: &str = "/v1/agents/invoke-agent";

/// Shared builder for a `reqwest` client used by both the invoker and the cluster.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Encode CLI `(name, value)` string args as a JSON array of the string values, in order (Golem's
/// agent identity + method args are positional). String-only in v1 (see module docs).
fn encode_params(args: &[(String, String)]) -> Value {
    Value::Array(args.iter().map(|(_n, v)| json!(v)).collect())
}

/// Build the `AgentInvocationRequest` JSON body (camelCase, per Golem's `AgentsApi`).
fn invoke_body(cfg: &ClusterConfig, inv: &AgentInvocation) -> Value {
    let (mode, schedule_at) = match &inv.mode {
        InvokeMode::Schedule(when) => ("schedule", Some(when.clone())),
        // Trigger has no dedicated REST mode here; it maps to `await` semantics at the transport
        // (fire-and-forget is handled by not blocking on the client side — but the REST call is still
        // await-shaped, so we render it as an immediate invocation). `Await` is the default.
        _ => ("await", None),
    };
    let mut body = json!({
        "appName": cfg.app_name,
        "envName": cfg.env_name,
        "agentTypeName": inv.agent_type,
        "parameters": encode_params(&inv.constructor),
        "methodName": inv.method,
        "methodParameters": encode_params(&inv.args),
        "mode": mode,
    });
    if let Some(uuid) = &inv.phantom {
        body["phantomId"] = json!(uuid);
    }
    if let Some(when) = schedule_at {
        body["scheduleAt"] = json!(when);
    }
    body
}

/// Render an `AgentInvocationResult` JSON body to a display string: the `result` value if present,
/// else a note that the invocation completed. A `TypedSchemaValue` is rendered best-effort (its inner
/// value if it's a simple wrapper, else the compact JSON).
fn render_result(v: &Value) -> String {
    match v.get("result") {
        Some(Value::Null) | None => "(no result)\n".to_string(),
        Some(result) => {
            // A `TypedSchemaValue` typically wraps `{ "value": <json> }` (or is the value directly).
            // Prefer the inner value; render a JSON string without quotes for the common string case.
            let inner = result.get("value").unwrap_or(result);
            match inner {
                Value::String(s) => format!("{s}\n"),
                other => format!("{other}\n"),
            }
        }
    }
}

/// A native [`AgentInvoker`] backed by the Golem agent REST API.
pub struct NativeHttpAgentInvoker {
    client: reqwest::Client,
    cfg: ClusterConfig,
    /// Overridable endpoint base for tests (defaults to `cfg.url`).
    endpoint: String,
}

impl NativeHttpAgentInvoker {
    pub fn new(cfg: ClusterConfig) -> Self {
        let endpoint = format!("{}{INVOKE_PATH}", cfg.url);
        Self { client: client(), cfg, endpoint }
    }

    #[cfg(test)]
    fn with_endpoint(cfg: ClusterConfig, endpoint: impl Into<String>) -> Self {
        Self { client: client(), cfg, endpoint: endpoint.into() }
    }

    /// POST the invocation and return the parsed result JSON (or an error string).
    async fn post(&self, inv: &AgentInvocation) -> Result<Value, String> {
        let body = invoke_body(&self.cfg, inv);
        let mut req = self
            .client
            .post(&self.endpoint)
            .header("content-type", "application/json");
        if let Some(token) = &self.cfg.token {
            req = req.bearer_auth(token);
        }
        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("agent invocation request failed: {e}"))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| format!("reading agent invocation response failed: {e}"))?;
        if !status.is_success() {
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .and_then(|e| e.as_str())
                        .map(str::to_string)
                        .or_else(|| v.get("message").and_then(|m| m.as_str()).map(str::to_string))
                })
                .unwrap_or_else(|| text.chars().take(300).collect());
            return Err(format!("agent invocation failed: HTTP {} — {detail}", status.as_u16()));
        }
        serde_json::from_str::<Value>(&text)
            .map_err(|e| format!("malformed agent invocation response: {e}"))
    }
}

#[async_trait::async_trait(?Send)]
impl AgentInvoker for NativeHttpAgentInvoker {
    async fn invoke(&self, inv: &AgentInvocation) -> Result<String, String> {
        let result = self.post(inv).await?;
        Ok(render_result(&result))
    }

    async fn invoke_async(&self, inv: &AgentInvocation) -> Result<InvokeHandle, String> {
        match &inv.mode {
            InvokeMode::Trigger => {
                // Fire-and-forget: POST but don't render a result. No cancel token (the REST invoke
                // response carries an idempotency key we could later use, but cancel-after-return is
                // not wired in v1 — honest, matching the wasm invoker).
                self.post(inv).await?;
                Ok(InvokeHandle {
                    cancel_token: None,
                    note: "triggered (fire-and-forget)".to_string(),
                })
            }
            InvokeMode::Schedule(when) => {
                // Scheduled: the REST call registers the future invocation (mode=schedule). No cancel
                // token surfaced in v1.
                self.post(inv).await?;
                Ok(InvokeHandle {
                    cancel_token: None,
                    note: format!("scheduled for {when}"),
                })
            }
            InvokeMode::Await => Err("invoke_async called with Await mode".to_string()),
        }
    }
}

/// A native [`GolemCluster`] over REST. The invoke path lives in [`NativeHttpAgentInvoker`]; the
/// read-ops here mirror the wasm `GolemApiCluster`'s v1 parity — the ones that need a constructed
/// agent-id (from the reflected constructor schema) are honest "needs schema" stubs, and the
/// in-instance ops (`rollback`/`fork`/`self_oplog`) have no native+cluster equivalent.
pub struct NativeHttpGolemCluster {
    #[allow(dead_code)]
    client: reqwest::Client,
    #[allow(dead_code)]
    cfg: ClusterConfig,
}

impl NativeHttpGolemCluster {
    pub fn new(cfg: ClusterConfig) -> Self {
        Self { client: client(), cfg }
    }
}

/// The shared "needs the reflected constructor schema" message — the same reason the wasm
/// `GolemApiCluster` stubs these ops in v1.
fn needs_schema(op: &str, agent_type: &str) -> String {
    format!(
        "golem {op} for '{agent_type}': needs the agent's reflected constructor schema to build its \
         agent-id (not wired in v1 — the same limit the in-cluster path has). Invocation of the agent \
         executable works; this read-op does not yet."
    )
}

#[async_trait::async_trait(?Send)]
impl GolemCluster for NativeHttpGolemCluster {
    async fn agent_list(&self) -> Result<String, String> {
        Err("golem agent list: full enumeration over REST needs a component filter (not wired in \
             v1); invoke agent executables directly instead"
            .to_string())
    }

    async fn agent_oplog(&self, agent_type: &str, _ctor: &[(String, String)]) -> Result<String, String> {
        Err(needs_schema("agent oplog", agent_type))
    }

    async fn agent_status(&self, agent_type: &str, _ctor: &[(String, String)]) -> Result<String, String> {
        Err(needs_schema("agent status", agent_type))
    }

    async fn connect(&self, identity: &str) -> Result<String, String> {
        Err(format!(
            "golem connect '{identity}': streaming agent inspection over REST is not wired in v1"
        ))
    }

    async fn self_oplog(&self) -> Result<String, String> {
        Err("golem oplog: the shell instance's own oplog is an in-instance operation with no \
             native+cluster equivalent (available inside Golem)"
            .to_string())
    }

    async fn rollback(&self) -> Result<String, String> {
        Err("golem rollback: rewinding the shell instance is a Golem-only operation (available \
             inside Golem, not native+cluster)"
            .to_string())
    }

    async fn fork(&self) -> Result<String, String> {
        Err("golem fork: forking the shell instance is a Golem-only operation (available inside \
             Golem, not native+cluster)"
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn test_cfg() -> ClusterConfig {
        ClusterConfig {
            url: "http://placeholder".to_string(),
            token: Some("tok".to_string()),
            app_name: "myapp".to_string(),
            env_name: "local".to_string(),
        }
    }

    #[test]
    fn invoke_body_maps_fields_camelcase() {
        let cfg = test_cfg();
        let inv = AgentInvocation::new(
            "ShoppingCart".to_string(),
            vec![("userid".into(), "jd".into())],
            "add-item".to_string(),
            vec![("sku".into(), "abc123".into())],
        );
        let body = invoke_body(&cfg, &inv);
        assert_eq!(body["appName"], "myapp");
        assert_eq!(body["envName"], "local");
        assert_eq!(body["agentTypeName"], "ShoppingCart");
        assert_eq!(body["methodName"], "add-item");
        assert_eq!(body["mode"], "await");
        // Constructor + method args are positional string arrays.
        assert_eq!(body["parameters"], json!(["jd"]));
        assert_eq!(body["methodParameters"], json!(["abc123"]));
        assert!(body.get("phantomId").is_none());
        assert!(body.get("scheduleAt").is_none());
    }

    #[test]
    fn invoke_body_schedule_mode_and_phantom() {
        let cfg = test_cfg();
        let mut inv = AgentInvocation::new("A".into(), vec![], "m".into(), vec![]);
        inv.mode = InvokeMode::Schedule("2026-06-01T09:00:00Z".into());
        inv.phantom = Some("550e8400e29b41d4a716446655440000".into());
        let body = invoke_body(&cfg, &inv);
        assert_eq!(body["mode"], "schedule");
        assert_eq!(body["scheduleAt"], "2026-06-01T09:00:00Z");
        assert_eq!(body["phantomId"], "550e8400e29b41d4a716446655440000");
    }

    #[test]
    fn render_result_extracts_string_value() {
        assert_eq!(
            render_result(&json!({ "result": { "value": "item added" } })),
            "item added\n"
        );
        // Direct string result (no wrapper).
        assert_eq!(render_result(&json!({ "result": "ok" })), "ok\n");
        // No result.
        assert_eq!(render_result(&json!({ "agentId": {} })), "(no result)\n");
    }

    /// One-shot localhost server that captures the request body and replies with `resp_body`.
    fn mock_agent_server(resp_body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                    resp_body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}{INVOKE_PATH}")
    }

    #[tokio::test]
    async fn invoke_round_trips_over_rest() {
        let url = mock_agent_server(
            r#"{"agentId":{"componentId":"c","agentId":"a"},"result":{"value":"item added"}}"#,
        );
        let invoker = NativeHttpAgentInvoker::with_endpoint(test_cfg(), url);
        let inv = AgentInvocation::new(
            "ShoppingCart".into(),
            vec![("userid".into(), "jd".into())],
            "add-item".into(),
            vec![("sku".into(), "abc".into())],
        );
        let out = invoker.invoke(&inv).await.unwrap();
        assert_eq!(out, "item added\n");
    }

    #[tokio::test]
    async fn invoke_surfaces_http_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let body = r#"{"error":"agent not found"}"#;
                let response = format!(
                    "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        let url = format!("http://{addr}{INVOKE_PATH}");
        let invoker = NativeHttpAgentInvoker::with_endpoint(test_cfg(), url);
        let inv = AgentInvocation::new("Missing".into(), vec![], "m".into(), vec![]);
        let err = invoker.invoke(&inv).await.unwrap_err();
        assert!(err.contains("404"), "got: {err}");
        assert!(err.contains("agent not found"), "got: {err}");
    }

    #[tokio::test]
    async fn cluster_read_ops_are_honest_stubs() {
        let cluster = NativeHttpGolemCluster::new(test_cfg());
        assert!(cluster.agent_oplog("A", &[]).await.unwrap_err().contains("schema"));
        assert!(cluster.agent_status("A", &[]).await.unwrap_err().contains("schema"));
        assert!(cluster.rollback().await.unwrap_err().contains("Golem-only"));
        assert!(cluster.fork().await.unwrap_err().contains("Golem-only"));
    }
}
