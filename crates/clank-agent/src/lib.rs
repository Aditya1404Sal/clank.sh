//! clank.sh as a Golem agent: a durable shell instance whose state (the Brush shell + the session
//! transcript) persists across invocations. Each invocation runs one command line.
//!
//! The `golem-rust` `export_golem_agentic` feature + the agent macros emit the component exports;
//! there is no manual `export!` line. The shell engine, wire types, and provider implementations
//! all live in `clank-embed` — this crate is only clank's own agent type.

mod clank_agent;
