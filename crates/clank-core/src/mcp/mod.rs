//! MCP (Model Context Protocol) support: the HTTP/JSON-RPC [`client`], the `mcp` command grammar
//! ([`cmd`]), per-server [`config`], and the installed-server [`state`].
//!
//! (The MCP resource virtual-filesystem lives in `crate::runtime::mcpfs` — it sits on the process/fs
//! substrate, not the protocol client.)

pub mod client;
pub(crate) mod cmd;
pub mod config;
// The native (reqwest) MCP HTTP transport. wasm uses the injected `wstd` client from `clank-agent`;
// this fills the same `McpHttp` seam off-Golem, unblocking MCP *and* grease-over-network (they share
// the transport). cfg-gated so `reqwest` never reaches the wasm build.
#[cfg(not(target_arch = "wasm32"))]
pub mod http_native;
pub mod state;
