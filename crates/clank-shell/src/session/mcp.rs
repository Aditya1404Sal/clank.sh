//! `Session` methods for MCP: the `mcp` management command, tool/resource/template dispatch,
//! session management, and the `/mnt/mcp` dynamic-read + `mcp watch` paths.

use super::*;

impl Session {
    /// Dispatch a parsed `mcp` management command. HTTP-performing subcommands (`add`, `reload`,
    /// `session open`/`close`) require the injected transport; the sync ones (`list`, `tools`,
    /// `remove`, `session list`/`info`) work without it.
    pub(super) async fn run_mcp(&mut self, cmd: crate::mcp::cmd::McpCommand) -> LineResult {
        use crate::mcp::cmd::McpCommand;
        match cmd {
            McpCommand::List => self.mcp_list(),
            McpCommand::Tools { server } => self.mcp_tools(&server),
            McpCommand::Remove { name } => self.mcp_remove(&name),
            McpCommand::Watch { uri } => self.run_mcp_watch(&uri).await,
            McpCommand::ResourceInfo { path } => self.run_mcp_resource_info(&path),
            McpCommand::Add {
                name,
                url,
                auth_env,
                auth_header,
            } => self.mcp_add(&name, &url, auth_env, auth_header).await,
            McpCommand::Reload { name } => self.mcp_reload(name.as_deref()).await,
            McpCommand::SessionList => self.mcp_session_list(),
            McpCommand::SessionInfo { id } => self.mcp_session_info(&id),
            McpCommand::SessionOpen { server } => self.mcp_session_open(&server).await,
            McpCommand::SessionClose { id } => self.mcp_session_close(&id).await,
        }
    }

    /// `mcp list`: configured servers with url/enabled/install status/tool count or error.
    fn mcp_list(&self) -> LineResult {
        let names = crate::mcp::config::list_names();
        if names.is_empty() {
            return LineResult::continue_with_stdout(b"no MCP servers configured\n".to_vec());
        }
        let mut out = String::new();
        for name in &names {
            match self.mcp.get(name) {
                Some(s) if s.installed => out.push_str(&format!(
                    "{name}  {}  enabled  {} tools\n",
                    s.config.url,
                    s.tools.len()
                )),
                Some(s) => out.push_str(&format!(
                    "{name}  {}  not installed  ({})\n",
                    s.config.url,
                    s.last_error.as_deref().unwrap_or("unknown error")
                )),
                None => {
                    // Configured on disk but not yet loaded into this session.
                    let url = crate::mcp::config::load(name)
                        .ok()
                        .flatten()
                        .map(|c| c.url)
                        .unwrap_or_default();
                    out.push_str(&format!("{name}  {url}  not loaded (run `mcp reload {name}`)\n"));
                }
            }
        }
        LineResult::continue_with_stdout(out.into_bytes())
    }

    /// `mcp tools <server>`: list an installed server's tools.
    fn mcp_tools(&self, server: &str) -> LineResult {
        match self.mcp.get(server) {
            Some(s) if s.installed => {
                let mut out = String::new();
                for t in &s.tools {
                    out.push_str(&format!(
                        "{}  {}\n",
                        t.name,
                        t.description.as_deref().unwrap_or("")
                    ));
                }
                LineResult::continue_with_stdout(out.into_bytes())
            }
            Some(_) => LineResult::from_outcome(
                Vec::new(),
                format!("mcp tools: '{server}' is configured but not installed\n").into_bytes(),
                1,
            ),
            None => LineResult::from_outcome(
                Vec::new(),
                format!("mcp tools: no such server '{server}'\n").into_bytes(),
                1,
            ),
        }
    }

    /// `mcp remove <server>`: delete the config + stub and forget the server.
    fn mcp_remove(&mut self, name: &str) -> LineResult {
        match crate::mcp::config::remove(name) {
            Ok(()) => {
                self.mcp.remove(name);
                LineResult::continue_with_stdout(format!("removed MCP server '{name}'\n").into_bytes())
            }
            Err(e) => LineResult::from_outcome(Vec::new(), format!("mcp remove: {e}\n").into_bytes(), 1),
        }
    }

    /// `mcp add <name> <url>`: write the config, then install (initialize + tools/list). An install
    /// failure keeps the config as "configured, not installed" and exits 4.
    async fn mcp_add(
        &mut self,
        name: &str,
        url: &str,
        auth_env: Option<String>,
        auth_header: Option<String>,
    ) -> LineResult {
        if !crate::mcp::config::is_valid_name(name) {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp add: invalid server name '{name}' (use kebab-case: [a-z0-9-])\n").into_bytes(),
                2,
            );
        }
        // Reject a name that collides with a built-in command (it would shadow it).
        if self.registry.get(name).is_some() {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp add: '{name}' collides with a built-in command\n").into_bytes(),
                2,
            );
        }
        let mut config = crate::mcp::config::McpServerConfig::new(url);
        config.auth_env = auth_env;
        config.auth_header = auth_header;
        if let Err(e) = crate::mcp::config::save(name, &config) {
            return LineResult::from_outcome(Vec::new(), format!("mcp add: {e}\n").into_bytes(), 1);
        }
        self.mcp_install(name, config).await
    }

    /// `mcp reload [<name>]`: re-read config(s) and re-install the enabled ones.
    async fn mcp_reload(&mut self, name: Option<&str>) -> LineResult {
        let names: Vec<String> = match name {
            Some(n) => vec![n.to_string()],
            None => crate::mcp::config::list_names(),
        };
        let mut out = String::new();
        let mut any_err = false;
        for n in names {
            let config = match crate::mcp::config::load(&n) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    out.push_str(&format!("mcp reload: no config for '{n}'\n"));
                    any_err = true;
                    continue;
                }
                Err(e) => {
                    out.push_str(&format!("mcp reload: {e}\n"));
                    any_err = true;
                    continue;
                }
            };
            if !config.enabled {
                self.mcp.remove(&n);
                out.push_str(&format!("{n}: disabled (skipped)\n"));
                continue;
            }
            let result = self.mcp_install(&n, config).await;
            out.push_str(&String::from_utf8_lossy(&result.terminal_output()));
            if result.exit_code != 0 {
                any_err = true;
            }
        }
        LineResult::from_outcome(out.into_bytes(), Vec::new(), u8::from(any_err))
    }

    /// Shared install path: initialize + tools/list, record the result in `McpState`, write the
    /// `/usr/lib/mcp/bin` stub on success. A transport/HTTP failure records "configured, not
    /// installed" and exits 4.
    async fn mcp_install(
        &mut self,
        name: &str,
        mut config: crate::mcp::config::McpServerConfig,
    ) -> LineResult {
        let Some(http) = self.mcp_http.as_deref() else {
            self.mcp.set_failed(name, config, "no HTTP transport".into());
            return LineResult::from_outcome(
                Vec::new(),
                b"mcp: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };
        let auth = config.resolve_auth();
        let mut client = crate::mcp::client::McpClient::new(http, &config.url, auth);
        let init = match client.initialize().await {
            Ok(i) => i,
            Err(e) => {
                let msg = format!("mcp: {name}: {}\n", e.message);
                self.mcp.set_failed(name, config, e.message);
                return LineResult::from_outcome(Vec::new(), msg.into_bytes(), e.exit_code);
            }
        };
        let tools = match client.list_tools(init.session_id.as_deref()).await {
            Ok(t) => t,
            Err(e) => {
                let msg = format!("mcp: {name}: {}\n", e.message);
                self.mcp.set_failed(name, config, e.message);
                return LineResult::from_outcome(Vec::new(), msg.into_bytes(), e.exit_code);
            }
        };
        let tool_count = tools.len();
        let mcp_tools: Vec<crate::mcp::state::McpTool> = tools.into_iter().map(Into::into).collect();
        // Cache the fetched tool list in the server's config, so a NEW process reconstructs this
        // server without network (`Session::reconstruct_mcp_from_configs`). Whole-file re-save —
        // idempotent, replay-safe on the durable agent.
        config.tools = mcp_tools
            .iter()
            .map(|t| crate::mcp::config::StoredTool {
                name: t.name.clone(),
                description: t.description.clone().unwrap_or_default(),
                input_schema: t.input_schema.to_string(),
            })
            .collect();
        let _ = crate::mcp::config::save(name, &config);
        self.mcp.set_installed(name, config, mcp_tools);
        // Write the /usr/lib/mcp/bin stub so which/ls/type see the server as a $PATH command.
        if let Some(help) = self.mcp.server_help(name) {
            let _ = crate::mcp::config::write_bin_stub(name, &help);
        }
        LineResult::continue_with_stdout(
            format!("installed MCP server '{name}' ({tool_count} tools)\n").into_bytes(),
        )
    }

    /// `mcp session list`: local id, server, server session id, protocol.
    fn mcp_session_list(&self) -> LineResult {
        let sessions = self.mcp.sessions();
        if sessions.is_empty() {
            return LineResult::continue_with_stdout(b"no open MCP sessions\n".to_vec());
        }
        let mut out = String::new();
        for s in sessions {
            out.push_str(&format!(
                "{}  {}  {}  {}\n",
                s.local_id,
                s.server,
                s.server_session_id.as_deref().unwrap_or("-"),
                s.protocol_version
            ));
        }
        LineResult::continue_with_stdout(out.into_bytes())
    }

    /// `mcp session info <id>`: server info, protocol, capabilities.
    fn mcp_session_info(&self, id: &str) -> LineResult {
        match self.mcp.session(id) {
            Some(s) => {
                let out = format!(
                    "id:         {}\nserver:     {}\nserver info: {}\nprotocol:   {}\ncapabilities: {}\n",
                    s.local_id, s.server, s.server_info, s.protocol_version, s.capabilities
                );
                LineResult::continue_with_stdout(out.into_bytes())
            }
            None => LineResult::from_outcome(
                Vec::new(),
                format!("mcp session info: no such session '{id}'\n").into_bytes(),
                1,
            ),
        }
    }

    /// `mcp session open <server>`: explicit initialize, record the session, print its ids.
    async fn mcp_session_open(&mut self, server: &str) -> LineResult {
        let Some(config) = self.mcp.get(server).map(|s| s.config.clone()) else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp session open: no such installed server '{server}'\n").into_bytes(),
                1,
            );
        };
        let Some(http) = self.mcp_http.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"mcp: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };
        let auth = config.resolve_auth();
        let mut client = crate::mcp::client::McpClient::new(http, &config.url, auth);
        match client.initialize().await {
            Ok(init) => {
                let local_id = self.mcp.open_session(server, &init);
                LineResult::continue_with_stdout(
                    format!(
                        "opened session {local_id} ({})\n",
                        init.session_id.as_deref().unwrap_or("no server session id")
                    )
                    .into_bytes(),
                )
            }
            Err(e) => LineResult::from_outcome(
                Vec::new(),
                format!("mcp session open: {}\n", e.message).into_bytes(),
                e.exit_code,
            ),
        }
    }

    /// `mcp session close <id>`: DELETE the server session, remove it locally. A 405 refusal still
    /// removes the local session (with a note).
    async fn mcp_session_close(&mut self, id: &str) -> LineResult {
        let Some((server, server_sid)) = self.mcp.close_session(id) else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp session close: no such session '{id}'\n").into_bytes(),
                1,
            );
        };
        // If there's no server-issued session id, there's nothing to DELETE — local removal is enough.
        let Some(server_sid) = server_sid else {
            return LineResult::continue_with_stdout(format!("closed session {id}\n").into_bytes());
        };
        let config = self.mcp.get(&server).map(|s| s.config.clone());
        let (Some(config), Some(http)) = (config, self.mcp_http.as_deref()) else {
            return LineResult::continue_with_stdout(
                format!("closed session {id} (locally; server not reachable)\n").into_bytes(),
            );
        };
        let auth = config.resolve_auth();
        let client = crate::mcp::client::McpClient::new(http, &config.url, auth);
        match client.close_session(&server_sid).await {
            Ok(()) => LineResult::continue_with_stdout(format!("closed session {id}\n").into_bytes()),
            Err(e) => LineResult::from_outcome(
                format!("closed session {id} locally\n").into_bytes(),
                format!("mcp session close: {}\n", e.message).into_bytes(),
                e.exit_code,
            ),
        }
    }

    /// Whether `line`'s leading word is an installed MCP server (and it isn't `mcp` itself). Drives
    /// the dynamic `<server> <tool>` dispatch.
    pub(super) fn is_mcp_tool_line(&self, line: &str) -> bool {
        match crate::mcp::cmd::parse_tool_invocation(line) {
            Some(Ok(inv)) => self.mcp.is_server(&inv.server),
            _ => false,
        }
    }

    /// If `line` is a top-level `cat /mnt/mcp/<server>/<path>` (optionally `sudo`-prefixed, one
    /// operand, no operators) naming a DYNAMIC MCP resource, return its `(server, uri)`. Static
    /// resources are real files that Brush's `cat` reads directly, so they return `None` here.
    pub(super) fn dynamic_mcp_read_target(&self, line: &str) -> Option<(String, String)> {
        // Reject anything with shell operators (the Wall-C wall — a live read can't run in a pipe).
        if line.chars().any(|c| "|&;<>`$".contains(c)) {
            return None;
        }
        let words = crate::ai::ask::dequote_words(line)?;
        let mut it = words.iter();
        let mut first = it.next()?.as_str();
        if first == "sudo" {
            first = it.next()?.as_str();
        }
        if first != "cat" {
            return None;
        }
        // Exactly one non-flag operand, and it must be an /mnt/mcp path.
        let operands: Vec<&String> = it.filter(|w| !w.starts_with('-')).collect();
        if operands.len() != 1 {
            return None;
        }
        let path = operands[0];
        if !crate::runtime::mcpfs::is_mcp_path(path) {
            return None;
        }
        let index = self.grease.mcp_resource_index();
        match crate::runtime::mcpfs::classify(path, &index) {
            crate::runtime::mcpfs::McpPathKind::Dynamic { server, uri } => Some((server, uri)),
            _ => None,
        }
    }

    /// Fetch a dynamic MCP resource live (`resources/read`) and print its content. Reuses the server's
    /// stored config for the endpoint + auth.
    pub(super) async fn run_mcp_resource_read(&mut self, server: &str, uri: &str) -> LineResult {
        let Some(m) = self.grease.mcp(server) else {
            return LineResult::from_outcome(Vec::new(), b"cat: mcp resource: server not installed\n".to_vec(), 1);
        };
        let url = m.url.clone();
        let auth_env = m.auth_env.clone();
        let Some(http) = self.mcp_http.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"cat: mcp resource: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };
        let config = crate::mcp::config::McpServerConfig { url: url.clone(), enabled: true, auth_env, auth_header: None, tools: Vec::new() };
        let auth = config.resolve_auth();
        let mut client = crate::mcp::client::McpClient::new(http, &url, auth);
        let session = client.initialize().await.ok().and_then(|i| i.session_id);
        match client.read_resource(uri, session.as_deref()).await {
            Ok(content) => LineResult::continue_with_stdout(content.into_bytes()),
            Err(e) => LineResult::from_outcome(Vec::new(), format!("cat: {uri}: {}\n", e.message).into_bytes(), e.exit_code),
        }
    }

    /// `mcp resource info <path>` — print the full MCP annotation set for a mounted resource. Reads
    /// from the cached resource index (no live fetch); an unknown path is an error.
    fn run_mcp_resource_info(&self, path: &str) -> LineResult {
        if !crate::runtime::mcpfs::is_mcp_path(path) {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp resource info: '{path}' is not a /mnt/mcp resource\n").into_bytes(),
                2,
            );
        }
        // Split `/mnt/mcp/<server>/<rel>`.
        let rel = path.trim_start_matches("/mnt/mcp").trim_start_matches('/');
        let Some((server, sub)) = rel.split_once('/') else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp resource info: '{path}' names a server, not a resource\n").into_bytes(),
                2,
            );
        };
        let Some(res) = self.grease.mcp_resource_entry(server, sub) else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp resource info: no such resource '{path}'\n").into_bytes(),
                1,
            );
        };
        let mut out = format!("uri: {}\n", res.uri);
        out.push_str(&format!("kind: {}\n", if res.is_static { "static" } else { "dynamic" }));
        if !res.description.is_empty() {
            out.push_str(&format!("description: {}\n", res.description));
        }
        if let Some(m) = &res.mime_type {
            out.push_str(&format!("mime-type: {m}\n"));
        }
        if let Some(s) = res.size {
            out.push_str(&format!("size: {s}\n"));
        }
        if let Some(lm) = &res.last_modified {
            out.push_str(&format!("last-modified: {lm}\n"));
        }
        if let Some(a) = &res.audience {
            out.push_str(&format!("audience: {a}\n"));
        }
        if let Some(p) = res.priority {
            out.push_str(&format!("priority: {p}\n"));
        }
        LineResult::continue_with_stdout(out.into_bytes())
    }

    /// `mcp watch <uri>` — a BOUNDED poll-based subscription (the durable agent can't hold a long-lived
    /// push stream across serialized invocations, and the transport is one-shot request/response). We
    /// `resources/subscribe` then poll `resources/read` a fixed number of times, printing the content
    /// each time it changes. Honest about being polling, not push.
    async fn run_mcp_watch(&mut self, uri: &str) -> LineResult {
        // Resolve which installed server owns this URI (by scheme/prefix match against its resources).
        let server = self.grease.mcp_packages().iter().find_map(|m| {
            let owns = m.resources.iter().any(|r| r.uri == uri)
                || uri.split_once("://").map(|(s, _)| s) == Some(m.name.as_str());
            if owns {
                Some((m.name.clone(), m.url.clone(), m.auth_env.clone()))
            } else {
                None
            }
        });
        let Some((_name, url, auth_env)) = server else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp watch: no installed server owns '{uri}'\n").into_bytes(),
                1,
            );
        };
        let Some(http) = self.mcp_http.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"mcp watch: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };
        let config = crate::mcp::config::McpServerConfig { url: url.clone(), enabled: true, auth_env, auth_header: None, tools: Vec::new() };
        let auth = config.resolve_auth();
        let mut client = crate::mcp::client::McpClient::new(http, &url, auth);
        let session = client.initialize().await.ok().and_then(|i| i.session_id);
        let _ = client.subscribe_resource(uri, session.as_deref()).await; // best-effort

        // Bounded poll loop: read the resource a fixed number of times, printing on change.
        const POLLS: usize = 3;
        let mut out = format!(
            "mcp watch {uri}: polling {POLLS}× (the durable agent can't hold a push stream; this is a \
             bounded poll, not a live subscription)\n"
        );
        let mut last: Option<String> = None;
        for i in 0..POLLS {
            match client.read_resource(uri, session.as_deref()).await {
                Ok(content) => {
                    if last.as_deref() != Some(content.as_str()) {
                        out.push_str(&format!("[poll {}] {}\n", i + 1, content.trim_end()));
                        last = Some(content);
                    } else {
                        out.push_str(&format!("[poll {}] (unchanged)\n", i + 1));
                    }
                }
                Err(e) => {
                    out.push_str(&format!("[poll {}] error: {}\n", i + 1, e.message));
                }
            }
        }
        out.push_str("mcp watch: done (bounded poll complete)\n");
        LineResult::continue_with_stdout(out.into_bytes())
    }

    /// Whether `line`'s leading word is an installed MCP resource-template executable
    /// (`<server>-<template>`). Top-level only (Wall-C — the read awaits under the reactor).
    pub(super) fn is_mcp_template_line(&self, line: &str) -> bool {
        let Some(word) = prompt_leading_word(line) else {
            return false;
        };
        self.grease.is_mcp_template(&word)
    }

    /// Run an installed MCP resource template: substitute the CLI args into the `{param}` placeholders
    /// of the stored URI template, then read the constructed resource live and print it (README:767).
    /// Positional args fill the template's `{param}` placeholders in order; `--param value` fills by
    /// name. The read awaits under the reactor (top-level only).
    pub(super) async fn run_mcp_template(&mut self, line: &str) -> LineResult {
        let words = match crate::ai::ask::dequote_words(line) {
            Some(w) => w,
            None => return LineResult::from_outcome(Vec::new(), b"mcp template: parse error\n".to_vec(), 2),
        };
        // Strip a leading sudo (the gate already resolved authz).
        let rest = if words.first().map(String::as_str) == Some("sudo") { &words[1..] } else { &words[..] };
        let cmd = rest[0].clone();
        let Some((url, auth_env, template)) = self.grease.mcp_template(&cmd) else {
            return LineResult::denied(); // is_mcp_template_line gated it
        };
        // Build the concrete URI: fill `{param}` placeholders. `--name value` fills by name; bare
        // positionals fill the remaining `{…}` slots left-to-right.
        let uri = match fill_uri_template(&template, &rest[1..]) {
            Ok(u) => u,
            Err(e) => return LineResult::from_outcome(Vec::new(), format!("{cmd}: {e}\n").into_bytes(), 2),
        };
        let Some(http) = self.mcp_http.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("{cmd}: no HTTP transport configured (available on the Golem agent)\n").into_bytes(),
                4,
            );
        };
        let config = crate::mcp::config::McpServerConfig { url: url.clone(), enabled: true, auth_env, auth_header: None, tools: Vec::new() };
        let auth = config.resolve_auth();
        let mut client = crate::mcp::client::McpClient::new(http, &url, auth);
        let session = client.initialize().await.ok().and_then(|i| i.session_id);
        match client.read_resource(&uri, session.as_deref()).await {
            Ok(content) => LineResult::continue_with_stdout(content.into_bytes()),
            Err(e) => LineResult::from_outcome(Vec::new(), format!("{cmd}: {uri}: {}\n", e.message).into_bytes(), e.exit_code),
        }
    }
}
