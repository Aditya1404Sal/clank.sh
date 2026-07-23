//! The `golem` cluster command + the `GolemCluster` injection seam.
//!
//! `golem` exposes the Golem *runtime* API subset — talking to a running cluster (agent listing,
//! oplog, status, interrupt/resume, connect, and shell-instance self-ops: oplog/rollback/fork). It
//! **excludes** build/push/deploy/upload (README:875). Like `mcp`/`grease`, `golem` is a Session-layer
//! interception whose durable backing (`golem:api` host bindings) lives in the `clank-agent` crate and
//! is injected as a [`GolemCluster`]; native / no-cluster yields a consistent honest error
//! (README:895).
//!
//! Two operations have **no golem-rust host primitive** and are honest-stubbed: `golem agent interrupt`
//! and `golem agent resume` (they exist only as status/oplog enum variants, never as callable funcs —
//! they're control-plane operations outside the guest host surface).

use brush_parser::{tokenize_str, unquote_str, Token};

/// A parsed `golem` command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GolemCommand {
    /// `golem agent list` — list running agents.
    AgentList,
    /// `golem agent oplog --type <t> [ctor flags] [-n <count>]` — an agent's oplog.
    AgentOplog { agent_type: String, ctor: Vec<(String, String)>, count: Option<u32> },
    /// `golem agent status --type <t> [ctor flags]` — an agent's status/metadata.
    AgentStatus { agent_type: String, ctor: Vec<(String, String)> },
    /// `golem agent interrupt <pid>` — honest-stubbed (no host primitive).
    AgentInterrupt { pid: String },
    /// `golem agent resume <pid>` — honest-stubbed (no host primitive).
    AgentResume { pid: String },
    /// `golem connect <agent-identity>` — inspect a running agent (oplog/files/status).
    Connect { identity: String },
    /// `golem oplog` — the shell instance's own oplog.
    Oplog,
    /// `golem rollback` — rewind the shell instance's state.
    Rollback,
    /// `golem fork` — fork the current shell instance.
    Fork,
}

/// Recognize a `golem` line. `None` when it isn't one (fall through to Brush); `Some(Err)` when it is
/// but doesn't parse. Operator-bearing lines fall through so the nested-context stub handles them.
pub(crate) fn classify(line: &str) -> Option<Result<GolemCommand, String>> {
    let tokens = tokenize_str(line).ok()?;
    if tokens.iter().any(|t| matches!(t, Token::Operator(_, _))) {
        return None;
    }
    let words: Vec<String> = tokens
        .iter()
        .filter_map(|t| match t {
            Token::Word(s, _) => Some(unquote_str(s)),
            Token::Operator(_, _) => None,
        })
        .collect();
    let first = words.first()?;
    if first != "golem" {
        return None;
    }
    Some(parse(&words[1..]))
}

fn parse(args: &[String]) -> Result<GolemCommand, String> {
    match args.first().map(String::as_str) {
        Some("agent") => parse_agent(&args[1..]),
        Some("connect") => {
            let identity = args.get(1).ok_or("golem connect: needs an agent identity")?;
            Ok(GolemCommand::Connect { identity: identity.clone() })
        }
        Some("oplog") => Ok(GolemCommand::Oplog),
        Some("rollback") => Ok(GolemCommand::Rollback),
        Some("fork") => Ok(GolemCommand::Fork),
        Some(other) => Err(format!(
            "golem: unknown subcommand '{other}' (try: agent, connect, oplog, rollback, fork)"
        )),
        None => Err("golem: needs a subcommand (try: agent, connect, oplog, rollback, fork)".to_string()),
    }
}

fn parse_agent(args: &[String]) -> Result<GolemCommand, String> {
    match args.first().map(String::as_str) {
        Some("list") => Ok(GolemCommand::AgentList),
        Some("interrupt") => {
            let pid = args.get(1).ok_or("golem agent interrupt: needs a <pid>")?;
            Ok(GolemCommand::AgentInterrupt { pid: pid.clone() })
        }
        Some("resume") => {
            let pid = args.get(1).ok_or("golem agent resume: needs a <pid>")?;
            Ok(GolemCommand::AgentResume { pid: pid.clone() })
        }
        Some("oplog") => {
            let (agent_type, ctor, count) = parse_type_ctor_count(&args[1..])?;
            Ok(GolemCommand::AgentOplog { agent_type, ctor, count })
        }
        Some("status") => {
            let (agent_type, ctor, _count) = parse_type_ctor_count(&args[1..])?;
            Ok(GolemCommand::AgentStatus { agent_type, ctor })
        }
        Some(other) => Err(format!(
            "golem agent: unknown subcommand '{other}' \
             (try: list, new, oplog, status, interrupt, resume)"
        )),
        None => Err("golem agent: needs a subcommand".to_string()),
    }
}

/// Parse `--type <t> [--<ctor> val …] [-n <count>]` shared by oplog/status.
#[allow(clippy::type_complexity)]
fn parse_type_ctor_count(
    args: &[String],
) -> Result<(String, Vec<(String, String)>, Option<u32>), String> {
    let mut agent_type = None;
    let mut ctor = Vec::new();
    let mut count = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--type" => {
                agent_type = Some(it.next().ok_or("--type needs a value")?.clone());
            }
            "-n" => {
                let v = it.next().ok_or("-n needs a value")?;
                count = Some(v.parse::<u32>().map_err(|_| format!("bad -n value '{v}'"))?);
            }
            flag if flag.starts_with("--") => {
                let key = &flag[2..];
                let val = it.next().ok_or_else(|| format!("--{key} needs a value"))?;
                ctor.push((key.to_string(), val.clone()));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }
    let agent_type = agent_type.ok_or("needs --type <agent-type>")?;
    Ok((agent_type, ctor, count))
}

/// The static `golem` manifest with per-subcommand authorization. Read-only inspection (list/oplog/
/// status/connect) is `Allow`; state-changing ops (rollback/fork/interrupt/resume) are `Confirm`.
pub(crate) fn manifests() -> Vec<crate::manifest::Manifest> {
    use crate::manifest::{AuthorizationPolicy, ExecutionScope, Manifest};
    let allow = |name: &str, syn: &str| {
        Manifest::builtin(name, syn).with_scope(ExecutionScope::Subprocess)
    };
    let confirm = |name: &str, syn: &str| {
        Manifest::builtin(name, syn)
            .with_scope(ExecutionScope::Subprocess)
            .with_policy(AuthorizationPolicy::Confirm)
    };
    let mut m = Manifest::builtin("golem", "interact with the Golem cluster (runtime API subset)")
        .with_scope(ExecutionScope::Subprocess)
        .with_help(
            "golem agent list — list running agents\n\
             golem agent oplog --type <t> [--<ctor> v] [-n <count>] — an agent's oplog\n\
             golem agent status --type <t> [--<ctor> v] — an agent's status\n\
             golem agent interrupt <pid> | resume <pid> — (no host primitive; honest error)\n\
             golem connect <identity> — inspect a running agent\n\
             golem oplog | rollback | fork — shell-instance self-operations\n\
             Excludes build/push/deploy/upload (not a development tool). Needs a configured cluster.",
        );
    m.subcommands = vec![
        allow("agent", "agent list/oplog/status/interrupt/resume"),
        allow("connect", "inspect a running agent"),
        allow("oplog", "the shell instance's own oplog"),
        confirm("rollback", "rewind the shell instance state"),
        confirm("fork", "fork the current shell instance"),
    ];
    vec![m]
}

/// The injected Golem cluster interface (durable `golem:api` bindings on the agent; a fake in tests).
/// `?Send` (wasip2 single-threaded), `dyn`-compatible.
#[async_trait::async_trait(?Send)]
pub trait GolemCluster {
    /// List running agents (rendered text).
    async fn agent_list(&self) -> Result<String, String>;
    /// An agent's oplog (most-recent `count` entries; `None` = a default).
    async fn agent_oplog(&self, agent_type: &str, ctor: &[(String, String)]) -> Result<String, String>;
    /// An agent's status/metadata.
    async fn agent_status(&self, agent_type: &str, ctor: &[(String, String)]) -> Result<String, String>;
    /// Inspect a running agent by identity.
    async fn connect(&self, identity: &str) -> Result<String, String>;
    /// The shell instance's own oplog.
    async fn self_oplog(&self) -> Result<String, String>;
    /// Rewind the shell instance state.
    async fn rollback(&self) -> Result<String, String>;
    /// Fork the current shell instance.
    async fn fork(&self) -> Result<String, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(line: &str) -> GolemCommand {
        classify(line).unwrap().unwrap()
    }

    #[test]
    fn classifies_subcommands() {
        assert_eq!(c("golem agent list"), GolemCommand::AgentList);
        assert_eq!(c("golem fork"), GolemCommand::Fork);
        assert_eq!(c("golem oplog"), GolemCommand::Oplog);
        assert_eq!(c("golem rollback"), GolemCommand::Rollback);
        assert_eq!(
            c("golem connect shopping-cart:jd"),
            GolemCommand::Connect { identity: "shopping-cart:jd".into() }
        );
        assert_eq!(
            c("golem agent interrupt 42"),
            GolemCommand::AgentInterrupt { pid: "42".into() }
        );
        assert_eq!(
            c("golem agent status --type ShoppingCart --userid jd"),
            GolemCommand::AgentStatus {
                agent_type: "ShoppingCart".into(),
                ctor: vec![("userid".into(), "jd".into())]
            }
        );
        assert_eq!(
            c("golem agent oplog --type ShoppingCart --userid jd -n 50"),
            GolemCommand::AgentOplog {
                agent_type: "ShoppingCart".into(),
                ctor: vec![("userid".into(), "jd".into())],
                count: Some(50)
            }
        );
    }

    #[test]
    fn non_golem_and_operator_and_error_lines() {
        assert!(classify("echo golem").is_none());
        assert!(classify("golem list | head").is_none()); // operator → fall through
        assert!(classify("golem frobnicate").unwrap().is_err());
        assert!(classify("golem agent oplog").unwrap().is_err()); // missing --type
        assert!(classify("golem connect").unwrap().is_err()); // missing identity
    }
}
