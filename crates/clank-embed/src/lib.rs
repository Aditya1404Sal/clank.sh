//! Embed clank's shell in your own Golem agent.
//!
//! `golem agent shell <agent-id>` drives any agent exposing the *shell surface* — three methods:
//!
//! | Method | Signature | Role |
//! |---|---|---|
//! | `eval` | `(String) -> EvalResult` | run one command line |
//! | `answer_prompt` | `(String) -> EvalResult` | answer an outstanding question |
//! | `abort_prompt` | `() -> EvalResult` | cancel an outstanding question |
//!
//! This crate provides everything behind that surface so an agent adopts it with ~12 lines of glue:
//! the [`EvalResult`]/[`PendingPromptView`] wire types, and [`EmbeddedShell`] — a lazily-initialized
//! shell [`Session`](clank_shell::session::Session) scoped to *your agent's own instance* (its own
//! durable filesystem, transcript, and process table; one agent instance = one Golem worker = one
//! isolated VFS, so the shell explores exactly your agent's sandbox and nothing else's).
//!
//! ```ignore
//! use clank_embed::{EmbeddedShell, EvalResult};
//!
//! #[agent_definition]
//! pub trait MyAgent {
//!     fn new(name: String) -> Self;
//!     // ... your own methods ...
//!     async fn eval(&mut self, cmd: String) -> EvalResult;
//!     async fn answer_prompt(&mut self, response: String) -> EvalResult;
//!     async fn abort_prompt(&mut self) -> EvalResult;
//! }
//!
//! pub struct MyAgentImpl { shell: EmbeddedShell }
//!
//! #[agent_implementation]
//! impl MyAgent for MyAgentImpl {
//!     fn new(name: String) -> Self { Self { shell: EmbeddedShell::with_durable_log_sink() } }
//!     async fn eval(&mut self, cmd: String) -> EvalResult { self.shell.eval(&cmd).await }
//!     async fn answer_prompt(&mut self, r: String) -> EvalResult { self.shell.answer(Some(r)).await }
//!     async fn abort_prompt(&mut self) -> EvalResult { self.shell.answer(None).await }
//! }
//! ```
//!
//! The three methods must appear in **both** the `#[agent_definition]` trait and the
//! `#[agent_implementation]` impl: golem-rust reflects the trait's method list into the agent-type
//! schema, but generates invoke dispatch only from the impl's items — a trait-default method would
//! be reflected yet fail every invocation. That per-agent glue is therefore irreducible today; this
//! crate single-sources everything else.
//!
//! **Tiers.** A default-features embed is the *exploration shell*: the full command surface over the
//! agent's own filesystem (`ls`/`cat`/`grep`/pipelines/redirects/`cd`…), with the model/MCP/cluster
//! commands (`ask`, `mcp`, `golem`, installed-agent invocation) degrading to honest errors. The
//! `providers` feature adds clank's durable Golem provider set and
//! [`EmbeddedShell::with_default_golem_providers`] to enable them all.

mod shell;
mod wire;

pub mod log_sink;

#[cfg(feature = "providers")]
pub mod agent_invoker;
#[cfg(feature = "providers")]
pub mod ask_provider;
#[cfg(feature = "providers")]
pub mod golem_cluster;
#[cfg(feature = "providers")]
pub mod mcp_http;

pub use shell::EmbeddedShell;
pub use wire::{EvalResult, PendingPromptView};

// Re-exported so an embedder can name `Session` in a `with_setup` closure (or implement the
// provider seam traits) without adding its own clank-shell dependency line.
pub use clank_shell;
