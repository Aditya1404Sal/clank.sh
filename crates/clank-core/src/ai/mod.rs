//! The AI layer: the `ask` command + LLM seam ([`ask`]), the text sent to the model ([`prompts`]),
//! the `~/.config/ask/ask.toml` model/provider config ([`config`]), and the `model` command
//! ([`model`]).
//!
//! The concrete LLM provider is injected into the `Session`: the durable provider lives in
//! `clank-embed` (wasm) and calls Anthropic over WASI-HTTP (the Golem runtime records that HTTP call in
//! the oplog, so it replays on recovery without re-billing); the native reqwest→Anthropic provider
//! lives in the separate `clank-native` crate. Both share the wire format in [`anthropic_wire`]. This
//! crate owns the target-agnostic [`ask::AskProvider`] seam.

pub mod error;

pub use error::Error;

pub mod ask;
// The target-agnostic Anthropic Messages API wire format (request build + response parse). Both the
// native reqwest provider in `clank-native` and the durable WASI-HTTP one in `clank-embed` share it,
// so the wire shape is defined once and can't drift between native and agent.
pub mod anthropic_wire;
// Every word clank says to a model: the system-prompt and tool-schema text `ask` assembles from (see
// the module's own docs). Kept apart from the wire format above — this is prose content, that is
// transport.
pub mod prompts;
// The native (reqwest) `ask` providers now live in the separate `clank-native` crate (mirroring how
// the durable provider lives in `clank-embed`), not here. wasm uses the injected durable provider from
// `clank-embed` (`DurableAnthropicProvider`, built on `anthropic_wire` over `wasi-fetch`);
// `clank-native`'s providers fill the same seam off-Golem, so `reqwest` never reaches wasm.
// `clank-native::anthropic` maps Anthropic's Messages API (via `anthropic_wire`); `clank-native::openai`
// maps the OpenAI Chat Completions API (shared by openai/grok/openrouter/ollama); `clank-native::llm`
// is the native provider-routing dispatcher. All three are plain `reqwest`, so none of them touch
// `golem-ai-llm` — that crate hard-pins `golem-rust = "=2.1.0"`, which cannot coexist with this
// branch's path dependency (see clank-agent/Cargo.toml), so the *agent's* multi-provider routing is
// deferred: the agent keeps the single-provider `DurableAnthropicProvider`, and only native `ask` is
// multi-provider today.
pub mod config;
pub(crate) mod model;
