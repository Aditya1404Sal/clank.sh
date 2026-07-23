//! The native multi-provider `ask` dispatcher — the off-Golem mirror of `clank-agent`'s durable
//! golem-ai-llm dispatcher.
//!
//! `ask`'s model id is `provider/model` (e.g. `openai/gpt-4o`, `anthropic/claude-…`). This provider
//! splits the prefix and routes to the right native transport:
//!
//! - `anthropic` → [`crate::ai::anthropic_native`] (Anthropic Messages API).
//! - `openai` / `grok` / `openrouter` / `ollama` → [`crate::ai::openai_native`] (one OpenAI Chat
//!   Completions mapping, differing only in endpoint + key env var).
//! - `bedrock` → an honest error: AWS SigV4 signing isn't available in native `ask`; use the Golem
//!   agent (which reaches Bedrock through golem-ai-llm).
//!
//! Keys resolve per provider from `~/.config/ask/ask.toml` `[providers.<name>].key`, else the
//! provider's env var (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `XAI_API_KEY`, `OPENROUTER_API_KEY`);
//! Ollama is keyless. The bare-id default provider is anthropic.

use crate::ai::anthropic_native::ReqwestAnthropicProvider;
use crate::ai::ask::{AskProvider, AskResponse, AskTool, AskTurn};
use crate::ai::openai_native::{self, Descriptor, GROK, OLLAMA, OPENAI, OPENROUTER};

/// A native [`AskProvider`] that routes each turn to the transport for the model's `provider/` prefix.
pub struct NativeLlmProvider {
    /// The Anthropic (Messages API) sub-provider — its own reqwest client + endpoint.
    anthropic: ReqwestAnthropicProvider,
    /// A shared reqwest client for the OpenAI-compatible providers.
    client: reqwest::Client,
}

impl NativeLlmProvider {
    /// A new dispatcher with fresh reqwest clients.
    #[must_use]
    pub fn new() -> Self {
        Self {
            anthropic: ReqwestAnthropicProvider::new(),
            client: reqwest::Client::builder()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    /// Route one OpenAI-compatible provider: resolve its key (unless keyless), then send.
    async fn openai_family(
        &self,
        desc: &Descriptor,
        system: Option<&str>,
        history: &[AskTurn],
        tools: &[AskTool],
        model: &str,
    ) -> AskResponse {
        let key = resolve_provider_key(desc.provider, desc.key_env);
        if desc.requires_key && key.is_none() {
            return AskResponse::error(format!(
                "ask: {} provider not configured: set {} in the environment or run \
                 `model add {} --key <key>` (stores it in ~/.config/ask/ask.toml)\n",
                desc.provider, desc.key_env, desc.provider
            ));
        }
        openai_native::turn(&self.client, desc, key.as_deref(), None, system, history, tools, model)
            .await
    }
}

impl Default for NativeLlmProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait(?Send)]
impl AskProvider for NativeLlmProvider {
    async fn turn(
        &self,
        system: Option<&str>,
        history: &[AskTurn],
        tools: &[AskTool],
        model: &str,
    ) -> AskResponse {
        // `provider/model` → (provider, bare). No prefix ⇒ anthropic. The transports want the BARE id.
        let (provider, bare) = match model.split_once('/') {
            Some((p, m)) => (p, m),
            None => ("anthropic", model),
        };
        match provider {
            "anthropic" => self.anthropic.turn(system, history, tools, bare).await,
            "openai" => self.openai_family(&OPENAI, system, history, tools, bare).await,
            "grok" => self.openai_family(&GROK, system, history, tools, bare).await,
            "openrouter" => self.openai_family(&OPENROUTER, system, history, tools, bare).await,
            "ollama" => self.openai_family(&OLLAMA, system, history, tools, bare).await,
            "bedrock" => AskResponse::error(
                "ask: provider 'bedrock' requires the Golem agent (AWS SigV4 signing isn't \
                 available in native ask); use the agent, which reaches Bedrock via golem-ai-llm\n",
            ),
            other => AskResponse::error(format!(
                "ask: unknown model provider '{other}'\n"
            )),
        }
    }
}

/// Resolve a provider's API key: `~/.config/ask/ask.toml` `[providers.<provider>].key` under `$HOME`,
/// else the provider's `key_env` environment variable. `None` when neither is set (or `key_env` is
/// empty, i.e. a keyless provider).
fn resolve_provider_key(provider: &str, key_env: &str) -> Option<String> {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(key) = crate::ai::config::provider_key(&home, provider) {
            if !key.is_empty() {
                return Some(key);
            }
        }
    }
    if key_env.is_empty() {
        return None;
    }
    std::env::var(key_env).ok().filter(|k| !k.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Route-only tests: bedrock + unknown provider return before any HTTP, so no network/env is
    // touched. (The per-provider wire mapping is covered in `openai_native`/`anthropic_native`.)
    #[tokio::test]
    async fn bedrock_is_an_honest_native_error() {
        let p = NativeLlmProvider::new();
        let resp = p
            .turn(None, &[AskTurn::User("hi".into())], &[], "bedrock/anthropic.claude-3")
            .await;
        let err = resp.error.expect("bedrock must error on native");
        assert!(err.contains("bedrock") && err.contains("agent"), "got: {err}");
    }

    #[tokio::test]
    async fn unknown_provider_is_rejected() {
        let p = NativeLlmProvider::new();
        let resp = p
            .turn(None, &[AskTurn::User("hi".into())], &[], "frobnicate/x")
            .await;
        assert!(resp.error.unwrap().contains("unknown model provider"));
    }
}
