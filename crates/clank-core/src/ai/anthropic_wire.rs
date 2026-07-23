//! The target-agnostic Anthropic Messages API wire format for `ask`.
//!
//! Both `ask` providers — the native reqwest one ([`anthropic_native`](super::anthropic_native)) and
//! the durable `wstd` one in `clank-agent` — map the neutral `AskTurn`/`AskTool`/`AskToolCall`/
//! `AskToolResult`/`AskResponse` types onto Anthropic's `POST /v1/messages` JSON. That mapping is pure
//! `serde_json` (no HTTP client), so it lives here, defined once, and each provider is a thin transport
//! wrapper around [`build_request`] + [`parse_response`] / [`parse_error`]. Keeping the wire format in
//! one ungated module means the request/response shape can't drift between native and agent.

use serde_json::{json, Value};

use crate::ai::ask::{AskResponse, AskToolCall, AskTool, AskTurn};

/// The Anthropic Messages API endpoint.
pub const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
/// The Anthropic API version header (stable public value).
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Output-token ceiling for an `ask` reply. Anthropic requires `max_tokens`; this is a sensible default
/// for a shell answer and can be made configurable in a later increment.
pub const MAX_TOKENS: u32 = 4096;

/// Build the Anthropic `/v1/messages` request body from the neutral turn inputs: the system prompt as a
/// top-level field, `max_tokens`, the tool definitions, and the message list from the history.
pub fn build_request(
    system: Option<&str>,
    history: &[AskTurn],
    tools: &[AskTool],
    model: &str,
) -> Value {
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

/// `AskTool` → an Anthropic tool definition. `input_schema` must be a JSON object; if the stored schema
/// string doesn't parse, fall back to a permissive empty-object schema (the model can still call the
/// tool; arguments are validated downstream).
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

/// Parse a successful Anthropic Messages response body into the neutral [`AskResponse`]: concatenate
/// `text` blocks, collect `tool_use` blocks as [`AskToolCall`]s, and read `stop_reason` for
/// `finished_for_tools`.
pub fn parse_response(v: &Value) -> AskResponse {
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
                            .get("input")
                            .map(|i| i.to_string())
                            .unwrap_or_else(|| "{}".to_string()),
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

/// Extract a human-readable detail from a non-2xx Anthropic response body. Anthropic error bodies are
/// JSON `{"error":{"message":...}}`; surface that message if present, else the raw body truncated. The
/// api key is never echoed back by the API, so this can't leak it.
pub fn parse_error(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.chars().take(300).collect())
}

/// Serialize a [`build_request`] body to bytes. A byte-level entry point so a transport that doesn't
/// itself depend on `serde_json` (the wasm agent) can build the request through this module alone.
pub fn serialize_request(body: &Value) -> Vec<u8> {
    serde_json::to_vec(body).unwrap_or_default()
}

/// Parse a successful Anthropic response body (as text) into the neutral [`AskResponse`]. Wraps
/// [`parse_response`] with the JSON decode so a `serde_json`-free transport can parse through this
/// module; a malformed body becomes an [`AskResponse::error`].
pub fn parse_response_body(text: &str) -> AskResponse {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => parse_response(&v),
        Err(e) => AskResponse::error(format!("ask: malformed model response: {e}\n")),
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

    #[test]
    fn parse_error_prefers_structured_message() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens is required"}}"#;
        assert_eq!(parse_error(body), "max_tokens is required");
    }

    #[test]
    fn parse_error_falls_back_to_truncated_body() {
        let body = "gateway exploded, not json";
        assert_eq!(parse_error(body), "gateway exploded, not json");
    }
}
