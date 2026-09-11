//! clank's **native platform layer** — the mirror image of `clank-embed`.
//!
//! `clank-core` defines the shell engine and five injected seams (`AskProvider`, `McpHttp`,
//! `AgentInvoker`, `GolemCluster`, `LogSink`) but is deliberately target-agnostic: it implements
//! none of them. Each platform crate supplies one implementation set —
//!
//! | | wasm / Golem agent | native |
//! |---|---|---|
//! | crate | `clank-embed` | **this crate** |
//! | transport | `wasi-fetch` over wasip3 WASI-HTTP | `reqwest` + rustls |
//! | entry point | `EmbeddedShell::with_default_golem_providers` | [`inject_native_providers`] |
//!
//! Before this crate existed the wasm half lived in `clank-embed` while the native half sat inside
//! `clank-core` — so the boundary was asymmetric for no principled reason, `clank-core` carried
//! `reqwest`/`reedline`/`crossterm`, and understanding a single seam meant reading three crates.
//!
//! It also owns the interactive REPL ([`run`]): the reedline TUI, the prompt, and the plain
//! line-reading fallback for piped or non-interactive stdin.

/// The Anthropic Messages API provider (`reqwest`), sharing its wire format with the agent's
/// durable provider via `clank_core::ai::anthropic_wire`.
pub mod anthropic;
/// Reader for the external Golem cluster config that gates [`rest`].
pub mod cluster_config;
/// The provider-routing dispatcher: `provider/model` → the transport for that provider.
pub mod llm;
/// The MCP HTTP transport (`reqwest`), which also backs every `grease` registry fetch.
pub mod mcp_http;
/// The OpenAI Chat Completions API provider, shared by openai/grok/openrouter/ollama.
pub mod openai;
/// REST-backed `AgentInvoker`/`GolemCluster` against a Golem cluster's HTTP API.
pub mod rest;
/// The native entry point: the REPL loop and provider wiring.
pub mod run;

pub use run::{inject_native_providers, run};
