//! MCP (Model Context Protocol) support: the HTTP/JSON-RPC [`client`], the `mcp` command grammar
//! ([`cmd`]), per-server [`config`], and the installed-server [`state`].
//!
//! (The MCP resource virtual-filesystem lives in `crate::runtime::mcpfs` — it sits on the process/fs
//! substrate, not the protocol client.)

pub mod client;
pub(crate) mod cmd;
pub mod config;
pub mod state;
