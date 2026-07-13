//! The durable Anthropic provider backing `ask` on the Golem agent.
//!
//! `clank-shell` defines the [`AskProvider`](clank_shell::ai::ask::AskProvider) seam but is dual-target
//! and can't depend on the Golem-host-only LLM crates. This module (in the wasm-only agent crate)
//! implements that seam with `golem-ai-llm`'s `DurableAnthropic`: the LLM response is recorded in the
//! Golem oplog and replayed on recovery (no re-bill). The API key is read from `ANTHROPIC_API_KEY` via
//! `AnthropicConfig::from_env` (supplied to the agent env through golem.yaml).
//!
//! The provider is a **single-turn transport**: [`turn`](AskProvider::turn) sends one turn of the
//! conversation (system + history + tool definitions) and returns the model's response. The `Session`
//! owns the multi-turn agentic loop — it maps the neutral `AskTurn`/`AskTool`/`AskToolCall`/
//! `AskToolResult` types onto `golem-ai-llm`'s `Event`/`ToolDefinition`/`ToolCall`/`ToolResult` at this
//! seam, keeping the golem crates out of `clank-shell`.

use clank_shell::ai::ask::{
    AskProvider, AskResponse, AskTool, AskToolCall, AskTurn,
};
use golem_ai_llm::model::{
    Config, ContentPart, Event, FinishReason, Message, Response, ResponseMetadata, Role, ToolCall,
    ToolDefinition, ToolFailure, ToolResult, ToolSuccess,
};
use golem_ai_llm::LlmProvider;
use golem_ai_llm_anthropic::{AnthropicConfig, DurableAnthropic};

/// Default output-token ceiling for an `ask` reply. Anthropic requires `max_tokens`; this is a
/// sensible default for a shell answer and can be made configurable in a later increment.
const MAX_TOKENS: u32 = 4096;

/// An [`AskProvider`] backed by the durable Anthropic provider.
pub struct DurableAnthropicProvider;

#[async_trait::async_trait(?Send)]
impl AskProvider for DurableAnthropicProvider {
    async fn turn(
        &self,
        system: Option<&str>,
        history: &[AskTurn],
        tools: &[AskTool],
        model: &str,
    ) -> AskResponse {
        // The API key comes from ANTHROPIC_API_KEY in the agent environment.
        let provider_config = match AnthropicConfig::from_env() {
            Ok(c) => c,
            Err(e) => {
                return AskResponse::error(format!(
                    "ask: model provider not configured: {}\n",
                    e.message
                ));
            }
        };

        let config = Config {
            model: model.to_string(),
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

        // Assemble the conversation: an optional system message, then every history turn converted to
        // its `Event`. The anthropic binding maps `Event::Response`→assistant `tool_use` blocks and
        // `Event::ToolResults`→`tool_result` user messages, preserving the tool_use/tool_result id
        // linkage the API requires across turns.
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
                    // Always valid JSON — the anthropic binding parses this into the tool_use block.
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
