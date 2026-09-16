//! clank's command families as a plug-in for the `bash` shell core: `ask`/`context`/`model`, the MCP
//! client, the `grease` package manager and the `golem` cluster commands.
//!
//! Re-exports the shell core so existing `clank_core::session::Session`-style paths keep working.

pub use bash::*;

pub mod ai;
pub mod clank;
pub mod golem;
pub mod grease;
pub mod mcp;

pub use clank::ext::ClankSessionExt;
pub use clank::Clank;
