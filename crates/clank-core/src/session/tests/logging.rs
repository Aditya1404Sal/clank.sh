//! The `/var/log` sinks: what each command writes, and what must never appear there.
//!
//! Fixtures live in the parent module ([`super`]).

use super::*;

/// A normal command writes start + end (with exit code) events to shell.log.
#[test]
fn shell_log_records_start_and_end() {
    on_rt(async {
        let cap = LogCapture::new("shell");
        let mut session = Session::new().await.unwrap();
        session.eval_line("echo hi").await;
        session.eval_line("false").await;
        let log = cap.read(crate::logging::LogFile::Shell);
        assert!(log.contains(r#"start line="echo hi""#), "got:\n{log}");
        // Terminal events carry the shell pid, so interleaved lines can be told apart — the
        // "PID/PPID-addressable audit events" the logging module doc advertises.
        assert!(
            log.contains(r#"end pid=1 line="echo hi" exit=0"#),
            "got:\n{log}"
        );
        assert!(
            log.contains("exit=1"),
            "the failing command's exit code is logged, got:\n{log}"
        );
    });
}

/// A destructive (`sudo-only`) command is recorded in ops.log with its authorization outcome, even
/// when denied (a bare `rm` is `sudo-only` → confirm-required without sudo).
#[test]
fn ops_log_records_destructive_ops() {
    on_rt(async {
        let cap = LogCapture::new("ops");
        let mut session = Session::new().await.unwrap();
        // A bare `rm` is the destructive tier; without sudo it needs confirmation.
        let r = session.eval_line("rm /tmp/whatever").await;
        assert!(r.pending_prompt.is_some(), "rm should confirm");
        session.answer_prompt(Some("no".into())).await;
        let log = cap.read(crate::logging::LogFile::Ops);
        assert!(
            log.contains("destructive"),
            "ops.log should record the destructive op, got:\n{log}"
        );
        assert!(log.contains("cmd=rm"), "got:\n{log}");
        assert!(log.contains("confirm-required"), "got:\n{log}");
    });
}

/// An `ask` LLM turn is recorded in http.log (via the `LoggingAskProvider` wrapper).
#[test]
fn http_log_records_the_llm_turn() {
    on_rt(async {
        let cap = LogCapture::new("http");
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("reply", seen)));
        session.eval_line(r#"sudo ask "hello""#).await;
        let log = cap.read(crate::logging::LogFile::Http);
        assert!(
            log.contains("kind=llm"),
            "http.log should record the LLM call, got:\n{log}"
        );
        assert!(log.contains("status=ok"), "got:\n{log}");
    });
}
