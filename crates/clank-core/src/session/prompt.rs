//! `Session` methods for the human-in-the-loop pause machinery: surfacing `prompt-user` and
//! authorization-confirmation pauses (`P` state), and resolving them via `answer_prompt`.

use super::{
    authz, promptuser, AnswerInput, Flow, LineResult, Pending, PendingKind, PendingPrompt,
    Resolution, Session, SessionCtx,
};
use crate::plugin::Plugin;

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
    #[allow(
        clippy::too_many_arguments,
        reason = "the confirmation's copy needs every one of them"
    )]
    pub(super) fn surface_auth_confirm(
        &mut self,
        plugin: Option<&dyn Plugin>,
        command_name: Option<&str>,
        gated_command: String,
        pid: Option<u32>,
        sudo_grant: bool,
        rerun_stdin: Option<String>,
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
        let question = if let Some(question) =
            plugin.and_then(|p| p.confirm_question(&gated_command, sudo_grant))
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
                .or_else(|| {
                    plugin
                        .and_then(|p| p.authz_manifest(name))
                        .map(|m| m.synopsis)
                })
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
                rerun_stdin,
            },
        )
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
            self.proc_table
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pause(pid);
        }
        let mut stdout = prompt.question.clone().into_bytes();
        stdout.push(b'\n');
        self.transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_output(&stdout);

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
        // Take the plug-in out of the slot for the duration, exactly as `eval_line` does — a
        // plug-in-owned pause resumes inside the plug-in, which may re-enter dispatch.
        let mut plugin = self.plugin.take();
        let result = self
            .answer_prompt_with_plugin(plugin.as_deref_mut(), response)
            .await;
        self.plugin = plugin;
        result
    }

    /// [`answer_prompt`](Self::answer_prompt) with the plug-in already in hand — the form
    /// `eval_line_inner` uses for the `kill <paused-pid>` abort, where the slot is already empty.
    pub(super) async fn answer_prompt_with_plugin(
        &mut self,
        plugin: Option<&mut dyn Plugin>,
        response: Option<String>,
    ) -> LineResult {
        let _log = crate::logging::install(self.log_sink.clone());
        // The paused row's PID, for the shell.log end event (its `start` was logged under this PID).
        let paused_pid = self.pending.as_ref().and_then(|p| p.pid);
        let result = self.answer_prompt_inner(plugin, response).await;
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

    async fn answer_prompt_inner(
        &mut self,
        plugin: Option<&mut dyn Plugin>,
        response: Option<String>,
    ) -> LineResult {
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
            self.proc_table
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .resume(pid);
        }

        match pending.kind {
            PendingKind::UserPrompt => self.resolve_user_prompt(resolution, pending.pid),
            PendingKind::AuthConfirm {
                command,
                sudo_grant,
                rerun_stdin,
            } => {
                // Restore any pre-captured pipeline stdin so a deferred `cat x | ask` tail sees it
                // when re-run (consumed by `run_ask` via `SessionCtx::take_rerun_stdin`).
                self.rerun_stdin = rerun_stdin;
                self.resolve_auth_confirm(plugin, resolution, &command, sudo_grant, pending.pid)
                    .await
            }
            PendingKind::Plugin(p) => {
                let Some(plugin) = plugin else {
                    return LineResult::stderr(
                        "clank: internal error: plug-in pause with no plug-in\n",
                    );
                };
                let result = plugin
                    .resume(p, resolution, pending.pid, &mut SessionCtx::new(self))
                    .await;
                // A resumed ask that completes (not re-paused) records its output like the direct
                // path. A re-pause returns a fresh `pending_prompt`; don't record that as final.
                if result.pending_prompt.is_none() {
                    self.transcript
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
            self.proc_table
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .complete(pid);
        }
        let (stdout, exit_code, secret) = match resolution {
            Resolution::Answered { stdout, secret } => (stdout, 0, secret),
            Resolution::Aborted => (Vec::new(), 130, false),
            Resolution::InvalidChoice { .. } => unreachable!("handled by caller"),
        };
        if !secret {
            self.transcript
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .record_output(&stdout);
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
        mut plugin: Option<&mut dyn Plugin>,
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
            self.rerun_stdin = None;
            if let Some(pid) = pid {
                self.proc_table
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .complete(pid);
            }
            let result = LineResult::denied();
            self.transcript
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .record_output(&result.terminal_output());
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
        let result = self
            .run_command(plugin.as_deref_mut(), command, pid, blanket)
            .await;
        // `context summarize` is inspection output — never recorded back (like `context show`). Every
        // other gated command records normally.
        if !plugin.is_some_and(|p| p.is_inspection(command)) {
            self.transcript
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .record_output(&result.terminal_output());
        }
        result
    }
}
