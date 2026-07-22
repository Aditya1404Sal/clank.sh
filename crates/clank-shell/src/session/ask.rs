//! `Session` methods for the AI layer: the `ask` command + agentic tool loop, `ask repl`,
//! `context summarize`/auto-compaction summarization, and model resolution. A few core helpers
//! (`shell_home`, `resolve_authz`, `mcp_help_for`) ride along here and are re-exported to `super`.

use super::*;

/// Read-only Brush builtins the model may call as tools even though clank keeps no manifest for them.
/// They don't mutate parent-shell state, so the model-tool scope gate allows them explicitly rather
/// than default-denying (see `run_ask`'s scope check). Everything else unregistered is refused.
const MODEL_SAFE_BUILTINS: &[&str] = &["echo", "pwd", "test", "[", "true", "false", ":"];

impl Session {
    /// The execution scope of `name` across all manifest sources — the static registry, then MCP
    /// servers, then grease packages (the same order [`Self::resolve_authz`] uses). `None` if no
    /// manifest exists for it anywhere.
    fn resolve_command_scope(&self, name: &str) -> Option<crate::manifest::ExecutionScope> {
        if let Some(m) = self.registry.get(name) {
            return Some(m.execution_scope);
        }
        if let Some(m) = self.mcp.manifest_for(name) {
            return Some(m.execution_scope);
        }
        if let Some(m) = self.grease.manifest_for(name) {
            return Some(m.execution_scope);
        }
        None
    }

    /// Run the agentic `ask` loop: assemble the transcript-as-context first user message, then drive
    /// turns with the injected provider. In A1 the tool set is empty, so the model always answers in
    /// one turn (behavior-identical to the pre-loop `ask`); A2 adds the `shell` tool and makes this a
    /// real tool-calling loop. If no provider is installed (e.g. the native build), degrade to a clean
    /// "not configured" error (exit 4) rather than panicking — the README's "features that require
    /// Golem fail with informative errors."
    ///
    /// Takes `&mut self` because a tool call re-enters `run_command`. The provider is `take()`n for the
    /// duration and **restored before every return** — an early return that forgets to restore would
    /// silently break the next `ask`.
    ///
    /// `blanket_authorized` is the `sudo ask` (or session `allow_all`) grant: when set, the model's
    /// tool calls that hit a `confirm`-policy command run without a per-call refusal. It never
    /// satisfies `sudo-only` (destructive ops still refuse), matching [`authz::decide`].
    pub(super) async fn run_ask(
        &mut self,
        mut args: crate::ai::ask::AskArgs,
        blanket_authorized: bool,
    ) -> LineResult {
        use crate::ai::ask::AskTurn;

        // Pick up any pipeline stdin captured for this dispatch (`cat x | ask "…"`). Taken so it
        // never leaks into an unrelated later `ask`.
        if let Some(stdin) = self.next_ask_stdin.take() {
            args.stdin = Some(stdin);
        }

        if self.ask_provider.is_none() {
            return LineResult::from_outcome(
                Vec::new(),
                b"ask: no model provider configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        }

        // The base context is the same bytes `context show` renders — "the AI reads exactly what you
        // see." `--fresh` sends no transcript. Rendered to an owned String *before* the loop so the
        // transcript lock is never held across an await.
        let base_transcript = if args.fresh {
            String::new()
        } else {
            String::from_utf8_lossy(&self.transcript.lock().unwrap().render()).into_owned()
        };

        // Resolve the model: `--model` > ask.toml default > built-in DEFAULT_MODEL. Strip the
        // `anthropic/` prefix for the provider (it wants the bare id); an unknown provider prefix is
        // an error before any model call.
        let (model, model_warning) = match self.resolve_ask_model(args.model.as_deref()) {
            Ok(pair) => pair,
            Err(msg) => return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 2),
        };

        let mut trace = Vec::new();
        if let Some(w) = model_warning {
            trace.extend_from_slice(w.as_bytes());
        }

        // The tool surface = the generic shell/prompt_user tools, plus one ToolDefinition per installed
        // MCP tool (mcp__<server>__<tool>) and per installed grease prompt (prompt__<name>). Every
        // `grease install`/`mcp add` thus expands what `ask` can do (README).
        let mut tools = crate::ai::ask::build_ask_tools(&self.registry);
        tools.extend(self.mcp.ask_tool_definitions());
        tools.extend(self.grease.ask_tool_definitions());

        let system = crate::ai::ask::with_json_addendum(
            crate::ai::ask::build_system_prompt_with_capabilities(
                &self.registry,
                &self.mcp,
                &self.grease,
            ),
            args.json,
        );
        let state = AskLoopState {
            system,
            tools,
            history: vec![AskTurn::User(crate::ai::ask::user_content_with_stdin(
                &base_transcript,
                &args.prompt,
                args.stdin.as_deref(),
            ))],
            model,
            trace,
            blanket_authorized,
            json: args.json,
        };
        // `resume` is empty on a fresh loop; the pid carries the paused row on a resume.
        self.drive_ask_loop(state, Vec::new(), None).await
    }

    /// Handle an ask-tail pipeline (`… | ask "q"`): run the upstream, capture its stdout, and
    /// dispatch the `ask` tail at the session layer with those bytes as stdin. `ask` is Confirm-gated
    /// (outbound HTTP), so a bare tail surfaces a confirmation (carrying the captured stdin so it
    /// survives the pause); `… | sudo ask` pre-authorizes. The upstream runs through the normal
    /// `execute` capture path — its own commands carry whatever policy they have (`cat`/`grep` are
    /// Allow); this gates only the `ask` itself.
    pub(super) async fn run_ask_pipe(
        &mut self,
        pipe: crate::ai::ask::AskTailPipe,
        pid: Option<u32>,
    ) -> LineResult {
        let crate::ai::ask::AskTailPipe {
            upstream,
            args,
            elevated,
        } = pipe;

        // Authorize the ask tail FIRST (before running the upstream), so a denied ask doesn't run the
        // producer for nothing and matches the top-level `ask` gate. `ask` ⇒ Confirm; a `sudo` on the
        // tail (`elevated`) or a session "all" grant pre-authorizes.
        let policy = crate::manifest::AuthorizationPolicy::Confirm; // the `ask` manifest's policy
        let blanket = elevated || self.authz.allow_all;
        match authz::decide(policy, elevated, self.authz.allow_all) {
            Decision::Allow => {}
            Decision::Deny => {
                return self.finish_intercepted(pid, LineResult::denied());
            }
            Decision::Confirm { sudo_grant } => {
                // Capture the upstream now so the piped context is preserved across the pause, then
                // defer the ask with that stdin stashed on the pending confirmation.
                let captured = self.capture_upstream(&upstream).await;
                return self.surface_auth_confirm(
                    Some("ask"),
                    ask_reconstruct(&args),
                    pid,
                    sudo_grant,
                    Some(captured),
                    None,
                );
            }
        }

        // Approved (allow / sudo / all): run the upstream, capture, and dispatch the ask with stdin.
        let captured = self.capture_upstream(&upstream).await;
        self.next_ask_stdin = Some(captured);
        let result = self.run_ask(args, blanket).await;
        if let Some(pid) = pid {
            self.proc_table.lock().unwrap().complete(pid);
        }
        self.transcript.lock().unwrap().record_output(&result.terminal_output());
        result
    }

    /// Run an upstream pipeline stage and return its stdout as a lossy String (the stdin payload for
    /// an ask-tail pipeline). A nonzero exit is not fatal — pipes feed whatever the producer emitted.
    /// The upstream is not recorded as its own transcript line (the whole `… | ask` line is recorded
    /// by the caller).
    async fn capture_upstream(&mut self, upstream: &str) -> String {
        let result = self.execute(upstream).await;
        String::from_utf8_lossy(&result.stdout).into_owned()
    }

    // ---- ask repl (native-only interactive session with its own transcript) --------------------

    /// Start an `ask repl` session: resolve the model, seed the isolated transcript (empty for
    /// `--fresh`, a copy of the parent for `--inherit`), and stash it on `self.repl`. Returns the
    /// resolved model id for the prompt banner, or an `Err` message (unknown provider / no provider).
    /// Native-only — the durable agent returns an honest message from `eval_line` instead.
    pub fn repl_start(&mut self, args: &crate::ai::ask::ReplArgs) -> Result<String, String> {
        if self.ask_provider.is_none() {
            return Err("ask repl: no model provider configured\n".to_string());
        }
        let (model, _warning) = self.resolve_ask_model(args.model.as_deref())?;
        let transcript = match args.seed {
            crate::ai::ask::ReplSeed::Fresh => Transcript::new(),
            crate::ai::ask::ReplSeed::Inherit => self.transcript.lock().unwrap().clone(),
        };
        self.repl = Some(ReplState { transcript, model });
        Ok(self.repl.as_ref().unwrap().model.clone())
    }

    /// The active REPL's model id, for the `[model]>` prompt. `None` if no REPL is active.
    pub fn repl_model(&self) -> Option<String> {
        self.repl.as_ref().map(|r| r.model.clone())
    }

    /// Handle a REPL meta-command (`:model <id>`, `:new-session`, `:exit`). Returns `Some(output)`
    /// for a handled meta-command (the bool is whether the REPL should exit), or `None` if `line`
    /// isn't a meta-command (the caller then treats it as a prompt via [`Self::repl_turn`]).
    pub fn repl_meta(&mut self, line: &str) -> Option<(String, bool)> {
        let line = line.trim();
        let Some(repl) = self.repl.as_mut() else {
            return None;
        };
        let mut words = line.split_whitespace();
        match words.next()? {
            ":exit" | ":quit" => Some((String::new(), true)),
            ":new-session" => {
                repl.transcript = Transcript::new();
                Some(("(new session)\n".to_string(), false))
            }
            ":model" => match words.next() {
                Some(id) => {
                    // Accept `anthropic/…` or a bare id; store as given (resolved per-turn).
                    repl.model = id.strip_prefix("anthropic/").unwrap_or(id).to_string();
                    Some((format!("model set to {}\n", repl.model), false))
                }
                None => Some((format!("current model: {}\n", repl.model), false)),
            },
            other if other.starts_with(':') => {
                Some((format!("unknown REPL command: {other}\n"), false))
            }
            _ => None, // not a meta-command — it's a prompt
        }
    }

    /// Run one REPL turn: send the isolated transcript + `prompt` to the model, record the exchange
    /// into the REPL transcript, and return the reply text. Conversational only (no shell tools) —
    /// the REPL is a chat surface, distinct from agentic `ask`. Requires an active REPL.
    pub async fn repl_turn(&mut self, prompt: &str) -> String {
        use crate::ai::ask::AskTurn;
        let Some(repl) = self.repl.as_ref() else {
            return "ask repl: no active session\n".to_string();
        };
        let model = repl.model.clone();
        let context = String::from_utf8_lossy(&repl.transcript.render()).into_owned();

        let state = AskLoopState {
            system: crate::ai::ask::build_system_prompt_with_mcp(&self.registry, &self.mcp),
            tools: Vec::new(), // conversational: the REPL doesn't expose shell tools
            history: vec![AskTurn::User(crate::ai::ask::user_content(&context, prompt))],
            model,
            trace: Vec::new(),
            blanket_authorized: false,
            json: false,
        };
        let result = self.drive_ask_loop(state, Vec::new(), None).await;
        let reply = String::from_utf8_lossy(&result.stdout).into_owned();

        // Record the exchange into the REPL's own transcript so the next turn has context.
        if let Some(repl) = self.repl.as_mut() {
            repl.transcript.record_command(&format!("> {prompt}"));
            repl.transcript.record_output(reply.as_bytes());
        }
        reply
    }

    /// End the REPL session: render its transcript to stdout (so it enters the parent transcript once
    /// as rendered output, per the README) and clear `self.repl`. Returns the rendered session bytes.
    pub fn repl_end(&mut self) -> Vec<u8> {
        match self.repl.take() {
            Some(repl) => repl.transcript.render(),
            None => Vec::new(),
        }
    }

    // ---- context summarize (LLM one-shot; inspection only, never mutates the transcript) ---------

    /// `context summarize`: send the rendered transcript to the model and print an AI summary on
    /// stdout. A single tool-less provider `turn` (the [`Self::repl_turn`] shape, not the agentic
    /// `drive_ask_loop`). **Inspection only** — it does NOT mutate the transcript, and the caller
    /// must NOT record its output back (matching `context show`). Runs at the Session async layer
    /// (like `ask`), so it's only reachable from a top-level `context summarize` line — a nested one
    /// (`$(...)`/pipe) hits the honest error in `apply_context`.
    pub(super) async fn run_context_summarize(&mut self) -> LineResult {
        // Render before awaiting so the transcript lock is never held across the model call.
        let rendered = String::from_utf8_lossy(&self.transcript.lock().unwrap().render()).into_owned();
        if rendered.trim().is_empty() {
            return LineResult::from_outcome(b"(transcript is empty)\n".to_vec(), Vec::new(), 0);
        }

        let (model, _warning) = match self.resolve_ask_model(None) {
            Ok(pair) => pair,
            Err(msg) => return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 2),
        };

        match self.summarize_text(&rendered, &model).await {
            Ok(None) => LineResult::from_outcome(
                Vec::new(),
                b"context summarize: no model provider configured (available on the Golem agent)\n"
                    .to_vec(),
                4,
            ),
            Ok(Some(summary)) => {
                let mut text = summary.into_bytes();
                if !text.ends_with(b"\n") {
                    text.push(b'\n');
                }
                LineResult::from_outcome(text, Vec::new(), 0)
            }
            Err(err) => LineResult::from_outcome(Vec::new(), err.into_bytes(), 4),
        }
    }

    /// One tool-less provider `turn` that summarizes `text` and returns the model's reply. `Ok(None)`
    /// when no provider is configured (native), `Err` on a model error. The shared mechanic behind both
    /// `context summarize` and the auto-compaction step: render/text is prepared by the caller (so the
    /// transcript lock is never held across the await), the provider is `take()`n and restored, and a
    /// single `SUMMARIZE_SYSTEM_PROMPT` turn is sent with no tools.
    async fn summarize_text(&mut self, text: &str, model: &str) -> Result<Option<String>, String> {
        use crate::ai::ask::AskTurn;

        let Some(provider) = self.ask_provider.take() else {
            return Ok(None);
        };
        let resp = provider
            .turn(
                Some(crate::ai::ask::SUMMARIZE_SYSTEM_PROMPT),
                &[AskTurn::User(text.to_string())],
                &[],
                model,
            )
            .await;
        self.ask_provider = Some(provider); // restore before returning

        match resp.error {
            Some(err) => Err(err),
            None => Ok(Some(resp.text)),
        }
    }

    /// The auto-compaction step: after a line's output is recorded and the window may have evicted old
    /// entries, upgrade the leading `[N earlier entries dropped]` count marker into a model-generated
    /// summary block. Runs at most once per line, only when eviction actually happened AND a provider is
    /// configured (agent, or a Fake in tests). On native / no provider / model error, the count marker is
    /// left as-is — the decided fallback, so recording never blocks or fails.
    pub(super) async fn compact_dropped_span(&mut self) {
        // Snapshot the pending dropped span without holding the lock across the await.
        let pending = self.transcript.lock().unwrap().pending_summary();
        let Some((_count, dropped_text)) = pending else {
            return;
        };
        if dropped_text.trim().is_empty() {
            return;
        }
        let model = match self.resolve_ask_model(None) {
            Ok((model, _warning)) => model,
            Err(_) => return, // no valid model → keep the count marker
        };
        if let Ok(Some(summary)) = self.summarize_text(&dropped_text, &model).await {
            self.transcript.lock().unwrap().set_marker_summary(summary);
        }
    }

    /// Resolve the model id `ask` should target: `--model` (if given) > the ask.toml default > the
    /// built-in [`crate::ai::ask::DEFAULT_MODEL`]. Returns `(bare_model_id, optional_warning)` — the
    /// `anthropic/` prefix is stripped for the provider. An unknown `provider/` prefix is an `Err`
    /// (surfaced before any model call). An ask.toml parse error is a non-fatal warning that falls
    /// back to the built-in default.
    fn resolve_ask_model(&self, cli_model: Option<&str>) -> Result<(String, Option<String>), String> {
        let mut warning = None;
        let chosen = if let Some(m) = cli_model {
            m.to_string()
        } else {
            let home = self.shell_home();
            match crate::ai::config::default_model(&home) {
                Ok(Some(m)) => m,
                Ok(None) => crate::ai::ask::DEFAULT_MODEL.to_string(),
                Err(e) => {
                    warning = Some(format!("ask: {e}; using the built-in default\n"));
                    crate::ai::ask::DEFAULT_MODEL.to_string()
                }
            }
        };

        // Validate/strip the provider prefix. Only `anthropic/` is known; a bare id is anthropic.
        let bare = match chosen.split_once('/') {
            Some(("anthropic", model)) => model.to_string(),
            Some((provider, _)) => {
                return Err(format!(
                    "ask: unknown provider '{provider}' (only anthropic is available)\n"
                ))
            }
            None => chosen,
        };
        Ok((bare, warning))
    }

    /// The shell's `$HOME` (seeded to `/home/user` on the agent), for locating `~/.config/ask/ask.toml`.
    fn shell_home(&self) -> String {
        self.shell
            .env()
            .get_str("HOME", &self.shell)
            .map(|h| h.into_owned())
            .unwrap_or_else(|| DEFAULT_HOME.to_string())
    }

    /// Resolve a line's authorization policy, consulting the static registry AND the dynamic MCP
    /// server manifests. Mirrors [`authz::resolve`] but adds the MCP layer: an installed server name
    /// (leading command) resolves to its `Confirm` manifest. Returns `(policy, elevated, command)`.
    pub(super) fn resolve_authz(
        &self,
        line: &str,
    ) -> (crate::manifest::AuthorizationPolicy, bool, Option<String>) {
        let (command, elevated) = authz::leading_command(line);
        if let Some(name) = command.as_deref() {
            if self.registry.get(name).is_none() {
                if let Some(m) = self.mcp.manifest_for(name) {
                    return (m.authorization_policy, elevated, command);
                }
                // An installed grease prompt: running it is an outbound LLM call ⇒ Confirm.
                if let Some(m) = self.grease.manifest_for(name) {
                    return (m.authorization_policy, elevated, command);
                }
            }
        }
        authz::resolve(&self.registry, line)
    }

    /// Resolve authorization for a whole line by its STRICTEST top-level command segment (README
    /// "Authorization" — a `confirm`/`sudo-only` command hidden behind `&&`/`;`/`|`/`&` after a
    /// harmless leading command must still gate). Splits the line with [`authz::split_segments`],
    /// resolves each segment with the MCP-aware [`Self::resolve_authz`], `decide`s each against
    /// `allow_all`, and returns the most-restrictive segment's `(policy, elevated, command)` — so the
    /// caller's existing `authz::decide(policy, elevated, allow_all)` reproduces the strictest
    /// `Decision` unchanged. Also returns every non-`Allow` command with its tier, so the confirmation
    /// can name all of them (approving the strictest runs the whole line).
    ///
    /// A single-segment line short-circuits to [`Self::resolve_authz`] — identical behavior, no
    /// overhead — so nothing changes for the overwhelmingly common plain command.
    pub(super) fn resolve_authz_strictest(
        &self,
        line: &str,
        allow_all: bool,
    ) -> (
        crate::manifest::AuthorizationPolicy,
        bool,
        Option<String>,
        Vec<(String, crate::manifest::AuthorizationPolicy)>,
    ) {
        let segments = authz::split_segments(line);
        if segments.len() <= 1 {
            let (policy, elevated, command) = self.resolve_authz(line);
            let gated = match authz::decide(policy, elevated, allow_all) {
                Decision::Allow => Vec::new(),
                _ => command
                    .clone()
                    .map(|c| vec![(c, policy)])
                    .unwrap_or_default(),
            };
            return (policy, elevated, command, gated);
        }

        // Multi-segment: resolve+decide each, track the strictest, and collect every gated command.
        let mut strictest: Option<(u8, crate::manifest::AuthorizationPolicy, bool, Option<String>)> =
            None;
        let mut gated: Vec<(String, crate::manifest::AuthorizationPolicy)> = Vec::new();
        for seg in segments {
            let (policy, elevated, command) = self.resolve_authz(seg);
            let decision = authz::decide(policy, elevated, allow_all);
            if !matches!(decision, Decision::Allow) {
                if let Some(name) = command.clone() {
                    gated.push((name, policy));
                }
            }
            let rank = authz::decision_rank(decision);
            if strictest.as_ref().map(|(r, ..)| rank > *r).unwrap_or(true) {
                strictest = Some((rank, policy, elevated, command));
            }
        }
        let (_, policy, elevated, command) = strictest.expect("split_segments never returns empty");
        (policy, elevated, command, gated)
    }

    /// Generated help for an MCP tool line ending in `--help` (or a bare `<server>`): the server's
    /// tool list. `None` if the line isn't an installed-server line or doesn't request help.
    pub(super) fn mcp_help_for(&self, line: &str) -> Option<String> {
        let inv = match crate::mcp::cmd::parse_tool_invocation(line)? {
            Ok(inv) => inv,
            Err(_) => return None,
        };
        if !self.mcp.is_server(&inv.server) {
            return None;
        }
        // Bare `<server>` or `--help` ⇒ server help; a `<tool> --help` ⇒ the same (tool-level help is
        // the server help in MCP-lite).
        if inv.help || inv.tool.is_none() {
            return self.mcp.server_help(&inv.server);
        }
        None
    }

    /// Drive the `ask` agentic loop from `state` until it completes (model answers, transport error,
    /// or the cap) or pauses for the human. `pending_results` are results already accumulated for the
    /// *current* assistant turn (non-empty only on resume, after a pause mid-batch); `pid` is the
    /// process-table row to pause on surfacing (the ask's own row, threaded through on resume).
    ///
    /// The provider is `take()`n and restored before every return. On a pause, the whole `state` is
    /// stashed in `PendingKind::AgentLoop` and a `pending_prompt` is returned; `answer_prompt` calls
    /// back into this helper to continue.
    async fn drive_ask_loop(
        &mut self,
        mut state: AskLoopState,
        mut pending_results: Vec<crate::ai::ask::AskToolResult>,
        pid: Option<u32>,
    ) -> LineResult {
        use crate::ai::ask::AskTurn;

        let Some(mut provider) = self.ask_provider.take() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"ask: no model provider configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };

        // Tool traces accumulate on `state.trace` (→ stderr); the final model text is stdout. Both land
        // in the transcript via the caller's `record_output` — AI tool lines are never first-class
        // transcript entries.
        let final_text;

        // Bound the number of *model* turns. Count assistant turns already in history so a resumed loop
        // doesn't restart the budget.
        let turns_taken = state
            .history
            .iter()
            .filter(|t| matches!(t, AskTurn::Assistant { .. }))
            .count();

        let mut turn = turns_taken;
        loop {
            if turn >= ASK_MAX_ITERATIONS {
                state.trace.extend_from_slice(
                    format!("[ask] tool-call limit ({ASK_MAX_ITERATIONS}) reached\n").as_bytes(),
                );
                self.ask_provider = Some(provider);
                if let Some(pid) = pid {
                    self.proc_table.lock().unwrap().complete(pid);
                }
                // Under `--json` a truncated loop can't have produced a validated JSON answer, so
                // honor the exit-6 contract rather than a bare exit 0.
                if state.json {
                    state.trace.extend_from_slice(
                        b"ask: --json: tool-call limit reached before a final JSON answer\n",
                    );
                    return LineResult::from_outcome(Vec::new(), state.trace, 6);
                }
                return LineResult::from_outcome(Vec::new(), state.trace, 0);
            }

            let resp = provider
                .turn(Some(&state.system), &state.history, &state.tools, &state.model)
                .await;

            if let Some(err) = resp.error {
                self.ask_provider = Some(provider);
                if let Some(pid) = pid {
                    self.proc_table.lock().unwrap().complete(pid);
                }
                let mut stderr = state.trace;
                stderr.extend_from_slice(err.as_bytes());
                return LineResult::from_outcome(Vec::new(), stderr, 4);
            }
            if resp.tool_calls.is_empty() {
                final_text = resp.text;
                break;
            }
            turn += 1;

            // Execute this turn's calls in order. A pause stashes the remaining calls + accumulated
            // results and returns immediately (non-blocking); `answer_prompt` resumes here.
            //
            // Restore the provider onto `self` for the duration of tool execution: a `prompt__<name>`
            // tool call re-enters `run_ask` (running the stored prompt through the model), which needs
            // the provider available. Re-take it after the batch, before the next `provider.turn`.
            self.ask_provider = Some(provider);
            let calls = resp.tool_calls.clone();
            let mut results = std::mem::take(&mut pending_results);
            for (i, call) in calls.iter().enumerate() {
                let step = Box::pin(self.execute_ask_tool(
                    call,
                    state.blanket_authorized,
                    &mut state.trace,
                ))
                .await;
                match step {
                    ToolStep::Done(r) => results.push(r),
                    ToolStep::Pause(kind) => {
                        // Record the assistant turn before pausing so the stashed history is complete.
                        state.history.push(AskTurn::Assistant {
                            text: resp.text.clone(),
                            tool_calls: calls.clone(),
                        });
                        let pause = AskPause {
                            call: call.clone(),
                            kind,
                            remaining: calls[i + 1..].to_vec(),
                            completed: results,
                        };
                        // Provider already restored on `self` above; leave it there for the resume.
                        return self.surface_agent_pause(state, pause, pid);
                    }
                }
            }

            state.history.push(AskTurn::Assistant {
                text: resp.text,
                tool_calls: calls,
            });
            state.history.push(AskTurn::ToolResults(results));

            // Re-take the provider for the next turn's `provider.turn` (it was restored on `self` for
            // tool execution above). A nested prompt tool call has already returned it to `self`.
            let Some(p) = self.ask_provider.take() else {
                if let Some(pid) = pid {
                    self.proc_table.lock().unwrap().complete(pid);
                }
                return LineResult::from_outcome(
                    Vec::new(),
                    b"ask: model provider went missing mid-loop\n".to_vec(),
                    4,
                );
            };
            provider = p;
        }

        self.ask_provider = Some(provider);
        if let Some(pid) = pid {
            self.proc_table.lock().unwrap().complete(pid);
        }

        // `--json`: enforce the output contract. Valid JSON (after stripping a stray code fence) ⇒
        // the JSON on stdout, exit 0, trace still on stderr. Otherwise exit 6 with the raw text on
        // stderr so it isn't lost (README: "raw model response emitted to stderr").
        if state.json {
            let candidate = crate::ai::ask::strip_json_fence(&final_text);
            match serde_json::from_str::<serde_json::Value>(candidate) {
                Ok(_) => {
                    return LineResult::from_outcome(
                        candidate.as_bytes().to_vec(),
                        state.trace,
                        0,
                    );
                }
                Err(_) => {
                    let mut stderr = state.trace;
                    stderr.extend_from_slice(b"ask: --json: model did not return valid JSON\n");
                    stderr.extend_from_slice(final_text.as_bytes());
                    if !final_text.ends_with('\n') {
                        stderr.push(b'\n');
                    }
                    return LineResult::from_outcome(Vec::new(), stderr, 6);
                }
            }
        }

        LineResult::from_outcome(final_text.into_bytes(), state.trace, 0)
    }

    /// Surface the human-facing prompt for a paused `ask` loop and stash the loop state. Returns a
    /// `pending_prompt` result; the shell does not block. For a `Confirm` pause the prompt is the
    /// README's authorization copy; for `prompt_user` it's the model's question.
    fn surface_agent_pause(
        &mut self,
        state: AskLoopState,
        pause: AskPause,
        pid: Option<u32>,
    ) -> LineResult {
        let prompt = match &pause.kind {
            AskPauseKind::Confirm {
                command,
                sudo_grant,
            } => {
                let name = authz::leading_command(command).0.unwrap_or_else(|| command.clone());
                let synopsis = format!("run `{command}`");
                PendingPrompt {
                    question: authz::confirm_question(&name, &synopsis, *sudo_grant),
                    choices: Some(authz::confirm_choices(*sudo_grant)),
                    secret: false,
                }
            }
            AskPauseKind::PromptUser => {
                // The model's question is the first user-facing text; re-derive it from the call args.
                let question = serde_json::from_str::<serde_json::Value>(&pause.call.arguments_json)
                    .ok()
                    .and_then(|v| v.get("question").and_then(|q| q.as_str()).map(String::from))
                    .unwrap_or_else(|| "the model has a question".to_string());
                PendingPrompt {
                    question,
                    choices: None,
                    secret: false,
                }
            }
        };
        self.surface_pending(
            prompt,
            pid,
            PendingKind::AgentLoop {
                state: Box::new(state),
                pause,
            },
        )
    }

    /// Resume a paused `ask` loop after the human answers. Resolves the paused tool call per its kind,
    /// drains any sibling calls from the same turn (each may pause again), then re-enters
    /// [`Session::drive_ask_loop`] to continue the conversation.
    pub(super) async fn resolve_agent_loop(
        &mut self,
        resolution: Resolution,
        mut state: Box<AskLoopState>,
        pause: AskPause,
        pid: Option<u32>,
    ) -> LineResult {
        use crate::ai::ask::{AskToolResult, AskTurn};

        // An abort (Ctrl-C, or `kill <paused-pid>`) terminates the WHOLE ask, not just this tool call:
        // exit 130 with the trace so far. The paused row is reaped here.
        if matches!(resolution, Resolution::Aborted) {
            if let Some(pid) = pid {
                self.proc_table.lock().unwrap().complete(pid);
            }
            state.trace.extend_from_slice(b"[ask] aborted by user\n");
            return LineResult::from_outcome(Vec::new(), state.trace, 130);
        }

        let answer_text = match &resolution {
            Resolution::Answered { stdout, .. } => {
                String::from_utf8_lossy(stdout).trim().to_string()
            }
            Resolution::Aborted => unreachable!("handled above"),
            Resolution::InvalidChoice { .. } => unreachable!("handled by answer_prompt"),
        };

        // Resolve the paused call.
        let resolved: AskToolResult = match pause.kind {
            AskPauseKind::Confirm {
                command,
                sudo_grant,
            } => {
                let approved = matches!(answer_text.as_str(), "yes" | "all");
                if answer_text == "all" && !sudo_grant {
                    // Blanket confirm-tier authorization for the rest of THIS loop only (not the
                    // session `allow_all`). Sudo-only never gets an "all".
                    state.blanket_authorized = true;
                }
                if approved {
                    Box::pin(self.run_shell_tool(
                        &pause.call,
                        &command,
                        state.blanket_authorized,
                        &mut state.trace,
                    ))
                    .await
                } else {
                    state
                        .trace
                        .extend_from_slice(format!("[tool] $ {command}\n[tool] denied by user\n").as_bytes());
                    AskToolResult {
                        id: pause.call.id.clone(),
                        name: pause.call.name.clone(),
                        outcome: Err("denied by user".into()),
                    }
                }
            }
            AskPauseKind::PromptUser => {
                let payload = serde_json::json!({ "answer": answer_text });
                AskToolResult {
                    id: pause.call.id.clone(),
                    name: pause.call.name.clone(),
                    outcome: Ok(payload.to_string()),
                }
            }
        };

        let mut results = pause.completed;
        results.push(resolved);

        // Drain the sibling calls from the same turn; any may pause again (re-stashing AgentLoop).
        for (i, call) in pause.remaining.iter().enumerate() {
            let step = Box::pin(self.execute_ask_tool(
                call,
                state.blanket_authorized,
                &mut state.trace,
            ))
            .await;
            match step {
                ToolStep::Done(r) => results.push(r),
                ToolStep::Pause(kind) => {
                    let next = AskPause {
                        call: call.clone(),
                        kind,
                        remaining: pause.remaining[i + 1..].to_vec(),
                        completed: results,
                    };
                    return self.surface_agent_pause(*state, next, pid);
                }
            }
        }

        // All calls in the paused turn are resolved: append their results and continue the loop.
        state.history.push(AskTurn::ToolResults(results));
        self.drive_ask_loop(*state, Vec::new(), pid).await
    }

    /// Attempt one tool call from the agentic loop. Returns either a finished [`AskToolResult`] (the
    /// call ran, was refused by a guard, or malformed) or a [`ToolStep::Pause`] when the call needs the
    /// human — a `confirm`/`sudo-only` command awaiting authorization, or the `prompt_user` tool.
    ///
    /// Guards return `Done(Err(..))` (loop continues): malformed/unknown tool, `ask` recursion, and any
    /// `shell-internal`/`parent-shell` command (`context`/`kill`/`cd`/`export`/… mutate shell state a
    /// tool must not reach — closes the cwd/env leak vector). The authorization pre-check reuses
    /// [`authz`]: `Allow` executes, `Confirm` pauses. Approved lines run through `run_command` (full
    /// session surface — `curl`/`wget` work) with no proc row; a nonzero exit is a *successful* tool
    /// result carrying the code (the model must see failures).
    async fn execute_ask_tool(
        &mut self,
        call: &crate::ai::ask::AskToolCall,
        blanket_authorized: bool,
        trace: &mut Vec<u8>,
    ) -> ToolStep {
        use crate::ai::ask::AskToolResult;

        let done_err = |msg: String| {
            ToolStep::Done(AskToolResult {
                id: call.id.clone(),
                name: call.name.clone(),
                outcome: Err(msg),
            })
        };

        // The `prompt_user` tool: pause and ask the human directly; the answer becomes the result.
        if call.name == crate::ai::ask::PROMPT_USER_TOOL {
            let question = match serde_json::from_str::<serde_json::Value>(&call.arguments_json) {
                Ok(v) => match v.get("question").and_then(|q| q.as_str()) {
                    Some(s) => s.to_string(),
                    None => {
                        return done_err(
                            "malformed arguments: missing string field 'question'".into(),
                        )
                    }
                },
                Err(e) => return done_err(format!("malformed arguments: {e}")),
            };
            trace.extend_from_slice(format!("[tool] prompt_user: {question}\n").as_bytes());
            return ToolStep::Pause(AskPauseKind::PromptUser);
        }

        // An MCP tool (name `mcp__<server>__<tool>`): decode to a `<server> <tool> --args '<json>'`
        // command line and route it through the same shell-tool machinery below (so its Confirm authz
        // pauses under a plain ask, or runs under `sudo ask`). The arguments_json is validated so the
        // `--args` payload is always well-formed JSON.
        let command = if let Some(rest) = call.name.strip_prefix("mcp__") {
            let Some((server, tool)) = rest.split_once("__") else {
                return done_err(format!("malformed MCP tool name '{}'", call.name));
            };
            // The whole arguments object is the tool's arguments (validate it's an object).
            let args_str = if call.arguments_json.trim().is_empty() {
                "{}".to_string()
            } else {
                match serde_json::from_str::<serde_json::Value>(&call.arguments_json) {
                    Ok(serde_json::Value::Object(_)) => call.arguments_json.clone(),
                    Ok(_) => return done_err("MCP tool arguments must be a JSON object".into()),
                    Err(e) => return done_err(format!("malformed arguments: {e}")),
                }
            };
            // Single-quote the JSON so the shell tokenizer preserves it (inner double-quotes survive
            // via the outer-quote-only handling in parse_tool_invocation).
            format!("{server} {tool} --args '{args_str}'")
        } else if let Some(name) = call.name.strip_prefix("prompt__") {
            // An installed grease prompt (`prompt__<name>`): decode the arguments object into
            // `--key value` flags and route the `<name> --flags` line through the shell-tool machinery
            // (so its Confirm authz pauses under a plain ask, runs under `sudo ask`; `run_prompt` fills
            // the body and dispatches to the model). Each value is single-quoted for the tokenizer.
            let args_val = if call.arguments_json.trim().is_empty() {
                serde_json::json!({})
            } else {
                match serde_json::from_str::<serde_json::Value>(&call.arguments_json) {
                    Ok(v @ serde_json::Value::Object(_)) => v,
                    Ok(_) => return done_err("prompt arguments must be a JSON object".into()),
                    Err(e) => return done_err(format!("malformed arguments: {e}")),
                }
            };
            let mut line = name.to_string();
            if let Some(obj) = args_val.as_object() {
                for (k, v) in obj {
                    // Render the value as a string (JSON strings unquoted; others via to_string).
                    let val = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let escaped = val.replace('\'', r"'\''");
                    line.push_str(&format!(" --{k} '{escaped}'"));
                }
            }
            line
        } else if call.name == crate::ai::ask::SHELL_TOOL {
            // Extract the `command` string from the tool arguments.
            match serde_json::from_str::<serde_json::Value>(&call.arguments_json) {
                Ok(v) => match v.get("command").and_then(|c| c.as_str()) {
                    Some(s) => s.to_string(),
                    None => {
                        return done_err(
                            "malformed arguments: missing string field 'command'".into(),
                        )
                    }
                },
                Err(e) => return done_err(format!("malformed arguments: {e}")),
            }
        } else {
            return done_err(format!("unknown tool '{}'", call.name));
        };

        // Guard: `ask` cannot call itself.
        if crate::ai::ask::classify(&command).is_some() {
            trace.extend_from_slice(format!("[tool] $ {command}\n[tool] refused: ask cannot call itself\n").as_bytes());
            return done_err("ask cannot call itself".into());
        }
        // Guard: shell-internal / parent-shell commands mutate state a subprocess-style tool can't
        // reach. Checked PER TOP-LEVEL SEGMENT, not just the leading command: `run_shell_tool` runs the
        // line on the SHARED Session (→ `run_command` → Brush), so a compound tool call like
        // `ls && cd /etc` must not smuggle a state-mutating builtin (`cd`/`export`/`unset`/`alias`)
        // past this gate behind a harmless leading command — it would take effect on the real shell.
        // (`prompt-user` as a bare shell line lands here too — ShellInternal, refused; the model
        // reaches the human through the dedicated `prompt_user` tool above instead.)
        use crate::manifest::ExecutionScope;
        for segment in authz::split_segments(&command) {
            let (_policy, _elevated, seg_cmd) = authz::resolve(&self.registry, segment);
            let Some(name) = seg_cmd.as_deref() else { continue };
            match self.resolve_command_scope(name) {
                // Isolated command — safe to run as a tool.
                Some(ExecutionScope::Subprocess) => {}
                // Mutates parent-shell state the isolated tool can't reach — refuse.
                Some(ExecutionScope::ShellInternal | ExecutionScope::ParentShell) => {
                    trace.extend_from_slice(
                        format!("[tool] $ {command}\n[tool] refused: shell-internal ({name})\n")
                            .as_bytes(),
                    );
                    return done_err(format!(
                        "{name} is a shell-internal command, not available as a tool; it mutates \
                         shell state ask cannot access"
                    ));
                }
                // Not in any manifest, but a known read-only Brush builtin the model legitimately
                // uses (echo/test/pwd/…) — allow.
                None if MODEL_SAFE_BUILTINS.contains(&name) => {}
                // Truly unknown — DEFAULT-DENY. clank can't establish it's subprocess-safe, and an
                // unregistered state-mutator must not slip through the model-tool boundary just
                // because it has no manifest. (The human path is unaffected — this gate is model-only.)
                None => {
                    trace.extend_from_slice(
                        format!("[tool] $ {command}\n[tool] refused: unknown command ({name})\n")
                            .as_bytes(),
                    );
                    return done_err(format!(
                        "{name} is not a recognized command; only known subprocess-safe commands may \
                         be called as tools"
                    ));
                }
            }
        }

        // Authorization pre-check (MCP-aware — an MCP tool line resolves to its Confirm manifest).
        // `command` has no leading `sudo` (a model line won't carry it), so elevation is driven
        // entirely by `blanket_authorized` (confirm-tier only). Gates on the STRICTEST segment so a
        // compound model tool call (`echo ok && rm -rf /x`) is caught on `rm`, not the leading `echo`.
        let (policy, elevated, _, _gated) =
            self.resolve_authz_strictest(&command, blanket_authorized);
        match authz::decide(policy, elevated, blanket_authorized) {
            Decision::Allow => {}
            Decision::Confirm { sudo_grant } => {
                // Pause and ask the human to authorize this command line (A3).
                trace.extend_from_slice(format!("[tool] $ {command}\n[tool] awaiting authorization\n").as_bytes());
                return ToolStep::Pause(AskPauseKind::Confirm { command, sudo_grant });
            }
            Decision::Deny => {
                trace.extend_from_slice(format!("[tool] $ {command}\n[tool] refused: denied\n").as_bytes());
                return done_err("denied by policy".into());
            }
        }

        ToolStep::Done(self.run_shell_tool(call, &command, blanket_authorized, trace).await)
    }

    /// Run an authorized `shell` command line and build its tool result (JSON stdout/stderr/exit,
    /// truncated). Shared by the inline path and the post-approval resume. Emits the `[tool]` trace.
    async fn run_shell_tool(
        &mut self,
        call: &crate::ai::ask::AskToolCall,
        command: &str,
        blanket_authorized: bool,
        trace: &mut Vec<u8>,
    ) -> crate::ai::ask::AskToolResult {
        let result = self.run_command(command, None, blanket_authorized).await;
        trace.extend_from_slice(
            format!("[tool] $ {command}\n[tool] exit {}\n", result.exit_code).as_bytes(),
        );
        let payload = serde_json::json!({
            "stdout": truncate_tool_output(&result.stdout),
            "stderr": truncate_tool_output(&result.stderr),
            "exit_code": result.exit_code,
        });
        crate::ai::ask::AskToolResult {
            id: call.id.clone(),
            name: call.name.clone(),
            outcome: Ok(payload.to_string()),
        }
    }
}
