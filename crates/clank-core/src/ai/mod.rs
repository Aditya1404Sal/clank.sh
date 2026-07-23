//! The AI layer: the `ask` command + LLM seam ([`ask`]), the `~/.config/ask/ask.toml` model/provider
//! config ([`config`]), and the `model` command ([`model`]).
//!
//! The concrete LLM provider is injected into the `Session`: the durable provider lives in
//! `clank-agent` (wasm) and calls Anthropic over `wstd` (the Golem runtime records that HTTP call in
//! the oplog, so it replays on recovery without re-billing); the native reqwest→Anthropic provider is
//! [`anthropic_native`]. Both share the wire format in [`anthropic_wire`]. This crate owns the
//! target-agnostic [`ask::AskProvider`] seam.

pub mod ask;
// The target-agnostic Anthropic Messages API wire format (request build + response parse). Both
// providers below and the durable `wstd` one in `clank-agent` share it, so the wire shape is defined
// once and can't drift between native and agent.
pub mod anthropic_wire;
// The native (reqwest) Anthropic `ask` provider. wasm uses the injected durable provider from
// `clank-agent`; this fills the same seam off-Golem. cfg-gated so `reqwest` never reaches wasm.
#[cfg(not(target_arch = "wasm32"))]
pub mod anthropic_native;
pub mod config;
pub(crate) mod model;
