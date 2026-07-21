//! `Session` methods for the human-in-the-loop pause machinery: surfacing `prompt-user` and
//! authorization-confirmation pauses (`P` state), and resolving them via `answer_prompt`.

use super::*;

impl Session {
    /// Handle a `prompt-user` line: parse it, record the pending prompt (durable state), leave the
    /// process row paused (`P`), and return immediately with `pending_prompt` set. The shell does
    /// not block — the caller collects a human answer and delivers it via [`answer_prompt`].
    pub(super) fn surface_prompt(&mut self, line: &str, pid: Option<u32>) -> LineResult {
        let args = match promptuser::parse(line) {
            Ok(args) => args,
            Err(e) => return self.finish_intercepted(pid, LineResult::stderr(format!("{e}\n"))),
        };
        // Piping into `prompt-user` (`X | prompt-user ...`) is a later increment; for now stdin is
        // never wired, so no markdown is prepended.
        let mut pending = args.into_pending(None);
        // The response is redacted iff the line carries a flag the `prompt-user` manifest declares in
        // its `redaction-rules` (README:200/323 — redaction is *governed by the manifest entry*, not a
        // hardcoded flag name). With the built-in `["--secret"]` rule this matches the prior behavior,
        // but the manifest field is now the authority: change the rule and the redaction follows.
        pending.secret = self.line_triggers_redaction("prompt-user", line);
        self.surface_pending(pending, pid, PendingKind::UserPrompt)
    }

    /// Whether `line` carries any flag that `command`'s manifest declares in its `redaction-rules`
    /// (README:200) — the manifest-driven check that governs which invocations redact. `false` if the
    /// command has no manifest, no rules, or the line doesn't tokenize.
    fn line_triggers_redaction(&self, command: &str, line: &str) -> bool {
        let Some(manifest) = self.registry.get(command) else {
            return false;
        };
        let Some(words) = crate::ai::ask::dequote_words(line) else {
            return false;
        };
        crate::manifest::flags_trigger_redaction(&manifest.redaction_rules, &words)
    }

    /// Surface an authorization confirmation for a gated command: pause, record the pending
    /// confirmation (with the command to run on approval), and return `pending_prompt` immediately.
    pub(super) fn surface_auth_confirm(
        &mut self,
        command_name: Option<&str>,
        gated_command: String,
        pid: Option<u32>,
        sudo_grant: bool,
        ask_stdin: Option<String>,
        // When the line has more than one gated command, the pre-rendered "rm [sudo-only], curl
        // [confirm]" summary (from `authz::gated_commands_summary`) so the prompt names them all —
        // approving the strictest runs the whole line. `None` for single-command lines.
        multi_summary: Option<String>,
    ) -> LineResult {
        let name = command_name.unwrap_or("command");
        // Capability disclosure: a `grease install <pkg>` confirmation discloses what the package is
        // and does (name, source registries, that it runs via `ask` = LLM + shell tools under
        // per-command authz) BEFORE the human approves — README "discloses capability requests before
        // completing". Only what's knowable pre-fetch is shown; declared args are one `grease info`
        // away after install.
        let question = if let Some(question) = self.grease_install_disclosure(&gated_command, sudo_grant)
        {
            question
        } else if let Some(summary) = multi_summary {
            // A compound line with several gated commands: name them all, tier = the strictest.
            authz::confirm_question_multi(&summary, sudo_grant)
        } else {
            let synopsis = self
                .registry
                .get(name)
                .map(|m| m.synopsis.clone())
                .or_else(|| self.mcp.manifest_for(name).map(|m| m.synopsis))
                .unwrap_or_else(|| "run this command".to_string());
            authz::confirm_question(name, &synopsis, sudo_grant)
        };
        let prompt = PendingPrompt {
            question,
            choices: Some(authz::confirm_choices(sudo_grant)),
            secret: false,
        };
        self.surface_pending(
            prompt,
            pid,
            PendingKind::AuthConfirm {
                command: gated_command,
                sudo_grant,
                ask_stdin,
            },
        )
    }

    /// If `gated_command` is a `grease install <pkg>` line, build a capability-disclosure confirmation
    /// prompt naming the package, its source registries, and its `ask` capability. `None` otherwise
    /// (the caller falls back to the generic confirm text).
    fn grease_install_disclosure(&self, gated_command: &str, sudo_grant: bool) -> Option<String> {
        let cmd = crate::grease::cmd::classify(gated_command)?.ok()?;
        let crate::grease::cmd::GreaseCommand::Install { name, .. } = cmd else {
            return None;
        };
        let registries = crate::grease::config::list_registries();
        let from = if registries.is_empty() {
            "no configured registry".to_string()
        } else {
            registries.join(", ")
        };
        let tail = if sudo_grant { "(y)es, (n)o" } else { "(y)es, (n)o, (a)ll" };
        // The disclosure fires before the fetch, so the package's kind isn't known yet — disclose the
        // full capability an install can grant: a prompt runs via ask (outbound LLM); a script runs
        // local shell commands; a skill installs model-facing context + `$PATH` scripts. Each is
        // Confirm-gated per run.
        Some(format!(
            "Install package \"{name}\" from {from}? Depending on its kind it may run via ask \
             (outbound LLM), execute local shell commands, or install a skill (model context + \
             $PATH scripts); each is confirmed per run unless you use sudo. {tail}"
        ))
    }

    /// Shared tail of the surface paths: pause the row, record the question, stash the pending
    /// state, and return a `pending_prompt` result.
    pub(super) fn surface_pending(
        &mut self,
        prompt: PendingPrompt,
        pid: Option<u32>,
        kind: PendingKind,
    ) -> LineResult {
        if let Some(pid) = pid {
            self.proc_table.lock().unwrap().pause(pid);
        }
        let mut stdout = prompt.question.clone().into_bytes();
        stdout.push(b'\n');
        self.transcript.lock().unwrap().record_output(&stdout);

        self.pending = Some(Pending {
            prompt: prompt.clone(),
            pid,
            kind,
        });
        LineResult {
            stdout,
            stderr: Vec::new(),
            exit_code: 0,
            flow: Flow::Continue,
            pending_prompt: Some(prompt),
        }
    }

    /// Deliver a response to the outstanding question (a `prompt-user` prompt or an authorization
    /// confirmation). `response` is `Some(text)` for an answer or `None` for an abort. Resumes the
    /// paused row.
    ///
    /// For a `prompt-user` prompt: a valid answer → the response on stdout, exit `0`; abort → exit
    /// `130`; an answer outside `--choices` → exit `1` with the prompt left pending to re-ask.
    ///
    /// For an authorization confirmation: `yes` (or `all`) → runs the gated command; `all` also
    /// grants blanket `confirm` approval for the session; `no`/abort → exit `5` (denied).
    /// Answer (or abort) the outstanding `prompt-user`/authorization question. Logs the resolved line's
    /// terminal `end` event to shell.log once it finishes (a resolution can itself re-pause — an approved
    /// `ask` whose tool call prompts again — in which case the `end` is deferred to the next resolution).
    pub async fn answer_prompt(&mut self, response: Option<String>) -> LineResult {
        let _log = crate::logging::install(self.log_sink.clone());
        // The paused row's PID, for the shell.log end event (its `start` was logged under this PID).
        let paused_pid = self.pending.as_ref().and_then(|p| p.pid);
        let result = self.answer_prompt_inner(response).await;
        // Only a truly-resolved line (no longer pending) gets its terminal event; a re-pause defers.
        if paused_pid.is_some() && result.pending_prompt.is_none() {
            let mut rec = crate::logging::Record::new("end");
            if let Some(pid) = paused_pid {
                rec = rec.field("pid", pid.to_string());
            }
            rec.field("exit", result.exit_code.to_string())
                .emit(crate::logging::LogFile::Shell);
        }
        result
    }

    async fn answer_prompt_inner(&mut self, response: Option<String>) -> LineResult {
        let Some(pending) = self.pending.take() else {
            self.pending = None;
            return LineResult::stderr("clank: no prompt-user question is awaiting a response\n");
        };

        let answer = match response {
            Some(text) => AnswerInput::Response(text),
            None => AnswerInput::Abort,
        };

        let resolution = promptuser::resolve(&pending.prompt, answer);
        if let Resolution::InvalidChoice { message } = resolution {
            // Prompt stays pending — re-ask. Don't touch the row (still `P`). Re-surface the pending
            // view (not a bare stderr): the question is still outstanding, and a caller that saw an
            // empty `pending_prompt` here would believe it resolved and hand control back, wedging the
            // session on a prompt it no longer knows about.
            let prompt = pending.prompt.clone();
            self.pending = Some(pending);
            return LineResult {
                stdout: Vec::new(),
                stderr: message.into_bytes(),
                exit_code: 1,
                flow: Flow::Continue,
                pending_prompt: Some(prompt),
            };
        }

        // Resolved: resume the row (it will be reaped by the specific path below).
        if let Some(pid) = pending.pid {
            self.proc_table.lock().unwrap().resume(pid);
        }

        match pending.kind {
            PendingKind::UserPrompt => self.resolve_user_prompt(resolution, pending.pid),
            PendingKind::AuthConfirm {
                command,
                sudo_grant,
                ask_stdin,
            } => {
                // Restore any pre-captured pipeline stdin so a deferred `cat x | ask` tail sees it
                // when re-run (consumed by `run_ask` via `next_ask_stdin`).
                self.next_ask_stdin = ask_stdin;
                self.resolve_auth_confirm(resolution, &command, sudo_grant, pending.pid)
                    .await
            }
            PendingKind::AgentLoop { state, pause } => {
                let result = self
                    .resolve_agent_loop(resolution, state, pause, pending.pid)
                    .await;
                // A resumed ask that completes (not re-paused) records its output like the direct
                // path. A re-pause returns a fresh `pending_prompt`; don't record that as final.
                if result.pending_prompt.is_none() {
                    self.transcript
                        .lock()
                        .unwrap()
                        .record_output(&result.terminal_output());
                }
                result
            }
        }
    }

    /// Resolve a `prompt-user` response: the answer to stdout (exit 0) or an abort (exit 130), reap
    /// the row, and record the transcript (unless `--secret`).
    fn resolve_user_prompt(&mut self, resolution: Resolution, pid: Option<u32>) -> LineResult {
        if let Some(pid) = pid {
            self.proc_table.lock().unwrap().complete(pid);
        }
        let (stdout, exit_code, secret) = match resolution {
            Resolution::Answered { stdout, secret } => (stdout, 0, secret),
            Resolution::Aborted => (Vec::new(), 130, false),
            Resolution::InvalidChoice { .. } => unreachable!("handled by caller"),
        };
        if !secret {
            self.transcript.lock().unwrap().record_output(&stdout);
        }
        LineResult {
            stdout,
            stderr: Vec::new(),
            exit_code,
            flow: Flow::Continue,
            pending_prompt: None,
        }
    }

    /// Resolve an authorization confirmation: on approval run the gated command (recording it), on
    /// denial reap the row and return exit `5`. A response of "all" also sets the session grant.
    async fn resolve_auth_confirm(
        &mut self,
        resolution: Resolution,
        command: &str,
        _sudo_grant: bool,
        pid: Option<u32>,
    ) -> LineResult {
        let approved = matches!(&resolution, Resolution::Answered { stdout, .. }
            if matches!(String::from_utf8_lossy(stdout).trim(), "yes" | "all"));
        let grant_all = matches!(&resolution, Resolution::Answered { stdout, .. }
            if String::from_utf8_lossy(stdout).trim() == "all");

        if !approved {
            // "no" or abort → denied (exit 5). Reap the row. Drop any pre-captured pipeline stdin so
            // it can't leak into an unrelated later `ask`.
            self.next_ask_stdin = None;
            if let Some(pid) = pid {
                self.proc_table.lock().unwrap().complete(pid);
            }
            let result = LineResult::denied();
            self.transcript.lock().unwrap().record_output(&result.terminal_output());
            return result;
        }

        if grant_all {
            self.authz.allow_all = true;
        }

        // Approved: run the gated command, reusing the row (still `R` after resume) and reaping it.
        // `blanket_authorized` is `false` here: approving a bare `ask`'s outbound-HTTP confirmation is
        // not the same as `sudo ask` — the ask's own tool calls still gate individually. A prior "all"
        // grant (now on `self.authz.allow_all`) is separately honored inside the tool executor.
        let blanket = self.authz.allow_all;
        let result = self.run_command(command, pid, blanket).await;
        // `context summarize` is inspection output — never recorded back (like `context show`). Every
        // other gated command records normally.
        if !is_context_summarize(command) {
            self.transcript.lock().unwrap().record_output(&result.terminal_output());
        }
        result
    }
}
