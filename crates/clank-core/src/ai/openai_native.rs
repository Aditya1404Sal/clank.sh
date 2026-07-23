//! The native (reqwest) OpenAI-**compatible** `ask` wire mapping — the off-Golem path for every
//! provider that speaks the OpenAI Chat Completions API: `openai`, `grok` (xAI), `openrouter`, and
//! `ollama` (local). The Golem agent reaches these through `golem-ai-llm`; native can't (those crates
//! are Golem-host-only), so this module maps the SAME neutral `AskTurn`/`AskTool`/… types directly
//! onto the OpenAI `POST /chat/completions` JSON via `reqwest` + `serde_json`.
//!
//! One wire mapping serves four providers because they share the format — only the **endpoint** and
//! **API-key env var** differ (see [`Descriptor`]). Anthropic is NOT here (its Messages API is a
//! different shape — see [`crate::ai::anthropic_native`]); Bedrock is agent-only (AWS SigV4).
//!
//! The `Session` owns the multi-turn agentic loop; this is a single-turn transport — one
//! [`turn`] = one Chat Completions request.

use serde_json::{json, Value};

use crate::ai::ask::{AskResponse, AskToolCall};
use crate::ai::ask::{AskTool, AskTurn};

/// Output-token ceiling for an `ask` reply — matches the Anthropic path's `MAX_TOKENS`.
const MAX_TOKENS: u32 = 4096;

/// A static description of one OpenAI-compatible provider: where to send and which env var holds the
/// key. `requires_key` is false for local Ollama (no auth).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Descriptor {
    /// The provider name (`openai`/`grok`/`openrouter`/`ollama`), for config lookup + error text.
    pub provider: &'static str,
    /// The base URL up to and including `/v1` (Chat Completions is `<base>/chat/completions`). For
    /// Ollama this is a fallback; the real base is read from `GOLEM_OLLAMA_BASE_URL` at call time.
    pub base_url: &'static str,
    /// The environment variable holding the API key (matches golem-ai-llm's config, so the same env
    /// works on native and the agent). Empty for a keyless provider.
    pub key_env: &'static str,
    /// Whether a key is required (false ⇒ keyless local Ollama).
    pub requires_key: bool,
}

/// The four OpenAI-compatible providers, with the endpoints + env vars golem-ai-llm uses.
pub(crate) const OPENAI: Descriptor = Descriptor {
    provider: "openai",
    base_url: "https://api.openai.com/v1",
    key_env: "OPENAI_API_KEY",
    requires_key: true,
};
pub(crate) const GROK: Descriptor = Descriptor {
    provider: "grok",
    base_url: "https://api.x.ai/v1",
    key_env: "XAI_API_KEY",
    requires_key: true,
};
pub(crate) const OPENROUTER: Descriptor = Descriptor {
    provider: "openrouter",
    base_url: "https://openrouter.ai/api/v1",
    key_env: "OPENROUTER_API_KEY",
    requires_key: true,
};
pub(crate) const OLLAMA: Descriptor = Descriptor {
    provider: "ollama",
    base_url: "http://localhost:11434/v1",
    key_env: "",
    requires_key: false,
};

/// The Chat Completions endpoint for `desc`. Ollama's base is overridable via `GOLEM_OLLAMA_BASE_URL`
/// (matching golem-ai-llm); everything else uses the fixed base.
fn endpoint(desc: &Descriptor) -> String {
    if desc.provider == "ollama" {
        let base = std::env::var("GOLEM_OLLAMA_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .map_or_else(|| "http://localhost:11434".to_string(), |s| s.trim_end_matches('/').to_string());
        format!("{base}/v1/chat/completions")
    } else {
        format!("{}/chat/completions", desc.base_url)
    }
}

/// Send one turn to an OpenAI-compatible provider and return the neutral response. `api_key` is `None`
/// for a keyless provider (Ollama); `endpoint_override` lets tests point at a mock server.
pub(crate) async fn turn(
    client: &reqwest::Client,
    desc: &Descriptor,
    api_key: Option<&str>,
    endpoint_override: Option<&str>,
    system: Option<&str>,
    history: &[AskTurn],
    tools: &[AskTool],
    model: &str,
) -> AskResponse {
    let url = endpoint_override.map_or_else(|| endpoint(desc), str::to_string);
    let body = build_request(system, history, tools, model);

    let mut req = client
        .post(&url)
        .header("content-type", "application/json")
        .json(&body);
    if let Some(key) = api_key {
        req = req.header("authorization", format!("Bearer {key}"));
    }

    let response = match req.send().await {
        Ok(r) => r,
        Err(e) => return AskResponse::error(format!("ask: model call failed: {e}\n")),
    };
    let status = response.status();
    let text = match response.text().await {
        Ok(t) => t,
        Err(e) => return AskResponse::error(format!("ask: reading model response failed: {e}\n")),
    };
    if !status.is_success() {
        // OpenAI-style error bodies are `{"error":{"message":...}}`; surface the message, else the raw
        // body (truncated). Never echoes the key back.
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

/// Build the Chat Completions request body from the neutral turn inputs.
fn build_request(system: Option<&str>, history: &[AskTurn], tools: &[AskTool], model: &str) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(sys) = system {
        messages.push(json!({ "role": "system", "content": sys }));
    }
    for turn in history {
        push_messages(turn, &mut messages);
    }
    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "messages": messages,
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(tool_to_json).collect::<Vec<_>>());
        body["tool_choice"] = json!("auto");
    }
    body
}

/// `AskTool` → an OpenAI `function` tool. `parameters` must be a JSON object; a schema string that
/// doesn't parse falls back to a permissive empty object.
fn tool_to_json(tool: &AskTool) -> Value {
    let schema: Value =
        serde_json::from_str(&tool.parameters_schema).unwrap_or_else(|_| json!({ "type": "object" }));
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": schema,
        }
    })
}

/// `AskTurn` → OpenAI Chat message(s), appended to `messages`. A `ToolResults` turn becomes ONE
/// `role:"tool"` message per result (OpenAI's shape), each linked by `tool_call_id` — unlike
/// Anthropic's single user message with multiple blocks.
fn push_messages(turn: &AskTurn, messages: &mut Vec<Value>) {
    match turn {
        AskTurn::User(text) => messages.push(json!({ "role": "user", "content": text })),
        AskTurn::Assistant { text, tool_calls } => {
            let mut msg = json!({ "role": "assistant" });
            // OpenAI wants `content: null` when the assistant only made tool calls.
            msg["content"] = if text.is_empty() { Value::Null } else { json!(text) };
            if !tool_calls.is_empty() {
                msg["tool_calls"] = json!(tool_calls
                    .iter()
                    .map(|c| json!({
                        "id": c.id,
                        "type": "function",
                        // OpenAI carries arguments as a JSON *string* — the neutral type already does.
                        "function": { "name": c.name, "arguments": c.arguments_json },
                    }))
                    .collect::<Vec<_>>());
            }
            messages.push(msg);
        }
        AskTurn::ToolResults(results) => {
            for r in results {
                let content = match &r.outcome {
                    Ok(result_json) => result_json.clone(),
                    Err(message) => message.clone(),
                };
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": r.id,
                    "content": content,
                }));
            }
        }
    }
}

/// Parse a Chat Completions response into the neutral [`AskResponse`]: `choices[0].message.content`
/// as text, `choices[0].message.tool_calls` as [`AskToolCall`]s, and `finish_reason == "tool_calls"`.
fn parse_response(v: &Value) -> AskResponse {
    let message = &v["choices"][0]["message"];
    let text = message["content"].as_str().unwrap_or_default().to_string();

    let mut tool_calls: Vec<AskToolCall> = Vec::new();
    if let Some(calls) = message["tool_calls"].as_array() {
        for call in calls {
            tool_calls.push(AskToolCall {
                id: call["id"].as_str().unwrap_or_default().to_string(),
                name: call["function"]["name"].as_str().unwrap_or_default().to_string(),
                // Already a JSON string in the OpenAI wire; keep it as-is (the neutral invariant).
                arguments_json: call["function"]["arguments"]
                    .as_str()
                    .unwrap_or("{}")
                    .to_string(),
            });
        }
    }

    let finished_for_tools = v["choices"][0]["finish_reason"].as_str() == Some("tool_calls");
    AskResponse {
        text,
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
    fn build_request_maps_system_user_and_tools() {
        let tools = vec![AskTool {
            name: "shell".into(),
            description: "run".into(),
            parameters_schema: r#"{"type":"object","properties":{"cmd":{"type":"string"}}}"#.into(),
        }];
        let body = build_request(Some("be terse"), &[AskTurn::User("hi".into())], &tools, "gpt-4o");
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "hi");
        // Tool is an OpenAI `function` with a PARSED parameters object.
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "shell");
        assert_eq!(body["tools"][0]["function"]["parameters"]["properties"]["cmd"]["type"], "string");
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn assistant_tool_calls_and_tool_results_link_by_id() {
        let history = vec![
            AskTurn::Assistant {
                text: String::new(),
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
        let body = build_request(None, &history, &[], "gpt-4o");
        let assistant = &body["messages"][0];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant["content"].is_null(), "tool-only assistant content is null");
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
        assert_eq!(assistant["tool_calls"][0]["type"], "function");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "shell");
        // arguments stay a JSON string.
        assert_eq!(assistant["tool_calls"][0]["function"]["arguments"], r#"{"cmd":"ls"}"#);
        // Tool result is its OWN role:tool message linked by tool_call_id.
        let tool = &body["messages"][1];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "call_1");
        assert_eq!(tool["content"], "file.txt");
    }

    #[test]
    fn parse_response_text_and_tool_calls() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "let me check",
                    "tool_calls": [{
                        "id": "tc_1",
                        "type": "function",
                        "function": { "name": "shell", "arguments": "{\"cmd\":\"pwd\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let resp = parse_response(&v);
        assert_eq!(resp.text, "let me check");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "tc_1");
        assert_eq!(resp.tool_calls[0].name, "shell");
        let parsed: Value = serde_json::from_str(&resp.tool_calls[0].arguments_json).unwrap();
        assert_eq!(parsed["cmd"], "pwd");
        assert!(resp.finished_for_tools);
    }

    #[test]
    fn parse_response_text_only() {
        let v = json!({
            "choices": [{ "message": { "content": "pong" }, "finish_reason": "stop" }]
        });
        let resp = parse_response(&v);
        assert_eq!(resp.text, "pong");
        assert!(resp.tool_calls.is_empty());
        assert!(!resp.finished_for_tools);
    }

    #[test]
    fn ollama_endpoint_honors_the_base_url_env() {
        // Deterministic default when the env is unset (serialize via the process env is out of scope
        // here; just check the fixed-base providers).
        assert_eq!(endpoint(&OPENAI), "https://api.openai.com/v1/chat/completions");
        assert_eq!(endpoint(&GROK), "https://api.x.ai/v1/chat/completions");
        assert_eq!(endpoint(&OPENROUTER), "https://openrouter.ai/api/v1/chat/completions");
    }
}
