//! MCP (Model Context Protocol) support: the HTTP/JSON-RPC [`client`], the `mcp` command grammar
//! ([`cmd`]), per-server [`config`], and the installed-server [`state`].
//!
//! (The MCP resource virtual-filesystem lives in `crate::runtime::mcpfs` — it sits on the process/fs
//! substrate, not the protocol client.)

pub mod client;
pub(crate) mod cmd;
pub mod config;
pub mod error;

pub use error::Error;
// The native (reqwest) MCP HTTP transport now lives in the separate `clank-native` crate (mirroring
// how the wasm client lives in `clank-embed`), not here: `clank-native::mcp_http` fills the same
// `McpHttp` seam off-Golem, unblocking MCP *and* grease-over-network (they share the transport).
pub mod state;
