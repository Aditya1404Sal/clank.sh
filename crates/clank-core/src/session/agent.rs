//! `Session` methods for Golem agent invocation (`<agent> [flags] <method>`) and the `golem`
//! cluster command. Invocation parsing (`parse_agent_line`) + types live in `super` (mod.rs).

use super::{Session, prompt_leading_word, LineResult, parse_agent_line, ParsedAgentLine, PendingInvocation};

/// The most fire-and-forget (`--trigger`/`--schedule`) invocations tracked at once. Beyond this the
/// oldest is presumed complete and reaped — see [`Session::spawn_agent_invocation_row`].
const MAX_PENDING_INVOCATIONS: usize = 64;

impl Session {
    /// Whether `line`'s leading word is an installed Golem agent. Drives the `run_agent` dispatch.
    /// Top-level only (a remote invocation awaits under the Session reactor; not reachable from Brush's
    /// nested runtime — the Wall-C wall).
    pub(super) fn is_agent_line(&self, line: &str) -> bool {
        let Some(word) = prompt_leading_word(line) else {
            return false;
        };
        self.grease.is_agent(&word)
    }

    /// Run an installed Golem agent: parse the ctor/wrapper-flags/method/args, validate the method (or
    /// dispatch a reserved subcommand), and invoke it via the injected wRPC invoker in the selected mode
    /// (await / trigger / schedule). Missing invoker → honest "needs a cluster"; unknown method → exit 2.
    pub(super) async fn run_agent(&mut self, line: &str) -> LineResult {
        let words = match crate::ai::ask::dequote_words(line) {
            Some(w) => w,
            None => return LineResult::from_outcome(Vec::new(), b"agent: parse error\n".to_vec(), 2),
        };
        // Strip a leading sudo (gate already resolved).
        let rest = if words.first().map(String::as_str) == Some("sudo") { &words[1..] } else { &words[..] };
        let name = rest[0].clone();
        let Some(pkg) = self.grease.agent(&name).cloned() else {
            return LineResult::denied(); // is_agent_line gated it
        };

        let parsed = match parse_agent_line(&rest[1..], &pkg) {
            Ok(p) => p,
            Err(msg) => return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 2),
        };

        // `--revision` has no wasm-rpc constructor slot — it's a golem:api concept (component-revision
        // targeting). Honest limitation rather than a silently-ignored flag.
        if parsed.revision.is_some() {
            return LineResult::from_outcome(
                Vec::new(),
                format!(
                    "{name}: --revision targeting is not supported on this SDK surface \
                     (component-revision selection is a golem:api concern; the invocation targets the \
                     running revision)\n"
                )
                .into_bytes(),
                2,
            );
        }

        // `--help` or no method → agent help.
        if parsed.method.is_empty() {
            let help = self.grease.pkg_help(&name).unwrap_or_default();
            return LineResult::continue_with_stdout(help.into_bytes());
        }

        // Reserved subcommands (README:832) — cannot be method names.
        match parsed.method.as_str() {
            "oplog" | "status" if pkg.ephemeral => {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("{name}: '{}' is not available for an ephemeral agent type\n", parsed.method)
                        .into_bytes(),
                    2,
                );
            }
            "oplog" => return self.run_agent_reserved(&name, &pkg, &parsed, "oplog").await,
            "status" => return self.run_agent_reserved(&name, &pkg, &parsed, "status").await,
            "stream" | "repl" => {
                // Long-lived/interactive — the durable agent serializes invocations and can't park on a
                // stream/REPL loop (same constraint as `ask repl`). Honest pointer.
                return LineResult::from_outcome(
                    Vec::new(),
                    format!(
                        "{name}: '{}' (interactive/streaming) is a native-terminal feature; the durable \
                         agent can't hold a long-lived session (Golem serializes invocations). Use the \
                         await/--trigger/--schedule invocation modes instead.\n",
                        parsed.method
                    )
                    .into_bytes(),
                    2,
                );
            }
            // The bare reserved word `help` (README:840) prints the agent's generated help, same as the
            // `--help` flag and the empty-method form above — it must not be treated as a method name.
            "help" => {
                let help = self.grease.pkg_help(&name).unwrap_or_default();
                return LineResult::continue_with_stdout(help.into_bytes());
            }
            _ => {}
        }

        if pkg.method(&parsed.method).is_none() {
            return LineResult::from_outcome(
                Vec::new(),
                format!("{name}: unknown method '{}' (try `{name} --help`)\n", parsed.method).into_bytes(),
                2,
            );
        }

        let inv = crate::golem::agent::AgentInvocation {
            agent_type: pkg.agent_type.clone(),
            constructor: parsed.constructor,
            method: parsed.method,
            args: parsed.args,
            mode: parsed.mode.clone(),
            phantom: parsed.phantom,
        };

        let Some(invoker) = self.agent_invoker.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                format!(
                    "{name}: Golem agent invocation requires a configured cluster \
                     (unavailable on this target)\n"
                )
                .into_bytes(),
                4,
            );
        };

        // Structured audit event for the Golem invocation (README:627): agent type, ordered constructor
        // params, method, mode, and phantom UUID. Logged to mcp.log (the outbound-call log) with the
        // agent identity so it correlates with Golem cluster logs. (revision + await-mode idempotency-key
        // have no fields yet — honest-stubbed / not surfaced by the SDK on this path.)
        let ctor = inv
            .constructor
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        let mode = match &inv.mode {
            crate::golem::agent::InvokeMode::Await => "await".to_string(),
            crate::golem::agent::InvokeMode::Trigger => "trigger".to_string(),
            crate::golem::agent::InvokeMode::Schedule(when) => format!("schedule:{when}"),
        };
        crate::logging::Record::new("agent-invoke")
            .field("type", &inv.agent_type)
            .field("ctor", &ctor)
            .field("method", &inv.method)
            .field("mode", &mode)
            .field("phantom", inv.phantom.as_deref().unwrap_or(""))
            .emit(crate::logging::LogFile::Mcp);

        match parsed.mode {
            crate::golem::agent::InvokeMode::Await => match invoker.invoke(&inv).await {
                Ok(result) => {
                    let mut out = result.into_bytes();
                    if !out.ends_with(b"\n") {
                        out.push(b'\n');
                    }
                    LineResult::continue_with_stdout(out)
                }
                Err(e) => LineResult::from_outcome(Vec::new(), format!("{name}: {e}\n").into_bytes(), 1),
            },
            crate::golem::agent::InvokeMode::Trigger | crate::golem::agent::InvokeMode::Schedule(_) => {
                // Fire-and-forget / deferred: returns a handle (+ a PID row for `ps`/`kill`).
                match invoker.invoke_async(&inv).await {
                    Ok(handle) => {
                        // Spawn an S-state proc row for the pending invocation (README: all modes return
                        // a PID); retain the cancel token so `kill <pid>` can cancel it.
                        let pid =
                            self.spawn_agent_invocation_row(line, &inv, handle.cancel_token.clone());
                        LineResult::continue_with_stdout(
                            format!("[{pid}] {name}: {}\n", handle.note).into_bytes(),
                        )
                    }
                    Err(e) => LineResult::from_outcome(Vec::new(), format!("{name}: {e}\n").into_bytes(), 1),
                }
            }
        }
    }

    /// Dispatch a `golem` cluster command through the injected [`crate::golem::cluster::GolemCluster`] seam.
    /// `interrupt`/`resume` are honest-stubbed (no host primitive); everything else needs a configured
    /// cluster (honest error otherwise).
    pub(super) async fn run_golem(&mut self, cmd: crate::golem::cluster::GolemCommand) -> LineResult {
        use crate::golem::cluster::GolemCommand as G;
        // interrupt/resume have no golem-rust host func — report honestly regardless of cluster.
        if let G::AgentInterrupt { pid } | G::AgentResume { pid } = &cmd {
            let verb = if matches!(cmd, G::AgentInterrupt { .. }) { "interrupt" } else { "resume" };
            return LineResult::from_outcome(
                Vec::new(),
                format!(
                    "golem agent {verb}: not available on this SDK surface — agent {verb} (pid {pid}) \
                     is a Golem control-plane operation with no guest host binding\n"
                )
                .into_bytes(),
                2,
            );
        }
        let Some(cluster) = self.golem_cluster.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"golem: requires a configured Golem cluster (unavailable on this target)\n".to_vec(),
                4,
            );
        };
        let result = match cmd {
            G::AgentList => cluster.agent_list().await,
            G::AgentOplog { agent_type, ctor, .. } => cluster.agent_oplog(&agent_type, &ctor).await,
            G::AgentStatus { agent_type, ctor } => cluster.agent_status(&agent_type, &ctor).await,
            G::Connect { identity } => cluster.connect(&identity).await,
            G::Oplog => cluster.self_oplog().await,
            G::Rollback => cluster.rollback().await,
            G::Fork => cluster.fork().await,
            G::AgentInterrupt { .. } | G::AgentResume { .. } => unreachable!(),
        };
        match result {
            Ok(text) => {
                let mut out = text.into_bytes();
                if !out.ends_with(b"\n") {
                    out.push(b'\n');
                }
                LineResult::continue_with_stdout(out)
            }
            Err(e) => LineResult::from_outcome(Vec::new(), format!("golem: {e}\n").into_bytes(), 1),
        }
    }

    /// Run an agent's reserved `oplog`/`status` subcommand via the golem cluster seam (the injected
    /// `AgentInvoker` also fronts these). v1 routes them through a dedicated cluster call; without a
    /// cluster it's the honest error.
    async fn run_agent_reserved(
        &mut self,
        name: &str,
        pkg: &crate::grease::pkg::AgentPackage,
        parsed: &ParsedAgentLine,
        sub: &str,
    ) -> LineResult {
        let Some(cluster) = self.golem_cluster.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                format!(
                    "{name}: '{sub}' requires a configured Golem cluster (unavailable on this target)\n"
                )
                .into_bytes(),
                4,
            );
        };
        let ctor: Vec<(String, String)> = parsed.constructor.clone();
        let result = match sub {
            "oplog" => cluster.agent_oplog(&pkg.agent_type, &ctor).await,
            "status" => cluster.agent_status(&pkg.agent_type, &ctor).await,
            _ => Err("unknown reserved subcommand".to_string()),
        };
        match result {
            Ok(text) => LineResult::continue_with_stdout(text.into_bytes()),
            Err(e) => LineResult::from_outcome(Vec::new(), format!("{name}: {sub}: {e}\n").into_bytes(), 1),
        }
    }

    /// Spawn an `S`-state `AgentInvocation` proc row for a triggered/scheduled invocation, and record
    /// its cancel token so `kill <pid>` can cancel it. Returns the PID.
    fn spawn_agent_invocation_row(
        &mut self,
        line: &str,
        inv: &crate::golem::agent::AgentInvocation,
        cancel_token: Option<String>,
    ) -> u32 {
        let argv: Vec<String> = line.split_whitespace().map(String::from).collect();
        // The agent identity `/proc/<pid>/status` will surface (README:252-258).
        let agent_params = inv
            .constructor
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        let meta = crate::runtime::proctable::AgentMeta {
            agent_type: inv.agent_type.clone(),
            agent_params,
            phantom_uuid: inv.phantom.clone(),
        };
        let mut table = self.proc_table.lock().unwrap();
        let pid = table.spawn_bg(
            crate::runtime::process::ProcessKind::AgentInvocation,
            argv,
            crate::runtime::proctable::SHELL_ROOT_PID,
        );
        table.set_agent_meta(pid, meta);
        drop(table);
        self.pending_invocations.push(PendingInvocation { pid, cancel_token });
        // Bound the fire-and-forget tracking. A `--trigger`/`--schedule` invocation has no
        // remote-completion signal, so without this its `S` row and `pending_invocations` entry would
        // linger forever — unbounded growth in a long-lived durable agent. When the tracked set
        // exceeds the cap, presume the OLDEST invocation done and reap it: complete its row (`S → Z`,
        // which the proc-table prune then bounds) and drop it from tracking. Deterministic (oldest
        // first), so oplog replay converges to the same bounded state. `kill` on an evicted pid then
        // reports "already dispatched", which is the honest answer for a fire-and-forget call.
        while self.pending_invocations.len() > MAX_PENDING_INVOCATIONS {
            let old = self.pending_invocations.remove(0);
            self.proc_table.lock().unwrap().complete(old.pid);
        }
        pid
    }
}
