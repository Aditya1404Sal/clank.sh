//! The durable Anthropic provider backing `ask` on the Golem agent.
//!
//! `clank-shell` defines the [`AskProvider`](clank_shell::askcmd::AskProvider) seam but is dual-target
//! and can't depend on the Golem-host-only LLM crates. This module (in the wasm-only agent crate)
//! implements that seam with `golem-ai-llm`'s `DurableAnthropic`: the LLM response is recorded in the
//! Golem oplog and replayed on recovery (no re-bill). The API key is read from `ANTHROPIC_API_KEY` via
//! `AnthropicConfig::from_env` (supplied to the agent env through golem.yaml).

use clank_shell::askcmd::{AskOutcome, AskProvider, AskRequest};
use golem_ai_llm::model::{Config, ContentPart, Event, Message, Role};
use golem_ai_llm::LlmProvider;
use golem_ai_llm_anthropic::{AnthropicConfig, DurableAnthropic};

/// Default output-token ceiling for a Core `ask` reply. Anthropic requires `max_tokens`; this is a
/// sensible default for a shell answer and can be made configurable in a later increment.
const MAX_TOKENS: u32 = 4096;

/// An [`AskProvider`] backed by the durable Anthropic provider.
pub struct DurableAnthropicProvider;

#[async_trait::async_trait(?Send)]
impl AskProvider for DurableAnthropicProvider {
    async fn complete(&self, request: AskRequest) -> AskOutcome {
        // The API key comes from ANTHROPIC_API_KEY in the agent environment.
        let provider_config = match AnthropicConfig::from_env() {
            Ok(c) => c,
            Err(e) => {
                return AskOutcome::remote_error(format!(
                    "ask: model provider not configured: {}\n",
                    e.message
                ));
            }
        };

        let config = Config {
            model: request.model,
            temperature: None,
            max_tokens: Some(MAX_TOKENS),
            stop_sequences: None,
            tools: None,
            tool_choice: None,
            provider_options: None,
        };

        // Assemble the conversation: an optional system message, then a single user message carrying
        // the transcript-as-context followed by the prompt (and, later, piped stdin).
        let mut events: Vec<Event> = Vec::new();
        if let Some(system) = request.system {
            events.push(Event::Message(Message {
                role: Role::System,
                name: None,
                content: vec![ContentPart::Text(system)],
            }));
        }
        events.push(Event::Message(Message {
            role: Role::User,
            name: None,
            content: vec![ContentPart::Text(user_content(&request.transcript, &request.prompt))],
        }));

        match DurableAnthropic::send(provider_config, events, config).await {
            Ok(response) => {
                let text: String = response
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                AskOutcome::reply(text.into_bytes())
            }
            Err(e) => AskOutcome::remote_error(format!("ask: model call failed: {}\n", e.message)),
        }
    }
}

/// Build the single user message body: the transcript window (if any) as context, then the prompt.
fn user_content(transcript: &str, prompt: &str) -> String {
    if transcript.is_empty() {
        prompt.to_string()
    } else {
        format!("# Shell transcript (context)\n{transcript}\n# Question\n{prompt}")
    }
}
