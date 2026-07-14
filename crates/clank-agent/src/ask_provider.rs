//! The `ask` LLM provider backing `ask` on the Golem agent.
//!
//! **SPIKE (spike/dev-golem-sdk): STUBBED.** The real provider used `golem-ai-llm` /
//! `golem-ai-llm-anthropic` (crates.io), which hard-pin `golem-rust = "=2.1.0"` — incompatible with
//! the dev SDK this spike builds against. So the golem-ai-llm dependency is dropped and this provider
//! is stubbed to an honest "not configured" error. `ask` therefore reports unavailable on the spike
//! branch; everything non-LLM (the shell, FS, prompt-user, context) is unaffected. Real `ask` on the
//! dev SDK is a follow-up (needs a dev-SDK-compatible golem-ai-llm).

use clank_shell::ai::ask::{AskProvider, AskResponse, AskTool, AskTurn};

/// An [`AskProvider`] stub for the dev-SDK spike (see module docs).
pub struct DurableAnthropicProvider;

#[async_trait::async_trait(?Send)]
impl AskProvider for DurableAnthropicProvider {
    async fn turn(
        &self,
        _system: Option<&str>,
        _history: &[AskTurn],
        _tools: &[AskTool],
        _model: &str,
    ) -> AskResponse {
        AskResponse::error(
            "ask: not available on this spike build (clank on the dev golem SDK); the durable \
             Anthropic provider awaits a dev-SDK-compatible golem-ai-llm\n",
        )
    }
}
