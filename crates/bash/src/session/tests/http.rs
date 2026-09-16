//! `curl`/`wget`: the confirm tier, the pipeline-head shape, and the honest refusal when
//! they are asked to run mid-pipeline.
//!
//! Fixtures live in the parent module ([`super`]).

use super::*;

/// `curl` is `confirm`-policy (outbound HTTP): it surfaces a confirmation before the request runs.
#[test]
fn curl_surfaces_a_confirmation() {
    on_rt(async {
        let url = http_mock("body");
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line(&format!("curl {url}")).await;
        assert!(
            result.pending_prompt.is_some(),
            "curl should surface a confirm"
        );
        assert!(session.has_pending_prompt());
        // No request ran yet: resolve the prompt so the mock thread can exit cleanly.
        session.answer_prompt(Some("no".to_string())).await;
    });
}

/// Approving a `curl` confirmation runs the request — the body comes back on stdout, exit 0.
/// Proves the post-approval deferred path routes through the HTTP dispatch in `run_command`.
#[test]
fn curl_approved_runs_the_request() {
    on_rt(async {
        let url = http_mock("approved-body");
        let mut session = Session::new().await.unwrap();
        session.eval_line(&format!("curl {url}")).await;
        let out = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "approved-body");
    });
}

/// A curl-HEADED pipeline composes: the Session runs the HTTP, and the downstream (Brush) reads
/// the response as stdin. `sudo` pre-authorizes, so the pipeline runs directly.
#[test]
fn curl_headed_pipeline_feeds_the_downstream() {
    let _cwd = CWD_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let url = http_mock("alpha\nbeta\ngamma\n");
        let mut session = Session::new().await.unwrap();
        let result = session
            .eval_line(&format!("sudo curl -s {url} | grep beta"))
            .await;
        assert_eq!(
            result.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "beta\n");
    });
}

/// The downstream may itself be a multi-stage Brush pipeline.
#[test]
fn curl_headed_pipeline_multistage_downstream() {
    let _cwd = CWD_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let url = http_mock("c\nb\na\n");
        let mut session = Session::new().await.unwrap();
        let result = session
            .eval_line(&format!("sudo curl -s {url} | grep -v b | grep -c ."))
            .await;
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "2\n");
    });
}

/// The deferred-confirm path: an unelevated curl pipeline surfaces the confirm; approving runs the
/// WHOLE pipeline (head HTTP + downstream). This is the path that forces the head-split to live in
/// `run_command` — the approval re-runs the raw line there, and an eval_line-only intercept would
/// have re-broken it.
#[test]
fn curl_headed_pipeline_approved_after_confirm() {
    let _cwd = CWD_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let url = http_mock("x1\nx2\n");
        let mut session = Session::new().await.unwrap();
        let pending = session
            .eval_line(&format!("curl -s {url} | grep -c x"))
            .await;
        assert!(
            pending.pending_prompt.is_some(),
            "pipeline curl should confirm"
        );
        let out = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "2\n");
    });
}

/// curl NOT at the head of the pipeline stays an honest stub error (exit 1) that now names the
/// supported form — never the old flattened-argv "unknown option" junk. Under per-segment authz the
/// unelevated `curl` (second segment; the leading `sudo` is on `echo`, not curl) gates first — so
/// approve it, then assert the stub.
#[test]
fn curl_mid_pipeline_is_an_honest_stub_error() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let gated = session
            .eval_line("echo feed | curl https://unused.invalid")
            .await;
        assert!(
            gated.pending_prompt.is_some(),
            "curl mid-pipeline should gate (unelevated)"
        );
        let result = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(result.exit_code, 1);
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains("FIRST in the pipeline"), "got: {stderr}");
        assert!(!stderr.contains("unknown option"), "got: {stderr}");
    });
}

/// Denying a `curl` confirmation → exit 5, no request.
#[test]
fn curl_denied_returns_exit_5() {
    on_rt(async {
        let url = http_mock("never");
        let mut session = Session::new().await.unwrap();
        session.eval_line(&format!("curl {url}")).await;
        let out = session.answer_prompt(Some("no".to_string())).await;
        assert_eq!(out.exit_code, 5);
    });
}

/// `sudo curl` pre-authorizes: the request runs immediately, no prompt. Proves the direct-allow
/// path also routes through the HTTP dispatch (not Brush's `execute`).
#[test]
fn sudo_curl_bypasses_gate_and_fetches() {
    on_rt(async {
        let url = http_mock("sudo-body");
        let mut session = Session::new().await.unwrap();
        let out = session.eval_line(&format!("sudo curl {url}")).await;
        assert!(out.pending_prompt.is_none(), "sudo curl should not prompt");
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "sudo-body");
    });
}

/// `curl -o <file>` (approved) writes the body to a file, stdout empty.
#[test]
fn curl_o_writes_a_file() {
    on_rt(async {
        let url = http_mock("file-body");
        let path = std::env::temp_dir().join(format!("clank_curl_o_{}", std::process::id()));
        let mut session = Session::new().await.unwrap();
        session
            .eval_line(&format!("sudo curl -o {} {url}", path.display()))
            .await;
        // (sudo → no prompt; runs immediately)
        let out = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(out, "file-body");
    });
}

/// `wget -O -` (approved) streams the body to stdout.
#[test]
fn wget_dash_o_to_stdout() {
    on_rt(async {
        let url = http_mock("wget-body");
        let mut session = Session::new().await.unwrap();
        let out = session.eval_line(&format!("sudo wget -O - {url}")).await;
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "wget-body");
    });
}

/// `curl`/`wget` carry `Subprocess`/`Confirm` manifests in the registry.
#[test]
fn http_commands_have_confirm_manifests() {
    on_rt(async {
        let session = Session::new().await.unwrap();
        for name in ["curl", "wget"] {
            let m = session.registry().get(name).expect("manifest");
            assert_eq!(
                m.execution_scope,
                crate::manifest::ExecutionScope::Subprocess
            );
            assert_eq!(
                m.authorization_policy,
                crate::manifest::AuthorizationPolicy::Confirm
            );
        }
    });
}
