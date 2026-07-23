//! The native (reqwest) Anthropic `ask` provider — the off-Golem mirror of `clank-agent`'s durable
//! `DurableAnthropicProvider`.
//!
//! `clank-core` owns the target-agnostic [`AskProvider`](crate::ai::ask::AskProvider) seam and the
//! neutral `AskTurn`/`AskTool`/`AskToolCall`/`AskToolResult`/`AskResponse` types; the durable impl
//! maps those onto `golem-ai-llm` (Golem-host-only). This module maps the SAME neutral types directly
//! onto Anthropic's `POST /v1/messages` JSON wire format via `reqwest` + `serde_json`, so `ask` works
//! natively without the Golem crates. The `Session` owns the multi-turn agentic loop; this provider
//! is a single-turn transport (one [`turn`](AskProvider::turn) = one Anthropic request).
//!
//! **API key** (README §444): read from `~/.config/ask/ask.toml` `[providers.anthropic].key` first,
//! then env `ANTHROPIC_API_KEY` (the same var the agent uses). Absent ⇒ an honest "not configured"
//! [`AskResponse::error`], so `ask` degrades cleanly rather than sending an unauthenticated request.

use serde_json::{json, Value};

use crate::ai::ask::{AskResponse, AskProvider, AskTool, AskToolCall, AskTurn};

/// The Anthropic Messages API endpoint.
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
/// The Anthropic API version header (stable public value).
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Output-token ceiling for an `ask` reply — matches the durable provider's `MAX_TOKENS`.
const MAX_TOKENS: u32 = 4096;
/// The provider name under which the key is stored in `ask.toml`.
const PROVIDER: &str = "anthropic";

/// A native `AskProvider` backed by a shared `reqwest` client hitting the Anthropic Messages API.
pub struct ReqwestAnthropicProvider {
    client: reqwest::Client,
    /// The endpoint to POST to — the real Anthropic URL in production, overridable in tests to point
    /// at a hermetic mock server.
    endpoint: String,
}

impl ReqwestAnthropicProvider {
    /// A new provider with a fresh `reqwest` client pointed at the real Anthropic Messages API.
    #[must_use]
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            endpoint: MESSAGES_URL.to_string(),
        }
    }

    /// Build a provider pointed at a custom endpoint (tests: a localhost mock server).
    #[cfg(test)]
    fn with_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: endpoint.into(),
        }
    }
}

impl Default for ReqwestAnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve the Anthropic API key: `ask.toml` `[providers.anthropic].key` under `$HOME`, else env
/// `ANTHROPIC_API_KEY`. `None` ⇒ the provider reports "not configured".
fn resolve_api_key() -> Option<String> {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(key) = crate::ai::config::provider_key(&home, PROVIDER) {
            if !key.is_empty() {
                return Some(key);
            }
        }
    }
    std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty())
}

#[async_trait::async_trait(?Send)]
impl AskProvider for ReqwestAnthropicProvider {
    async fn turn(
        &self,
        system: Option<&str>,
        history: &[AskTurn],
        tools: &[AskTool],
        model: &str,
    ) -> AskResponse {
        let Some(api_key) = resolve_api_key() else {
            return AskResponse::error(
                "ask: model provider not configured: set ANTHROPIC_API_KEY in the environment \
                 or run `model add anthropic --key <key>` (stores it in ~/.config/ask/ask.toml)\n",
            );
        };

        let body = build_request(system, history, tools, model);

        let response = self
            .client
            .post(&self.endpoint)
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await;

        let response = match response {
            Ok(r) => r,
            Err(e) => return AskResponse::error(format!("ask: model call failed: {e}\n")),
        };

        let status = response.status();
        let text = match response.text().await {
            Ok(t) => t,
            Err(e) => return AskResponse::error(format!("ask: reading model response failed: {e}\n")),
        };

        if !status.is_success() {
            // Anthropic error bodies are JSON `{"error":{"message":...}}`; surface the message if we
            // can parse it, else the raw body (truncated), never the api key (it's not echoed back).
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| text.chars().take(300).collect());
            return AskResponse::error(format!(
                "ask: model call failed: HTTP {} — {detail}\n",
                status.as_u16()
            ));
        }

        match serde_json::from_str::<Value>(&text) {
            Ok(v) => parse_response(&v),
            Err(e) => AskResponse::error(format!("ask: malformed model response: {e}\n")),
        }
    }
}

/// Build the Anthropic `/v1/messages` request body from the neutral turn inputs. Mirrors the durable
/// provider's request assembly (system as a top-level field, `max_tokens`, tools, and the message
/// list from the history).
fn build_request(system: Option<&str>, history: &[AskTurn], tools: &[AskTool], model: &str) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "messages": history.iter().map(turn_to_message).collect::<Vec<_>>(),
    });
    if let Some(sys) = system {
        body["system"] = json!(sys);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(tool_to_json).collect::<Vec<_>>());
    }
    body
}

/// `AskTool` → an Anthropic tool definition. `input_schema` must be a JSON object; if the stored
/// schema string doesn't parse, fall back to a permissive empty-object schema (the model can still
/// call the tool; arguments are validated downstream).
fn tool_to_json(tool: &AskTool) -> Value {
    let schema: Value = serde_json::from_str(&tool.parameters_schema)
        .unwrap_or_else(|_| json!({ "type": "object" }));
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": schema,
    })
}

/// `AskTurn` → one Anthropic message. Preserves the `tool_use` / `tool_result` id linkage the model
/// needs to match a result to its call across turns.
fn turn_to_message(turn: &AskTurn) -> Value {
    match turn {
        AskTurn::User(text) => json!({
            "role": "user",
            "content": [ { "type": "text", "text": text } ],
        }),
        AskTurn::Assistant { text, tool_calls } => {
            let mut content: Vec<Value> = Vec::new();
            if !text.is_empty() {
                content.push(json!({ "type": "text", "text": text }));
            }
            for call in tool_calls {
                // `arguments_json` is always valid JSON (the type's invariant); fall back to `{}`.
                let input: Value = serde_json::from_str(&call.arguments_json).unwrap_or(json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": input,
                }));
            }
            json!({ "role": "assistant", "content": content })
        }
        AskTurn::ToolResults(results) => {
            let content: Vec<Value> = results
                .iter()
                .map(|r| match &r.outcome {
                    Ok(result_json) => json!({
                        "type": "tool_result",
                        "tool_use_id": r.id,
                        "content": result_json,
                    }),
                    Err(message) => json!({
                        "type": "tool_result",
                        "tool_use_id": r.id,
                        "content": message,
                        "is_error": true,
                    }),
                })
                .collect();
            json!({ "role": "user", "content": content })
        }
    }
}

/// Parse an Anthropic Messages response into the neutral [`AskResponse`]: concatenate `text` blocks,
/// collect `tool_use` blocks as [`AskToolCall`]s, and read `stop_reason` for `finished_for_tools`.
fn parse_response(v: &Value) -> AskResponse {
    let mut texts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<AskToolCall> = Vec::new();

    if let Some(blocks) = v["content"].as_array() {
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(t) = block["text"].as_str() {
                        texts.push(t.to_string());
                    }
                }
                Some("tool_use") => {
                    tool_calls.push(AskToolCall {
                        id: block["id"].as_str().unwrap_or_default().to_string(),
                        name: block["name"].as_str().unwrap_or_default().to_string(),
                        // Re-serialize the parsed input object back to a JSON string (the neutral
                        // type carries arguments as a string).
                        arguments_json: block
                            .get("input").map_or_else(|| "{}".to_string(), std::string::ToString::to_string),
                    });
                }
                _ => {}
            }
        }
    }

    let finished_for_tools = v["stop_reason"].as_str() == Some("tool_use");

    AskResponse {
        text: texts.join("\n"),
        tool_calls,
        finished_for_tools,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ask::AskToolResult;

    #[test]
    fn build_request_includes_system_model_and_max_tokens() {
        let history = vec![AskTurn::User("hi".into())];
        let body = build_request(Some("be terse"), &history, &[], "claude-haiku-4-5");
        assert_eq!(body["model"], "claude-haiku-4-5");
        assert_eq!(body["max_tokens"], MAX_TOKENS);
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
        // No tools → no `tools` key.
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn tools_render_with_parsed_input_schema() {
        let tools = vec![AskTool {
            name: "shell".into(),
            description: "run a command".into(),
            parameters_schema: r#"{"type":"object","properties":{"cmd":{"type":"string"}}}"#.into(),
        }];
        let body = build_request(None, &[], &tools, "m");
        let t = &body["tools"][0];
        assert_eq!(t["name"], "shell");
        assert_eq!(t["description"], "run a command");
        // input_schema is the PARSED object, not a string.
        assert_eq!(t["input_schema"]["type"], "object");
        assert_eq!(t["input_schema"]["properties"]["cmd"]["type"], "string");
    }

    #[test]
    fn assistant_turn_preserves_tool_use_id_linkage() {
        let history = vec![
            AskTurn::User("do it".into()),
            AskTurn::Assistant {
                text: "working".into(),
                tool_calls: vec![AskToolCall {
                    id: "call_1".into(),
                    name: "shell".into(),
                    arguments_json: r#"{"cmd":"ls"}"#.into(),
                }],
            },
            AskTurn::ToolResults(vec![AskToolResult {
                id: "call_1".into(),
                name: "shell".into(),
                outcome: Ok("file.txt".into()),
            }]),
        ];
        let body = build_request(None, &history, &[], "m");
        let msgs = body["messages"].as_array().unwrap();
        // Assistant message: a text block + a tool_use block carrying id + parsed input.
        let assistant = &msgs[1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0]["text"], "working");
        assert_eq!(assistant["content"][1]["type"], "tool_use");
        assert_eq!(assistant["content"][1]["id"], "call_1");
        assert_eq!(assistant["content"][1]["input"]["cmd"], "ls");
        // Tool result message links back via tool_use_id = the SAME id.
        let result = &msgs[2];
        assert_eq!(result["role"], "user");
        assert_eq!(result["content"][0]["type"], "tool_result");
        assert_eq!(result["content"][0]["tool_use_id"], "call_1");
        assert_eq!(result["content"][0]["content"], "file.txt");
    }

    #[test]
    fn tool_result_error_sets_is_error() {
        let history = vec![AskTurn::ToolResults(vec![AskToolResult {
            id: "c2".into(),
            name: "shell".into(),
            outcome: Err("authorization denied".into()),
        }])];
        let body = build_request(None, &history, &[], "m");
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["tool_use_id"], "c2");
        assert_eq!(block["content"], "authorization denied");
        assert_eq!(block["is_error"], true);
    }

    #[test]
    fn parse_response_text_only() {
        let v = json!({
            "content": [ { "type": "text", "text": "hello there" } ],
            "stop_reason": "end_turn",
        });
        let resp = parse_response(&v);
        assert_eq!(resp.text, "hello there");
        assert!(resp.tool_calls.is_empty());
        assert!(!resp.finished_for_tools);
        assert!(resp.error.is_none());
    }

    #[test]
    fn parse_response_with_tool_calls() {
        let v = json!({
            "content": [
                { "type": "text", "text": "let me check" },
                { "type": "tool_use", "id": "tu_1", "name": "shell", "input": { "cmd": "pwd" } },
            ],
            "stop_reason": "tool_use",
        });
        let resp = parse_response(&v);
        assert_eq!(resp.text, "let me check");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "tu_1");
        assert_eq!(resp.tool_calls[0].name, "shell");
        // arguments re-serialized to a JSON string.
        let parsed: Value = serde_json::from_str(&resp.tool_calls[0].arguments_json).unwrap();
        assert_eq!(parsed["cmd"], "pwd");
        assert!(resp.finished_for_tools);
    }

    #[test]
    fn parse_response_joins_multiple_text_blocks() {
        let v = json!({
            "content": [
                { "type": "text", "text": "one" },
                { "type": "text", "text": "two" },
            ],
            "stop_reason": "end_turn",
        });
        assert_eq!(parse_response(&v).text, "one\ntwo");
    }

    // --- end-to-end over a hermetic mock server (mirrors wcurl's TcpListener harness) ---

    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// One-shot localhost server replying `200 application/json` with `body` to the first request.
    fn mock_anthropic(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}/v1/messages")
    }

    // The three end-to-end phases live in ONE test: they each mutate the process-global
    // `ANTHROPIC_API_KEY` / `HOME`, so running them as separate `#[tokio::test]`s races (the test
    // harness runs tests in parallel). A single sequential test owns the env for its duration. The
    // pure mapping above (build_request/parse_response) is already covered by the parallel-safe tests.
    #[tokio::test]
    async fn turn_end_to_end_phases() {
        // Phase 1: a text reply round-trips through reqwest against a mock server.
        std::env::set_var("ANTHROPIC_API_KEY", "sk-test-dummy");
        let url = mock_anthropic(r#"{"content":[{"type":"text","text":"pong"}],"stop_reason":"end_turn"}"#);
        let provider = ReqwestAnthropicProvider::with_endpoint(url);
        let resp = provider
            .turn(Some("sys"), &[AskTurn::User("ping".into())], &[], "claude-haiku-4-5")
            .await;
        assert_eq!(resp.text, "pong", "err: {:?}", resp.error);
        assert!(resp.error.is_none());
        assert!(resp.tool_calls.is_empty());

        // Phase 2: a tool_use reply is parsed into an AskToolCall with finished_for_tools.
        let url = mock_anthropic(
            r#"{"content":[{"type":"tool_use","id":"tu_9","name":"shell","input":{"cmd":"ls"}}],"stop_reason":"tool_use"}"#,
        );
        let provider = ReqwestAnthropicProvider::with_endpoint(url);
        let tools = vec![AskTool {
            name: "shell".into(),
            description: "run".into(),
            parameters_schema: r#"{"type":"object"}"#.into(),
        }];
        let resp = provider
            .turn(None, &[AskTurn::User("list".into())], &tools, "m")
            .await;
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "tu_9");
        assert!(resp.finished_for_tools);

        // Phase 3: no key anywhere → an honest "not configured" error, no request sent.
        std::env::remove_var("ANTHROPIC_API_KEY");
        let tmp = std::env::temp_dir().join(format!("clank_ask_nokey_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &tmp);
        let provider = ReqwestAnthropicProvider::with_endpoint("http://127.0.0.1:1/v1/messages");
        let resp = provider.turn(None, &[AskTurn::User("hi".into())], &[], "m").await;
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        assert!(resp.error.is_some());
        assert!(resp.error.unwrap().contains("not configured"));
    }
}
