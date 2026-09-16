//! The authorization gate on the human path: the `sudo-only` tier, the `sudo` prefix, and
//! the wrapper commands that used to launder a gated command past it.
//!
//! Fixtures live in the parent module ([`super`]).

use super::*;

/// `xargs` must not launder a command past the authorization gate.
///
/// Regression: xargs re-enters via `shell.run_string`, which goes straight into Brush and never
/// returns through `eval_line`'s gate — so `echo /path | xargs rm` DELETED the file at exit 0 with no
/// confirmation, while bare `rm` is sudo-only and pauses. The re-entry was a hole in the
/// authorization model rather than a corner of it, and `find … | xargs rm -rf` is a line an LLM
/// writes without prompting.
#[test]
fn xargs_cannot_launder_a_command_past_the_authz_gate() {
    // Outside `on_rt`: the guard must not be held across an await (clippy `await_holding_lock`).
    let _cwd = CWD_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let dir = std::env::temp_dir().join(format!("clank-xargs-authz-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let victim = dir.join("victim");
        std::fs::write(&victim, b"x").unwrap();

        let mut session = Session::new().await.unwrap();
        let gated = session
            .eval_line(&format!("echo {} | xargs rm", victim.display()))
            .await;
        // It PAUSES for confirmation, exactly as a bare `rm` does — the gate now sees through the
        // wrapper to the command it will re-enter with, so `xargs rm` is treated as `rm`.
        let prompt = gated
            .pending_prompt
            .as_ref()
            .map(|p| p.question.clone())
            .unwrap_or_default();
        assert!(
            prompt.contains("rm"),
            "must pause naming rm, got prompt {prompt:?} exit {}",
            gated.exit_code
        );
        assert!(victim.exists(), "nothing runs before the human answers");

        // Declining leaves the file alone.
        let declined = session.answer_prompt(Some("n".into())).await;
        assert_ne!(declined.exit_code, 0, "a declined gate must not succeed");
        assert!(victim.exists(), "the file survives a declined xargs rm");

        // Approving runs it — the gate is a gate, not a wall.
        //
        // Approval goes through the prompt rather than `sudo`, because clank only strips a LEADING
        // `sudo`: on a pipeline `sudo` would have to sit on the `xargs` segment to elevate it, and a
        // mid-line `sudo` is not dispatchable today. That limitation is orthogonal to this fix.
        let again = session
            .eval_line(&format!("echo {} | xargs rm", victim.display()))
            .await;
        assert!(again.pending_prompt.is_some(), "gates again");
        let approved = session.answer_prompt(Some("y".into())).await;
        assert_eq!(
            approved.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&approved.stderr)
        );
        assert!(!victim.exists(), "an approved xargs rm removes the file");
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// `rm` is `sudo-only`: without elevation it surfaces a confirmation instead of deleting, and
/// the file survives. (The gate composes on the same pending-prompt pause as `prompt-user`.)
#[test]
fn sudo_only_rm_without_sudo_surfaces_confirmation() {
    on_rt(async {
        let path = seed_file("gate");
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line(&format!("rm {}", path.display())).await;
        // A confirmation was surfaced, not a deletion.
        assert!(
            result.pending_prompt.is_some(),
            "rm should surface a sudo confirmation"
        );
        assert!(session.has_pending_prompt());
        assert!(
            path.exists(),
            "file must survive an unapproved sudo-only rm"
        );
        let _ = std::fs::remove_file(&path);
    });
}

/// Denying (`no`) a `sudo-only` `rm` confirmation → exit 5, file survives, pending clears.
#[test]
fn sudo_only_rm_denied_returns_exit_5() {
    on_rt(async {
        let path = seed_file("deny");
        let mut session = Session::new().await.unwrap();
        session.eval_line(&format!("rm {}", path.display())).await;
        let denied = session.answer_prompt(Some("no".to_string())).await;
        assert_eq!(denied.exit_code, 5, "denial is exit 5");
        assert!(path.exists(), "denied rm must not delete");
        assert!(!session.has_pending_prompt());
        let _ = std::fs::remove_file(&path);
    });
}

/// Approving (`yes`) a `sudo-only` `rm` confirmation runs the deferred command — the file is
/// deleted, exit 0.
#[test]
fn sudo_only_rm_approved_runs_the_command() {
    on_rt(async {
        let path = seed_file("approve");
        let mut session = Session::new().await.unwrap();
        session.eval_line(&format!("rm {}", path.display())).await;
        let approved = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(approved.exit_code, 0, "approved rm succeeds");
        assert!(!path.exists(), "approved rm must delete the file");
        assert!(!session.has_pending_prompt());
    });
}

/// A `sudo rm` prefix pre-authorizes the sudo-only command — it runs immediately, no prompt.
#[test]
fn sudo_prefix_bypasses_the_gate() {
    on_rt(async {
        let path = seed_file("sudo");
        let mut session = Session::new().await.unwrap();
        let result = session
            .eval_line(&format!("sudo rm {}", path.display()))
            .await;
        assert!(result.pending_prompt.is_none(), "sudo rm should not prompt");
        assert_eq!(result.exit_code, 0);
        assert!(!path.exists(), "sudo rm deletes immediately");
    });
}

/// An `allow`-policy command (e.g. `echo`) is completely unaffected by the gate.
#[test]
fn allow_policy_command_is_ungated() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line("echo hi").await;
        assert!(result.pending_prompt.is_none());
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "hi\n");
        assert_eq!(result.exit_code, 0);
    });
}
