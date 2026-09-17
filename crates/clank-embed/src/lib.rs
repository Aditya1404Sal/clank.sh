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
//! shell [`Session`](bash::session::Session) scoped to *your agent's own instance* (its own
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
//!     fn new(name: String) -> Self { Self { shell: EmbeddedShell::new() } }
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
//! **Tiers.** A default-features embed is the *exploration shell*: the shell core over the agent's
//! own filesystem (`ls`/`cat`/`grep`/pipelines/redirects/`cd`…), with none of clank's command
//! families installed — `ask`, `mcp`, `grease` and `golem` are simply not commands there. The
//! `providers` feature installs the clank plug-in with clank's durable Golem provider set, via
//! [`EmbeddedShell::with_default_golem_providers`].

mod shell;
mod wire;

pub mod log_sink;

// `agent_invoker` is feature-gated only, NOT target-gated: it compiles natively, and its unit tests
// (argument encoding, result rendering, phantom-UUID parsing) are pure logic worth running on the
// host. Keeping it host-compilable is what lets CI execute those 8 tests at all.
#[cfg(feature = "providers")]
pub mod agent_invoker;

// `golem_cluster` is target-gated with the two below. Unlike agent_invoker it is ALL host calls
// (`get_self_metadata`, `fork`) with no host-testable logic and no tests, so compiling it natively
// buys nothing — and once `with_default_golem_providers` became wasm-only, nothing constructed it
// on the host, which `dead_code` correctly flagged.
#[cfg(all(feature = "providers", target_arch = "wasm32"))]
pub mod golem_cluster;

// These two additionally require `target_arch = "wasm32"`, because they go through `whttp`'s wasm
// arm (`wasi-fetch`, a wasip3 WASI-HTTP client with no host implementation). This crate's own
// `whttp` dependency is target-gated in Cargo.toml for exactly that reason, so the modules must be
// too; gating by feature alone let `cargo build -p clank-embed --features providers` succeed
// natively while linking a wasm-only client into a host build.
#[cfg(all(feature = "providers", target_arch = "wasm32"))]
pub mod ask_provider;
#[cfg(all(feature = "providers", target_arch = "wasm32"))]
pub mod mcp_http;

pub use shell::EmbeddedShell;
pub use wire::{EvalResult, PendingPromptView};

// The replay-safe `/var/log` sink. Re-exported because an embedder combining it with a hand-picked
// provider mix must be able to NAME it — it was `pub(crate)`, so the documented `with_setup`
// example demonstrating exactly that could not compile for any external caller.
pub use log_sink::DurableLogSink;

// Re-exported unconditionally so an embedder can name `Session` in a `with_setup` closure without
// adding its own `bash` dependency line — every tier needs this, since the bare `EmbeddedShell`
// (no plug-in) is built directly on `bash::session::Session`.
pub use bash;

// Re-exported behind `providers` so an embedder that hand-picks its own provider mix (implementing
// the seam traits, or naming `ClankSessionExt`/`Clank`) doesn't need its own `clank-core` dependency
// line either. `clank-core` itself is optional now (see Cargo.toml), pulled in only by this feature.
#[cfg(feature = "providers")]
pub use clank_core;
