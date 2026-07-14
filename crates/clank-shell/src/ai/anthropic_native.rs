//! The native (reqwest) Anthropic `ask` provider — the off-Golem mirror of `clank-agent`'s durable
//! `DurableAnthropicProvider`.
//!
//! `clank-shell` owns the target-agnostic [`AskProvider`](crate::ai::ask::AskProvider) seam and the
//! neutral `AskTurn`/`AskTool`/`AskToolCall`/`AskToolResult`/`AskResponse` types. The request/response
//! wire mapping onto Anthropic's `POST /v1/messages` lives in [`anthropic_wire`](super::anthropic_wire)
//! (shared with the durable agent provider); this module is the thin `reqwest` transport around it, so
//! `ask` works natively without the Golem crates. The `Session` owns the multi-turn agentic loop; this
//! provider is a single-turn transport (one [`turn`](AskProvider::turn) = one Anthropic request).
//!
//! **API key** (README §444): read from `~/.config/ask/ask.toml` `[providers.anthropic].key` first,
//! then env `ANTHROPIC_API_KEY` (the same var the agent uses). Absent ⇒ an honest "not configured"
//! [`AskResponse::error`], so `ask` degrades cleanly rather than sending an unauthenticated request.

use serde_json::Value;

use crate::ai::anthropic_wire::{
    build_request, parse_error, parse_response, ANTHROPIC_VERSION, MESSAGES_URL,
};
use crate::ai::ask::{AskProvider, AskResponse, AskTool, AskTurn};

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
            return AskResponse::error(format!(
                "ask: model call failed: HTTP {} — {}\n",
                status.as_u16(),
                parse_error(&text)
            ));
        }

        match serde_json::from_str::<Value>(&text) {
            Ok(v) => parse_response(&v),
            Err(e) => AskResponse::error(format!("ask: malformed model response: {e}\n")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ask::AskTool;

    // The pure request-build / response-parse mapping is covered in `anthropic_wire`'s tests. Here we
    // exercise only the reqwest transport end-to-end over a hermetic mock server (mirrors wcurl's
    // TcpListener harness).

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
    // harness runs tests in parallel). A single sequential test owns the env for its duration.
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
