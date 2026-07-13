//! `Session`-held MCP state: installed servers (with their fetched tool lists) and open sessions.
//!
//! Reconstructed deterministically under Golem replay — the installed-tools table derives from the
//! `mcp add`/`reload` lines, whose HTTP (`initialize`/`tools/list`) replays from the oplog (same
//! mechanism as curl durability). Local session ids (`s1`, `s2`, …) and the JSON-RPC id counter are
//! plain monotonic counters, deterministic like PIDs.

use serde_json::Value;

use crate::manifest::{AuthorizationPolicy, ExecutionScope, Manifest, ParamSpec, ParamType};
use crate::mcp::client::{InitializeResult, ToolSpec};
use crate::mcp::config::McpServerConfig;

/// One installed MCP tool (mirrors [`ToolSpec`] with an owned schema).
#[derive(Clone, Debug)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

impl From<ToolSpec> for McpTool {
    fn from(t: ToolSpec) -> Self {
        Self { name: t.name, description: t.description, input_schema: t.input_schema }
    }
}

/// One configured MCP server plus its install status.
#[derive(Clone, Debug)]
pub struct McpServer {
    pub name: String,
    pub config: McpServerConfig,
    /// The tools fetched at install; empty until a successful `tools/list`.
    pub tools: Vec<McpTool>,
    /// Whether `initialize` + `tools/list` succeeded (`false` ⇒ configured but not installed).
    pub installed: bool,
    /// The last install error, if any (shown by `mcp list`).
    pub last_error: Option<String>,
}

/// One open MCP session: a local id, the server it belongs to, and the server-issued session id.
#[derive(Clone, Debug)]
pub struct McpSession {
    pub local_id: String,
    pub server: String,
    pub server_session_id: Option<String>,
    pub protocol_version: String,
    pub server_info: String,
    pub capabilities: Value,
}

/// The `Session`-held MCP state.
#[derive(Default)]
pub struct McpState {
    servers: Vec<McpServer>,
    sessions: Vec<McpSession>,
    next_session: u64,
    /// Monotonic counter bumped on every server change (not session open/close — sessions don't affect
    /// the command surface), so the `Session` can cache capability views and rebuild only on change.
    version: u64,
}

impl McpState {
    /// The mutation counter — bumped by server install/remove (see [`version`](Self::version)'s field
    /// docs). A cache keyed on it is invalidated exactly when the manifests/system-prompt would change.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Install or replace a server's record with a fetched tool list.
    pub fn set_installed(&mut self, name: &str, config: McpServerConfig, tools: Vec<McpTool>) {
        self.remove(name);
        self.servers.push(McpServer {
            name: name.to_string(),
            config,
            tools,
            installed: true,
            last_error: None,
        });
        self.version = self.version.wrapping_add(1);
    }

    /// Record a server that is configured but failed to install (with the error).
    pub fn set_failed(&mut self, name: &str, config: McpServerConfig, error: String) {
        self.remove(name);
        self.servers.push(McpServer {
            name: name.to_string(),
            config,
            tools: Vec::new(),
            installed: false,
            last_error: Some(error),
        });
        self.version = self.version.wrapping_add(1);
    }

    /// Drop a server (and any of its sessions).
    pub fn remove(&mut self, name: &str) {
        self.servers.retain(|s| s.name != name);
        self.sessions.retain(|s| s.server != name);
        self.version = self.version.wrapping_add(1);
    }

    pub fn servers(&self) -> &[McpServer] {
        &self.servers
    }

    pub fn get(&self, name: &str) -> Option<&McpServer> {
        self.servers.iter().find(|s| s.name == name)
    }

    /// Whether `name` is an installed server (drives command dispatch).
    pub fn is_server(&self, name: &str) -> bool {
        self.servers.iter().any(|s| s.name == name && s.installed)
    }

    /// A tool spec by server + tool name.
    pub fn tool(&self, server: &str, tool: &str) -> Option<&McpTool> {
        self.get(server)?.tools.iter().find(|t| t.name == tool)
    }

    // ---- sessions ----

    /// Open (record) a new session from an `initialize` result. Returns the local id (`s1`, `s2`, …).
    pub fn open_session(&mut self, server: &str, init: &InitializeResult) -> String {
        self.next_session += 1;
        let local_id = format!("s{}", self.next_session);
        self.sessions.push(McpSession {
            local_id: local_id.clone(),
            server: server.to_string(),
            server_session_id: init.session_id.clone(),
            protocol_version: init.protocol_version.clone(),
            server_info: format!("{} {}", init.server_name, init.server_version),
            capabilities: init.capabilities.clone(),
        });
        local_id
    }

    pub fn sessions(&self) -> &[McpSession] {
        &self.sessions
    }

    pub fn session(&self, local_id: &str) -> Option<&McpSession> {
        self.sessions.iter().find(|s| s.local_id == local_id)
    }

    /// The first open session for a server, if any (implicit-session reuse).
    pub fn session_for(&self, server: &str) -> Option<&McpSession> {
        self.sessions.iter().find(|s| s.server == server)
    }

    /// Remove a session by local id; returns its server session id (for the DELETE) if it existed.
    pub fn close_session(&mut self, local_id: &str) -> Option<(String, Option<String>)> {
        let idx = self.sessions.iter().position(|s| s.local_id == local_id)?;
        let s = self.sessions.remove(idx);
        Some((s.server, s.server_session_id))
    }

    // ---- dynamic manifest surface ----

    /// The dynamic manifest for an installed server: `Subprocess` scope, `Confirm` policy (MCP tool
    /// calls are outbound HTTP), one subcommand per tool (schema → [`ParamSpec`] for help/completion;
    /// the raw schema stays on [`McpTool`]).
    pub fn manifest_for(&self, name: &str) -> Option<Manifest> {
        let server = self.get(name)?;
        if !server.installed {
            return None;
        }
        let subcommands: Vec<Manifest> = server
            .tools
            .iter()
            .map(|t| {
                Manifest::builtin(&t.name, t.description.clone().unwrap_or_default())
                    .with_scope(ExecutionScope::Subprocess)
                    .with_policy(AuthorizationPolicy::Confirm)
                    .with_params(schema_to_params(&t.input_schema))
            })
            .collect();
        let synopsis = format!("MCP server ({} tools)", server.tools.len());
        let mut m = Manifest::builtin(name, synopsis)
            .with_scope(ExecutionScope::Subprocess)
            .with_policy(AuthorizationPolicy::Confirm)
            .with_help(self.server_help(name).unwrap_or_default());
        m.subcommands = subcommands;
        Some(m)
    }

    /// The dynamic manifests for all installed servers (for the per-line [`crate::runtime::dynreg`] slot that
    /// `man` consults).
    pub fn all_manifests(&self) -> Vec<Manifest> {
        self.servers
            .iter()
            .filter_map(|s| self.manifest_for(&s.name))
            .collect()
    }

    /// Every installed MCP tool as an [`crate::ai::ask::AskTool`], for the agentic `ask` tool surface.
    /// The tool name is namespaced `mcp__<server>__<tool>` (the executor decodes it back to a
    /// `<server> <tool>` call); the parameters schema is the raw inputSchema string.
    pub fn ask_tool_definitions(&self) -> Vec<crate::ai::ask::AskTool> {
        let mut tools = Vec::new();
        for server in &self.servers {
            if !server.installed {
                continue;
            }
            for t in &server.tools {
                let desc = format!(
                    "[MCP: {}] {}",
                    server.name,
                    t.description.as_deref().unwrap_or("")
                );
                tools.push(crate::ai::ask::AskTool {
                    name: format!("mcp__{}__{}", server.name, t.name),
                    description: desc,
                    parameters_schema: t.input_schema.to_string(),
                });
            }
        }
        tools
    }

    /// Human-facing help for a server: its tools and their synopses.
    pub fn server_help(&self, name: &str) -> Option<String> {
        let server = self.get(name)?;
        let mut out = format!("{name} — MCP server at {}\n\nTools:\n", server.config.url);
        if server.tools.is_empty() {
            out.push_str("  (none)\n");
        }
        for t in &server.tools {
            let desc = t.description.as_deref().unwrap_or("");
            out.push_str(&format!("  {name} {} — {desc}\n", t.name));
        }
        out.push_str(&format!(
            "\nUsage: {name} <tool> [--param value ...] [--args '<json>'] [--json] [--session-id <id>]\n\
             MCP tool calls are outbound HTTP and require confirmation (or `sudo {name} …`).\n"
        ));
        Some(out)
    }
}

/// Translate a JSON-schema `object` into [`ParamSpec`]s for the help/completion surface. Lossy by
/// design — the raw schema is kept on [`McpTool`] for the actual call; this is display metadata only.
fn schema_to_params(schema: &Value) -> Vec<ParamSpec> {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut params: Vec<ParamSpec> = props
        .iter()
        .map(|(name, spec)| {
            let ty = match spec.get("type").and_then(Value::as_str) {
                Some("integer") | Some("number") => ParamType::Int,
                Some("boolean") => ParamType::Flag,
                Some("string") => match spec.get("enum").and_then(Value::as_array) {
                    Some(vals) => ParamType::Enum(
                        vals.iter().filter_map(Value::as_str).map(String::from).collect(),
                    ),
                    None => ParamType::String,
                },
                _ => ParamType::String,
            };
            ParamSpec {
                name: name.clone(),
                ty,
                required: required.contains(&name.as_str()),
                default: None,
            }
        })
        .collect();
    params.sort_by(|a, b| a.name.cmp(&b.name));
    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::config::McpServerConfig;

    fn tool(name: &str, schema: Value) -> McpTool {
        McpTool { name: name.into(), description: Some(format!("does {name}")), input_schema: schema }
    }

    #[test]
    fn installed_server_exposes_a_confirm_manifest_with_tool_subcommands() {
        let mut state = McpState::default();
        state.set_installed(
            "demo",
            McpServerConfig::new("https://x/mcp"),
            vec![tool("search", serde_json::json!({
                "type":"object",
                "properties":{"query":{"type":"string"},"limit":{"type":"integer"}},
                "required":["query"]
            }))],
        );
        assert!(state.is_server("demo"));
        let m = state.manifest_for("demo").unwrap();
        assert_eq!(m.authorization_policy, AuthorizationPolicy::Confirm);
        assert_eq!(m.subcommands.len(), 1);
        let sub = &m.subcommands[0];
        assert_eq!(sub.name, "search");
        // query (required, string) + limit (int).
        assert_eq!(sub.input_schema.len(), 2);
        let q = sub.input_schema.iter().find(|p| p.name == "query").unwrap();
        assert!(q.required && matches!(q.ty, ParamType::String));
        let l = sub.input_schema.iter().find(|p| p.name == "limit").unwrap();
        assert!(!l.required && matches!(l.ty, ParamType::Int));
    }

    #[test]
    fn failed_install_is_not_a_server() {
        let mut state = McpState::default();
        state.set_failed("bad", McpServerConfig::new("https://x/mcp"), "boom".into());
        assert!(!state.is_server("bad"));
        assert!(state.manifest_for("bad").is_none());
        assert_eq!(state.get("bad").unwrap().last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn version_bumps_on_server_changes_only() {
        // The Session's capability cache keys on version(); every change to the command surface (install
        // / failed-install / remove) must bump it, but opening/closing sessions must NOT (sessions aren't
        // part of the manifests/system-prompt).
        let mut state = McpState::default();
        let v0 = state.version();
        state.set_installed("a", McpServerConfig::new("https://x/mcp"), vec![]);
        let v1 = state.version();
        assert_ne!(v1, v0, "set_installed must bump the version");
        state.set_failed("b", McpServerConfig::new("https://y/mcp"), "boom".into());
        let v2 = state.version();
        assert_ne!(v2, v1, "set_failed must bump the version");
        // A session open does not change the command surface → no bump.
        let init = InitializeResult {
            session_id: None,
            protocol_version: "1".into(),
            server_name: "a".into(),
            server_version: "1".into(),
            capabilities: Value::Null,
        };
        state.open_session("a", &init);
        assert_eq!(state.version(), v2, "opening a session must NOT bump the version");
        state.remove("a");
        assert_ne!(state.version(), v2, "remove must bump the version");
    }

    #[test]
    fn sessions_open_close_with_monotonic_ids() {
        let mut state = McpState::default();
        let init = InitializeResult {
            protocol_version: "2025-03-26".into(),
            server_name: "demo".into(),
            server_version: "1".into(),
            session_id: Some("srv-1".into()),
            capabilities: serde_json::json!({}),
        };
        let id1 = state.open_session("demo", &init);
        let id2 = state.open_session("demo", &init);
        assert_eq!(id1, "s1");
        assert_eq!(id2, "s2");
        assert!(state.session_for("demo").is_some());
        let (server, srv_sid) = state.close_session("s1").unwrap();
        assert_eq!(server, "demo");
        assert_eq!(srv_sid.as_deref(), Some("srv-1"));
        assert!(state.session("s1").is_none());
    }
}
