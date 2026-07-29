//! The `prompt_user` pause: a non-blocking two-call protocol, so these cover what the
//! session will and will not accept while a prompt is outstanding.
//!
//! Fixtures live in the parent module ([`super`]).

use super::*;

/// `prompt-user` is intercepted before Brush dispatch: an invocation Brush would reject
/// differently (unknown command) instead surfaces `promptuser::parse`'s own error, proving
/// the interception — not Brush — handled the line. Also proves the line still shows `Z` in
/// `ps` (the process-table row completes normally for intercepted lines, same as `context`).
#[test]
fn prompt_user_is_intercepted_before_brush_dispatch() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, flow) = session.run_line("prompt-user --confirm").await;
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("missing question"),
            "expected promptuser's own parse error, got: {out}"
        );
        assert_eq!(flow, Flow::Continue);

        let (ps_out, _) = session.run_line("ps").await;
        let ps_out = String::from_utf8(ps_out).unwrap();
        let row = ps_out
            .lines()
            .find(|l| l.contains("prompt-user"))
            .expect("ps should list the prompt-user line");
        assert!(row.contains('Z'), "completed line should be Z, got: {row}");
    });
}

/// A non-`--secret` `prompt-user` error is recorded in the transcript like any other command
/// (only `--secret` *responses* are redacted, per the README — this line never reached the
/// point of collecting a response).
#[test]
fn prompt_user_error_is_recorded_in_transcript() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.run_line("prompt-user --bogus").await;
        let (transcript, _) = session.run_line("context show").await;
        let transcript = String::from_utf8(transcript).unwrap();
        assert!(transcript.contains("prompt-user --bogus"));
        assert!(transcript.contains("unknown flag"));
    });
}

/// The registry carries a manifest for `prompt-user` even though it's never registered as a
/// Brush `SimpleCommand` (it's intercepted before dispatch) — `type`/tool-surface consumers
/// should still see it.
#[test]
fn prompt_user_has_a_registry_manifest() {
    on_rt(async {
        let session = Session::new().await.unwrap();
        let manifest = session
            .registry()
            .get("prompt-user")
            .expect("prompt-user should have a manifest");
        assert_eq!(
            manifest.execution_scope,
            crate::manifest::ExecutionScope::ShellInternal
        );
    });
}

/// The full two-step path: `prompt-user` surfaces the question (returns immediately with
/// `pending_prompt` set, does NOT hang), then `answer_prompt` delivers the response to stdout
/// with exit 0, recorded in the transcript (not `--secret`), and clears the pending state.
#[test]
fn prompt_user_surfaces_then_answer_resolves() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();

        // Step 1: surface. The question comes back and the prompt is pending — no hang.
        let surfaced = session
            .eval_line(r#"prompt-user "Which environment?""#)
            .await;
        assert_eq!(surfaced.exit_code, 0);
        let pending = surfaced
            .pending_prompt
            .expect("should surface a pending prompt");
        assert_eq!(pending.question, "Which environment?");
        assert!(session.has_pending_prompt());

        // Step 2: answer. The response flows to stdout, pending clears.
        let answered = session.answer_prompt(Some("production".to_string())).await;
        assert_eq!(String::from_utf8(answered.stdout).unwrap(), "production\n");
        assert_eq!(answered.exit_code, 0);
        assert!(answered.pending_prompt.is_none());
        assert!(!session.has_pending_prompt());

        let (transcript, _) = session.run_line("context show").await;
        let transcript = String::from_utf8(transcript).unwrap();
        assert!(transcript.contains("production"), "got: {transcript}");
    });
}

/// `kill <pid>` of the P-state prompt-paused row is the one command allowed through while a
/// prompt is pending: it aborts the prompt (exit 130, same as an explicit abort). Any other
/// command — and a kill of a DIFFERENT pid — stays rejected.
#[test]
fn kill_of_pending_prompt_pid_aborts_it() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.eval_line(r#"prompt-user "q?""#).await;
        assert!(session.has_pending_prompt());
        let paused_pid = {
            let table = session.proc_table.lock().unwrap();
            let row = table
                .rows()
                .iter()
                .find(|r| r.state == crate::runtime::proctable::ProcState::P)
                .expect("a paused row");
            row.pid
        };

        // A kill of some other pid is rejected like any other command.
        let other = session
            .eval_line(&format!("kill {}", paused_pid + 100))
            .await;
        assert_eq!(other.exit_code, 1);
        assert!(session.has_pending_prompt());

        // Killing the paused pid aborts the prompt: exit 130, pending cleared, row reaped.
        let killed = session.eval_line(&format!("kill {paused_pid}")).await;
        assert_eq!(killed.exit_code, 130);
        assert!(!session.has_pending_prompt());
        let after = session.eval_line("echo ok").await;
        assert_eq!(String::from_utf8(after.stdout).unwrap(), "ok\n");
    });
}

/// An aborted answer (`None`) exits 130 with no stdout (README) and clears the pending prompt.
#[test]
fn prompt_user_abort_exits_130() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.eval_line(r#"prompt-user "q?""#).await;
        let result = session.answer_prompt(None).await;
        assert!(result.stdout.is_empty());
        assert_eq!(result.exit_code, 130);
        assert!(!session.has_pending_prompt());
    });
}

/// An answer outside the prompt's `--choices` errors and leaves the prompt pending to re-ask.
#[test]
fn prompt_user_invalid_choice_keeps_prompt_pending() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .eval_line(r#"prompt-user "Approve?" --confirm"#)
            .await;
        let bad = session.answer_prompt(Some("maybe".to_string())).await;
        assert_eq!(bad.exit_code, 1);
        assert!(session.has_pending_prompt(), "prompt should stay pending");

        // A valid choice then resolves it.
        let ok = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(String::from_utf8(ok.stdout).unwrap(), "yes\n");
        assert!(!session.has_pending_prompt());
    });
}

/// A `--secret` response is never entered into the transcript (README), though the command
/// line itself is still recorded.
#[test]
fn prompt_user_secret_response_is_redacted_from_transcript() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .eval_line(r#"prompt-user "Enter the API key:" --secret"#)
            .await;
        let result = session.answer_prompt(Some("s3cr3t-key".to_string())).await;
        // The caller still gets the response on stdout...
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "s3cr3t-key\n");

        // ...but it must not appear in the transcript.
        let (transcript, _) = session.run_line("context show").await;
        let transcript = String::from_utf8(transcript).unwrap();
        assert!(
            !transcript.contains("s3cr3t-key"),
            "secret response leaked into transcript: {transcript}"
        );
        // The command line itself is still recorded.
        assert!(transcript.contains("prompt-user"));
    });
}

/// While a prompt is pending, an ordinary command is rejected — the caller must answer first.
#[test]
fn command_while_prompt_pending_is_rejected() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.eval_line(r#"prompt-user "q?""#).await;
        let blocked = session.eval_line("echo hi").await;
        assert_ne!(blocked.exit_code, 0);
        assert!(
            String::from_utf8(blocked.terminal_output())
                .unwrap()
                .contains("awaiting a response"),
            "expected a 'answer the prompt first' error"
        );
        // The prompt is still pending and still answerable.
        assert!(session.has_pending_prompt());
    });
}

/// A command rejected while a prompt is pending must RE-SURFACE that prompt in `pending_prompt`, not
/// return a bare error with `pending_prompt: None`. Regression for the connect-shell deadlock: a new
/// client that attaches to an agent already holding a prompt (e.g. a prior session Ctrl-C'd mid-`ask`)
/// sees the rejection; without the re-surfaced prompt it believes nothing is pending and routes the
/// next input to `eval` — which is rejected again, forever. With it, the client can answer and recover.
#[test]
fn rejected_command_while_pending_re_surfaces_the_prompt() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.eval_line(r#"prompt-user "q?""#).await;
        let blocked = session.eval_line("echo hi").await;
        // Same rejection contract as before: non-zero exit + the "answer first" message.
        assert_ne!(blocked.exit_code, 0);
        assert!(String::from_utf8(blocked.terminal_output())
            .unwrap()
            .contains("awaiting a response"));
        // ...but now the outstanding prompt rides along so a fresh client can resolve it.
        let p = blocked
            .pending_prompt
            .expect("a rejected command must re-surface the pending prompt");
        assert_eq!(p.question, "q?");
        // And it is still answerable through the normal path — the session recovers cleanly.
        let done = session.answer_prompt(Some("ok".to_string())).await;
        assert_eq!(done.exit_code, 0);
        assert!(!session.has_pending_prompt());
    });
}

/// `answer_prompt` with no prompt outstanding is a clean error, not a panic.
#[test]
fn answer_prompt_with_no_pending_is_an_error() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session.answer_prompt(Some("x".to_string())).await;
        assert_ne!(result.exit_code, 0);
    });
}
