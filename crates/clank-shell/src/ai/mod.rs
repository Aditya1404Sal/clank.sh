//! The AI layer: the `ask` command + LLM seam ([`ask`]), the `~/.config/ask/ask.toml` model/provider
//! config ([`config`]), and the `model` command ([`model`]).
//!
//! The concrete durable LLM provider lives in `clank-agent` and is injected into the `Session` — this
//! crate owns only the [`ask::AskProvider`] seam (dual-target; native has no provider).

pub mod ask;
pub mod config;
pub(crate) mod model;
