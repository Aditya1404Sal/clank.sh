//! clank.sh as a Golem agent: a durable shell instance whose state (the Brush shell + the session
//! transcript) persists across invocations. Each invocation runs one command line.
//!
//! The `golem-rust` `export_golem_agentic` feature + the agent macros emit the component exports;
//! there is no manual `export!` line.

mod ask_provider;
mod clank_agent;
mod mcp_http;
