//! The `mcp` command: classifier + grammar for MCP server management.
//!
//! `mcp` is a **Session-layer interception** (like `curl`/`ask`) because its HTTP-performing
//! subcommands (`add`, `reload`, `session open`/`close`) must await under the live WASI-HTTP reactor.
//! The sync subcommands (`list`, `tools`, `remove`, `session list`/`info`) don't strictly need it, but
//! routing the whole command through one place keeps dispatch simple. The actual work lives in
//! `Session` methods; this module only parses.
//!
//! MCP-lite grammar (tools only; resources/prompts/watch deferred):
//! ```text
//! mcp list
//! mcp add <name> <url> [--auth-env VAR] [--auth-header HEADER]
//! mcp remove <name>
//! mcp reload [<name>]
//! mcp tools <server>
//! mcp session list | open <server> | close <id> | info <id>
//! mcp watch <uri>            (honest "not supported in MCP-lite")
//! ```

use brush_parser::{tokenize_str, unquote_str, Token};

/// A parsed `mcp` command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum McpCommand {
    List,
    Add {
        name: String,
        url: String,
        auth_env: Option<String>,
        auth_header: Option<String>,
    },
    Remove {
        name: String,
    },
    Reload {
        name: Option<String>,
    },
    Tools {
        server: String,
    },
    SessionList,
    SessionOpen {
        server: String,
    },
    SessionClose {
        id: String,
    },
    SessionInfo {
        id: String,
    },
    /// `mcp watch <uri>` — poll-based subscription to a resource's changes.
    Watch {
        uri: String,
    },
    /// `mcp resource info <path>` — the full annotation set for a mounted `/mnt/mcp/...` resource.
    ResourceInfo {
        path: String,
    },
}

/// Recognize an `mcp` line. `None` when it isn't one; `Some(Err)` when it is but doesn't parse.
pub(crate) fn classify(line: &str) -> Option<Result<McpCommand, String>> {
    let tokens = tokenize_str(line).ok()?;
    // If the line contains an operator (pipe/redirect/;), it isn't a bare `mcp` management command —
    // let it fall through to Brush (a stub handles nested contexts).
    if tokens.iter().any(|t| matches!(t, Token::Operator(_, _))) {
        return None;
    }
    let words: Vec<String> = tokens
        .into_iter()
        .filter_map(|t| match t {
            Token::Word(s, _) => Some(unquote_str(&s)),
            Token::Operator(_, _) => None,
        })
        .collect();
    if words.first().map(String::as_str) != Some("mcp") {
        return None;
    }
    Some(parse(&words[1..]))
}

fn parse(args: &[String]) -> Result<McpCommand, String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" => Ok(McpCommand::List),
        "add" => parse_add(&args[1..]),
        "remove" | "rm" => {
            let name = args.get(1).ok_or("mcp remove: needs a server name")?;
            Ok(McpCommand::Remove { name: name.clone() })
        }
        "reload" => Ok(McpCommand::Reload {
            name: args.get(1).cloned(),
        }),
        "tools" => {
            let server = args.get(1).ok_or("mcp tools: needs a server name")?;
            Ok(McpCommand::Tools { server: server.clone() })
        }
        "session" => parse_session(&args[1..]),
        "watch" => {
            let uri = args.get(1).ok_or("mcp watch: needs a resource uri")?;
            Ok(McpCommand::Watch { uri: uri.clone() })
        }
        "resource" => {
            // `mcp resource info <path>`.
            match args.get(1).map(String::as_str) {
                Some("info") => {
                    let path = args.get(2).ok_or("mcp resource info: needs a /mnt/mcp path")?;
                    Ok(McpCommand::ResourceInfo { path: path.clone() })
                }
                Some(other) => Err(format!("mcp resource: unknown subcommand '{other}' (try: info)")),
                None => Err("mcp resource: needs a subcommand (try: info)".to_string()),
            }
        }
        other => Err(format!(
            "mcp: unknown subcommand '{other}' \
             (try: list, add, remove, reload, tools, session, watch, resource)"
        )),
    }
}

fn parse_add(args: &[String]) -> Result<McpCommand, String> {
    let mut positional = Vec::new();
    let mut auth_env = None;
    let mut auth_header = None;
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--auth-env" => {
                auth_env = Some(iter.next().ok_or("--auth-env needs a value")?.clone());
            }
            "--auth-header" => {
                auth_header = Some(iter.next().ok_or("--auth-header needs a value")?.clone());
            }
            other if other.starts_with("--") => {
                return Err(format!("mcp add: unknown flag '{other}'"));
            }
            other => positional.push(other.to_string()),
        }
    }
    let name = positional
        .first()
        .ok_or("mcp add: needs <name> <url>")?
        .clone();
    let url = positional
        .get(1)
        .ok_or("mcp add: needs a <url>")?
        .clone();
    Ok(McpCommand::Add { name, url, auth_env, auth_header })
}

fn parse_session(args: &[String]) -> Result<McpCommand, String> {
    match args.first().map(String::as_str) {
        Some("list") | None => Ok(McpCommand::SessionList),
        Some("open") => {
            let server = args.get(1).ok_or("mcp session open: needs a server name")?;
            Ok(McpCommand::SessionOpen { server: server.clone() })
        }
        Some("close") => {
            let id = args.get(1).ok_or("mcp session close: needs a session id")?;
            Ok(McpCommand::SessionClose { id: id.clone() })
        }
        Some("info") => {
            let id = args.get(1).ok_or("mcp session info: needs a session id")?;
            Ok(McpCommand::SessionInfo { id: id.clone() })
        }
        Some(other) => Err(format!(
            "mcp session: unknown subcommand '{other}' (try: list, open, close, info)"
        )),
    }
}

/// A parsed MCP tool invocation (`<server> <tool> …`) — the dynamic command surface.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ToolInvocation {
    pub server: String,
    /// `None` for a bare `<server>` (or `<server> --help`) — surface help.
    pub tool: Option<String>,
    pub help: bool,
    pub json: bool,
    pub session_id: Option<String>,
    /// The raw `--args '<json>'` escape hatch, if given (bypasses per-flag mapping).
    pub raw_args: Option<String>,
    /// `--key value` and bare `--flag` pairs, in order, for schema-driven mapping.
    pub flags: Vec<(String, Option<String>)>,
}

/// Parse a line whose leading word is an installed server name into a [`ToolInvocation`]. The caller
/// checks `is_server(leading)` first. Returns `Err` for a malformed invocation.
pub(crate) fn parse_tool_invocation(line: &str) -> Option<Result<ToolInvocation, String>> {
    let tokens = tokenize_str(line).ok()?;
    if tokens.iter().any(|t| matches!(t, Token::Operator(_, _))) {
        return None; // operator-bearing lines fall through to Brush
    }
    // Two views of each word: the normal fully-dequoted form (`words`), and a form that strips ONLY
    // the outer quote layer (`raw_words`) so a single-quoted JSON `--args '{"k":"v"}'` keeps its inner
    // double-quotes (unquote_str would collapse them, yielding invalid JSON).
    let mut words = Vec::new();
    let mut raw_words = Vec::new();
    for t in tokens {
        if let Token::Word(s, _) = t {
            raw_words.push(strip_outer_quotes(&s));
            words.push(unquote_str(&s));
        }
    }
    let server = words.first()?.clone();

    let mut inv = ToolInvocation {
        server,
        tool: None,
        help: false,
        json: false,
        session_id: None,
        raw_args: None,
        flags: Vec::new(),
    };
    // Index-based walk so `--args` can pull the raw (outer-quote-only) form of the next word.
    let mut positional_seen = false;
    let mut i = 1;
    while i < words.len() {
        let w = &words[i];
        match w.as_str() {
            "--help" | "-h" => inv.help = true,
            "--json" => inv.json = true,
            "--args" => {
                i += 1;
                match raw_words.get(i) {
                    Some(v) => inv.raw_args = Some(v.clone()),
                    None => return Some(Err("--args needs a JSON value".into())),
                }
            }
            "--session-id" => {
                i += 1;
                match words.get(i) {
                    Some(v) => inv.session_id = Some(v.clone()),
                    None => return Some(Err("--session-id needs a value".into())),
                }
            }
            flag if flag.starts_with("--") => {
                let key = flag.trim_start_matches("--").to_string();
                // A following non-flag word is the value; otherwise it's a bare boolean flag.
                let value = match words.get(i + 1) {
                    Some(next) if !next.starts_with("--") => {
                        i += 1;
                        Some(words[i].clone())
                    }
                    _ => None,
                };
                inv.flags.push((key, value));
            }
            other if !positional_seen => {
                inv.tool = Some(other.to_string());
                positional_seen = true;
            }
            other => return Some(Err(format!("unexpected argument '{other}'"))),
        }
        i += 1;
    }
    Some(Ok(inv))
}

/// Strip only the OUTER matching quote pair from a raw token, preserving inner quotes. `'{"a":1}'` →
/// `{"a":1}`; `"x"` → `x`; unquoted stays as-is. Used for `--args` JSON where `unquote_str` would
/// over-strip the inner double-quotes.
fn strip_outer_quotes(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// The `mcp` manifest. `Subprocess` scope, `Confirm` policy at the top level is too coarse — most
/// subcommands are read-only. The authz gate is subcommand-aware (see `authz::resolve`), so the
/// top-level policy is `Allow` and the mutating subcommands (`add`/`remove`/`reload`/`session
/// open`/`close`) carry `Confirm` via their subcommand manifests.
pub(crate) fn manifests() -> Vec<crate::manifest::Manifest> {
    use crate::manifest::{AuthorizationPolicy, ExecutionScope, Manifest};
    let confirm = |name: &str, synopsis: &str| {
        Manifest::builtin(name, synopsis)
            .with_scope(ExecutionScope::Subprocess)
            .with_policy(AuthorizationPolicy::Confirm)
    };
    let allow = |name: &str, synopsis: &str| {
        Manifest::builtin(name, synopsis).with_scope(ExecutionScope::Subprocess)
    };
    let mut m = Manifest::builtin("mcp", "manage MCP servers, tools, and sessions")
        .with_scope(ExecutionScope::Subprocess)
        .with_help(
            "mcp list — configured servers\n\
             mcp add <name> <url> [--auth-env VAR] [--auth-header H] — install a server (outbound HTTP)\n\
             mcp remove <name> — remove a server\n\
             mcp reload [<name>] — re-read config and re-install (outbound HTTP)\n\
             mcp tools <server> — list a server's tools\n\
             mcp session list|open <server>|close <id>|info <id> — manage sessions\n\
             MCP-lite: HTTPS servers, tools only (resources/prompts/watch deferred).",
        );
    m.subcommands = vec![
        allow("list", "list configured MCP servers"),
        confirm("add", "install an MCP server (outbound HTTP)"),
        confirm("remove", "remove an MCP server"),
        confirm("reload", "re-read config and re-install (outbound HTTP)"),
        allow("tools", "list an MCP server's tools"),
        // `session` is Allow at this level (list/info are read-only, open/close are low-risk session
        // lifecycle). The actual outbound tool CALLS carry Confirm via the per-server manifest.
        allow("session", "manage MCP sessions"),
    ];
    vec![m]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(line: &str) -> McpCommand {
        classify(line).unwrap().unwrap()
    }

    #[test]
    fn classifies_management_subcommands() {
        assert_eq!(c("mcp list"), McpCommand::List);
        assert_eq!(c("mcp"), McpCommand::List);
        assert_eq!(
            c("mcp add github https://api.example/mcp"),
            McpCommand::Add {
                name: "github".into(),
                url: "https://api.example/mcp".into(),
                auth_env: None,
                auth_header: None
            }
        );
        assert_eq!(c("mcp remove github"), McpCommand::Remove { name: "github".into() });
        assert_eq!(c("mcp reload"), McpCommand::Reload { name: None });
        assert_eq!(c("mcp tools github"), McpCommand::Tools { server: "github".into() });
        assert_eq!(c("mcp watch github://repo/issues"), McpCommand::Watch { uri: "github://repo/issues".into() });
        assert_eq!(
            c("mcp resource info /mnt/mcp/x/y"),
            McpCommand::ResourceInfo { path: "/mnt/mcp/x/y".into() }
        );
        // `mcp watch` without a uri errors.
        assert!(classify("mcp watch").unwrap().is_err());
    }

    #[test]
    fn add_parses_auth_flags() {
        assert_eq!(
            c("mcp add gh https://x/mcp --auth-env GH_TOKEN --auth-header X-Key"),
            McpCommand::Add {
                name: "gh".into(),
                url: "https://x/mcp".into(),
                auth_env: Some("GH_TOKEN".into()),
                auth_header: Some("X-Key".into()),
            }
        );
    }

    #[test]
    fn session_subcommands() {
        assert_eq!(c("mcp session list"), McpCommand::SessionList);
        assert_eq!(c("mcp session"), McpCommand::SessionList);
        assert_eq!(c("mcp session open gh"), McpCommand::SessionOpen { server: "gh".into() });
        assert_eq!(c("mcp session close s1"), McpCommand::SessionClose { id: "s1".into() });
        assert_eq!(c("mcp session info s1"), McpCommand::SessionInfo { id: "s1".into() });
    }

    #[test]
    fn errors_on_missing_args() {
        assert!(classify("mcp add github").unwrap().is_err());
        assert!(classify("mcp remove").unwrap().is_err());
        assert!(classify("mcp session open").unwrap().is_err());
        assert!(classify("mcp frobnicate").unwrap().is_err());
    }

    #[test]
    fn non_mcp_and_piped_lines_are_none() {
        assert!(classify("echo mcp").is_none());
        assert!(classify("mcp list | grep x").is_none());
    }

    #[test]
    fn tool_invocation_parses_tool_and_flags() {
        let inv = parse_tool_invocation("github search --query rust --limit 5").unwrap().unwrap();
        assert_eq!(inv.server, "github");
        assert_eq!(inv.tool.as_deref(), Some("search"));
        assert_eq!(inv.flags, vec![
            ("query".into(), Some("rust".into())),
            ("limit".into(), Some("5".into())),
        ]);
    }

    #[test]
    fn tool_invocation_help_and_args_and_session() {
        let inv = parse_tool_invocation("gh --help").unwrap().unwrap();
        assert!(inv.help && inv.tool.is_none());

        // --args preserves single-quoted JSON verbatim (only the outer quotes are stripped, so inner
        // double-quotes survive — unlike normal word dequoting).
        let inv = parse_tool_invocation("gh call --args '{\"a\":1}' --json --session-id s2")
            .unwrap()
            .unwrap();
        assert_eq!(inv.tool.as_deref(), Some("call"));
        assert_eq!(inv.raw_args.as_deref(), Some("{\"a\":1}"));
        assert!(inv.json);
        assert_eq!(inv.session_id.as_deref(), Some("s2"));
    }

    #[test]
    fn tool_invocation_bare_boolean_flag() {
        let inv = parse_tool_invocation("gh list --verbose").unwrap().unwrap();
        assert_eq!(inv.flags, vec![("verbose".into(), None)]);
    }
}
