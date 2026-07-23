//! The AI layer: the `ask` command + LLM seam ([`ask`]), the `~/.config/ask/ask.toml` model/provider
//! config ([`config`]), and the `model` command ([`model`]).
//!
//! The concrete LLM provider is injected into the `Session`: the durable golem-ai-llm provider lives
//! in `clank-agent` (wasm), and the native reqwest→Anthropic provider is [`anthropic_native`]. This
//! crate owns the target-agnostic [`ask::AskProvider`] seam.

pub mod ask;
// The native (reqwest) `ask` providers. wasm uses the injected durable golem-ai-llm dispatcher from
// `clank-agent`; these fill the same seam off-Golem. cfg-gated so `reqwest` never reaches wasm.
// `anthropic_native` maps Anthropic's Messages API; `openai_native` maps the OpenAI Chat Completions
// API (shared by openai/grok/openrouter/ollama); `llm_native` is the provider-routing dispatcher.
#[cfg(not(target_arch = "wasm32"))]
pub mod anthropic_native;
#[cfg(not(target_arch = "wasm32"))]
pub mod openai_native;
#[cfg(not(target_arch = "wasm32"))]
pub mod llm_native;
pub mod config;
pub(crate) mod model;
