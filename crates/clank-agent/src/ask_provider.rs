//! The durable multi-provider LLM backend for `ask` on the Golem agent.
//!
//! `clank-core` defines the [`AskProvider`](clank_core::ai::ask::AskProvider) seam but is dual-target
//! and can't depend on the Golem-host-only LLM crates. This module (in the wasm-only agent crate)
//! implements that seam with `golem-ai-llm`'s durable providers: the LLM response is recorded in the
//! Golem oplog and replayed on recovery (no re-bill).
//!
//! **Provider routing.** The `model` string is `provider/model` (e.g. `openai/gpt-4o`,
//! `anthropic/claude-…`). [`turn`](AskProvider::turn) splits the prefix, routes to that provider's
//! durable binding (`DurableAnthropic`/`DurableOpenAI`/…), and passes the *bare* model id to
//! `golem-ai-llm`'s `Config`. All six golem-ai-llm providers share ONE interface
//! (`Event`/`ToolDefinition`/`ToolCall`/`ToolResult`/`Response`), so the neutral↔golem mapping below
//! is written once and reused across every provider — only the per-provider config type and durable
//! type differ, which the [`send_with!`] macro selects.
//!
//! **API keys** are read from the agent environment by each provider's `<Provider>Config::from_env`
//! (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …), supplied through golem.yaml at deploy time. A provider
//! whose key is absent yields an honest "not configured" [`AskResponse`], so `ask` degrades cleanly.
//!
//! The provider is a **single-turn transport**: [`turn`](AskProvider::turn) sends one turn of the
//! conversation (system + history + tool definitions) and returns the model's response. The `Session`
//! owns the multi-turn agentic loop and maps its neutral `AskTurn`/`AskTool`/`AskToolCall`/
//! `AskToolResult` types onto golem-ai-llm's types at this seam, keeping the golem crates out of
//! `clank-core`.

use clank_core::ai::ask::{AskProvider, AskResponse, AskTool, AskToolCall, AskTurn};
use golem_ai_llm::model::{
    Config, ContentPart, Error, Event, FinishReason, Message, Response, ResponseMetadata, Role,
    ToolCall, ToolDefinition, ToolFailure, ToolResult, ToolSuccess,
};
use golem_ai_llm::LlmProvider;
use golem_ai_llm_anthropic::{AnthropicConfig, DurableAnthropic};
use golem_ai_llm_bedrock::{BedrockConfig, DurableBedrock};
use golem_ai_llm_grok::{DurableGrok, GrokConfig};
use golem_ai_llm_ollama::{DurableOllama, OllamaConfig};
use golem_ai_llm_openai::{DurableOpenAI, OpenAiConfig};
use golem_ai_llm_openrouter::{DurableOpenRouter, OpenRouterConfig};

/// Default output-token ceiling for an `ask` reply. Providers that require a `max_tokens` (Anthropic)
/// get this; others treat it as an upper bound. A sensible default for a shell answer.
const MAX_TOKENS: u32 = 4096;

/// The default provider when a model id carries no `provider/` prefix.
const DEFAULT_PROVIDER: &str = "anthropic";

/// Route one send to the durable provider named at the call site: resolve that provider's config from
/// the agent environment (its own `*_API_KEY`), then issue one durable send. On a missing/invalid
/// config, return an honest "not configured" `AskResponse` from the enclosing `turn`. `$events` and
/// `$config` are moved into the taken arm (match arms are exclusive, so only one send runs).
macro_rules! send_with {
    ($cfg:ty, $durable:ty, $provider:expr, $events:expr, $config:expr) => {
        match <$cfg>::from_env() {
            Ok(c) => <$durable>::send(c, $events, $config).await,
            Err(e) => {
                return AskResponse::error(format!(
                    "ask: {} provider not configured: {}\n",
                    $provider, e.message
                ));
            }
        }
    };
}

/// An [`AskProvider`] that routes each turn to the durable golem-ai-llm provider named by the model's
/// `provider/` prefix.
pub(crate) struct DurableLlmProvider;

#[async_trait::async_trait(?Send)]
impl AskProvider for DurableLlmProvider {
    async fn turn(
        &self,
        system: Option<&str>,
        history: &[AskTurn],
        tools: &[AskTool],
        model: &str,
    ) -> AskResponse {
        // `provider/model` → (provider, bare model). No prefix ⇒ the default provider. golem-ai-llm's
        // `Config.model` wants the BARE id (the provider's own model name), so the prefix is dropped here.
        let (provider, bare_model) = match model.split_once('/') {
            Some((p, m)) => (p, m),
            None => (DEFAULT_PROVIDER, model),
        };

        let config = Config {
            model: bare_model.to_string(),
            temperature: None,
            max_tokens: Some(MAX_TOKENS),
            stop_sequences: None,
            tools: if tools.is_empty() {
                None
            } else {
                Some(tools.iter().map(to_tool_def).collect())
            },
            tool_choice: None,
            provider_options: None,
        };

        // Assemble the conversation: an optional system message, then every history turn as its
        // `Event`(s). The provider binding maps `Event::Response`→assistant tool-use blocks and
        // `Event::ToolResults`→tool-result messages, preserving the tool_use/tool_result id linkage
        // the chat APIs require across turns.
        let mut events: Vec<Event> = Vec::new();
        if let Some(system) = system {
            events.push(Event::Message(Message {
                role: Role::System,
                name: None,
                content: vec![ContentPart::Text(system.to_string())],
            }));
        }
        for turn in history {
            turn_to_events(turn, &mut events);
        }

        // Route to the durable provider. Every arm returns `Result<Response, Error>`; the response
        // shape is identical across providers (the shared golem:llm interface), so parsing is done
        // once below. An unknown provider is an honest error (should be caught earlier by
        // `resolve_ask_model`, but this seam stays self-defending).
        let result: Result<Response, Error> = match provider {
            "anthropic" => send_with!(AnthropicConfig, DurableAnthropic, provider, events, config),
            "openai" => send_with!(OpenAiConfig, DurableOpenAI, provider, events, config),
            "ollama" => send_with!(OllamaConfig, DurableOllama, provider, events, config),
            "grok" => send_with!(GrokConfig, DurableGrok, provider, events, config),
            "openrouter" => {
                send_with!(OpenRouterConfig, DurableOpenRouter, provider, events, config)
            }
            "bedrock" => send_with!(BedrockConfig, DurableBedrock, provider, events, config),
            other => {
                return AskResponse::error(format!("ask: unknown model provider '{other}'\n"));
            }
        };

        match result {
            Ok(response) => {
                let text: String = response
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Text(t) => Some(t.as_str()),
                        ContentPart::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let finished_for_tools =
                    matches!(response.metadata.finish_reason, Some(FinishReason::ToolCalls));
                AskResponse {
                    text,
                    tool_calls: response.tool_calls.iter().map(from_tool_call).collect(),
                    finished_for_tools,
                    error: None,
                }
            }
            Err(e) => AskResponse::error(format!("ask: model call failed: {}\n", e.message)),
        }
    }
}

/// Convert a neutral [`AskTool`] to the binding's [`ToolDefinition`].
fn to_tool_def(tool: &AskTool) -> ToolDefinition {
    ToolDefinition {
        name: tool.name.clone(),
        description: Some(tool.description.clone()),
        parameters_schema: tool.parameters_schema.clone(),
    }
}

/// Convert a binding [`ToolCall`] to the neutral [`AskToolCall`].
fn from_tool_call(call: &ToolCall) -> AskToolCall {
    AskToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments_json: call.arguments_json.clone(),
    }
}

/// Convert one neutral history turn to the binding `Event`(s) it produces, appending to `events`.
fn turn_to_events(turn: &AskTurn, events: &mut Vec<Event>) {
    match turn {
        AskTurn::User(text) => events.push(Event::Message(Message {
            role: Role::User,
            name: None,
            content: vec![ContentPart::Text(text.clone())],
        })),
        AskTurn::Assistant { text, tool_calls } => events.push(Event::Response(Response {
            id: String::new(),
            content: if text.is_empty() {
                Vec::new()
            } else {
                vec![ContentPart::Text(text.clone())]
            },
            tool_calls: tool_calls
                .iter()
                .map(|c| ToolCall {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    arguments_json: c.arguments_json.clone(),
                })
                .collect(),
            metadata: ResponseMetadata {
                finish_reason: None,
                usage: None,
                provider_id: None,
                timestamp: None,
                provider_metadata_json: None,
            },
        })),
        AskTurn::ToolResults(results) => {
            let converted = results
                .iter()
                .map(|r| match &r.outcome {
                    Ok(result_json) => ToolResult::Success(ToolSuccess {
                        id: r.id.clone(),
                        name: r.name.clone(),
                        result_json: result_json.clone(),
                        execution_time_ms: None,
                    }),
                    Err(message) => ToolResult::Error(ToolFailure {
                        id: r.id.clone(),
                        name: r.name.clone(),
                        error_message: message.clone(),
                        error_code: None,
                    }),
                })
                .collect();
            events.push(Event::ToolResults(converted));
        }
    }
}
