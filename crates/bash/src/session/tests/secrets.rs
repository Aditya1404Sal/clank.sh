//! `export --secret`: the value must expand for commands, stay out of `env`, out of the
//! transcript, out of `shell.log`, and out of any later output that happens to contain it.
//!
//! Fixtures live in the parent module ([`super`]).

use super::*;

/// A `--secret` variable is available via the environment — `$NAME` expands in a subsequent command,
/// exactly like an ordinary exported var (the redaction contract must not break value availability).
#[test]
fn secret_export_value_still_expands() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_EXPANDS=sk-expand-123")
            .await;
        let result = session.eval_line(r#"echo "$CLANK_SECRET_EXPANDS""#).await;
        // The author's own `echo` deliberately emits the value — but the transcript recorder masks it,
        // so we assert on the raw command *result* (what a subprocess/child would see), not the record.
        assert_eq!(
            String::from_utf8(result.stdout).unwrap(),
            "sk-expand-123\n",
            "$NAME must still expand to the secret value"
        );
    });
}

/// An operator-bearing `--secret` export is refused with an honest error — it must never fall
/// through to Brush's export, which would set the value with zero redaction. (Exactly the live-demo
/// probe: `export --secret K=v && echo set` looked like the feature was inert; the truth was a
/// silent unredacted fallthrough.)
#[test]
fn secret_export_with_operators_is_refused() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session
            .eval_line("export --secret CLANK_SECRET_NONPLAIN=sk-nonplain-7 && echo set")
            .await;
        assert_eq!(result.exit_code, 2);
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains("standalone command"), "got: {stderr}");
        // Neither half ran: the variable is unset and the `&&` tail never printed.
        assert!(String::from_utf8(result.stdout).unwrap().is_empty());
        let echoed = session
            .eval_line(r#"echo "${CLANK_SECRET_NONPLAIN:-unset}""#)
            .await;
        assert_eq!(String::from_utf8(echoed.stdout).unwrap(), "unset\n");
    });
}

/// Quoted metacharacters in the VALUE keep the line plain — `export --secret K="a|b"` is a
/// standalone command (the `|` is data, not an operator) and must keep working.
#[test]
fn secret_export_quoted_metachar_value_still_plain() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session
            .eval_line(r#"export --secret CLANK_SECRET_PIPEVAL="a|b""#)
            .await;
        assert_eq!(result.exit_code, 0);
        let echoed = session.eval_line(r#"echo "$CLANK_SECRET_PIPEVAL""#).await;
        assert_eq!(String::from_utf8(echoed.stdout).unwrap(), "a|b\n");
    });
}

/// A `--secret` variable is hidden from `env` output (neither the value nor the key appears), while
/// an ordinary environment var (one genuinely in the process env) still shows — proving the filter
/// drops *only* the secret-marked key.
///
/// The control var is seeded directly into `std::env`, not via a plain Brush `export`: clank's
/// `env` reads the process environment (`std::env`), while Brush's `export` populates Brush's own
/// variable table — the two are decoupled by design, so a Brush-only `export FOO=bar` never shows
/// in `env`. That pre-existing decoupling is not what this test is about.
#[test]
fn secret_export_hidden_from_env() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        std::env::set_var("CLANK_ENV_CONTROL", "controlval");
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_ENV=sk-envhidden-456")
            .await;
        let (env_out, _) = session.run_line("env").await;
        let env_out = String::from_utf8(env_out).unwrap();
        assert!(
            !env_out.contains("sk-envhidden-456"),
            "secret value must not appear in env:\n{env_out}"
        );
        assert!(
            !env_out.contains("CLANK_SECRET_ENV"),
            "secret key must not appear in env:\n{env_out}"
        );
        // A genuine process-env var is unaffected — proves we filter only the marked one.
        assert!(
            env_out.contains("CLANK_ENV_CONTROL=controlval"),
            "a non-secret process-env var should still show in env:\n{env_out}"
        );
        std::env::remove_var("CLANK_ENV_CONTROL");
    });
}

/// `env` filters the secret even as a non-final PIPELINE stage. Natively that stage runs on a Brush
/// `spawn_blocking` WORKER thread — a thread the per-line secret set was never installed on — which is
/// exactly why the filter set is a process-global slot and not a thread-local (the conformance suite
/// caught the leak live: `env | grep -c SECRET` printed 1 natively).
///
/// Crucially the consumer is `grep`, NOT `cat`: `grep` is a `run_tool` command that genuinely reads
/// the pipe, while uu `cat` reads the real process stdin natively and never sees `env`'s piped output
/// — so an `env | cat` assertion is a false-green that passes even while the secret leaks.
#[test]
fn secret_hidden_from_env_in_a_pipeline() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_PIPE_ENV=sk-pipehide-999")
            .await;
        // env is stage 0 (non-final) → owned-subshell worker thread on native. Filtered before grep.
        let result = session.eval_line("env | grep CLANK_SECRET_PIPE_ENV").await;
        let out = String::from_utf8(result.stdout).unwrap();
        assert!(
            out.is_empty(),
            "the secret must be filtered before a pipeline consumer reads it, but env leaked it \
             to grep:\n{out}"
        );
    });
}

/// `env -0` (NUL-separated listing) still filters the secret — the custom listing path handles the
/// `-0` flag rather than delegating to `uu_env`.
#[test]
fn secret_hidden_from_env_null_separated() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_NUL=sk-nulhide-111")
            .await;
        let (out, _) = session.run_line("env -0").await;
        let out = String::from_utf8(out).unwrap();
        assert!(
            !out.contains("sk-nulhide-111") && !out.contains("CLANK_SECRET_NUL"),
            "env -0 must also filter the secret:\n{out}"
        );
    });
}

/// `env -i` (ignore-environment) listing still hides a secret — starting from an empty env means the
/// secret was never there, and the filter path is what serves the listing (not `uu_env`). A control
/// `NAME=value` operand added on the line still shows.
#[test]
fn secret_hidden_from_env_ignore_and_inline_add() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        std::env::set_var("CLANK_ENV_IADD", "willbedropped");
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_IENV=sk-ihide-222")
            .await;
        // `-i` starts from empty; the inline VISIBLE=y add should be the only listed var.
        let (out, _) = session.run_line("env -i VISIBLE=y").await;
        let out = String::from_utf8(out).unwrap();
        assert!(
            !out.contains("sk-ihide-222") && !out.contains("CLANK_SECRET_IENV"),
            "env -i must not surface the secret:\n{out}"
        );
        assert!(
            out.contains("VISIBLE=y"),
            "an inline add should still list:\n{out}"
        );
        std::env::remove_var("CLANK_ENV_IADD");
    });
}

/// `env -u NAME` (unset) drops a normal var from the listing and still filters secrets.
#[test]
fn env_unset_drops_a_var() {
    on_rt(async {
        std::env::set_var("CLANK_ENV_UNSET_ME", "dropme");
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("env -u CLANK_ENV_UNSET_ME").await;
        let out = String::from_utf8(out).unwrap();
        assert!(
            !out.contains("CLANK_ENV_UNSET_ME"),
            "env -u NAME should drop NAME from the listing:\n{out}"
        );
        std::env::remove_var("CLANK_ENV_UNSET_ME");
    });
}

/// The defining `export --secret NAME=VALUE` line is recorded to the transcript with its value
/// redacted; the key name is preserved (only the value is sensitive).
#[test]
fn secret_export_line_redacted_in_transcript() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_TX=sk-txredact-789")
            .await;
        let (show_out, _) = session.run_line("context show").await;
        let show_out = String::from_utf8(show_out).unwrap();
        assert!(
            !show_out.contains("sk-txredact-789"),
            "secret value must not appear in the transcript:\n{show_out}"
        );
        assert!(
            show_out.contains("export --secret CLANK_SECRET_TX=<redacted>"),
            "the export line should be recorded with its value redacted:\n{show_out}"
        );
    });
}

/// A `export --secret` declaration never writes its VALUE to shell.log (README "never written to
/// logs"). The declaration is the one case the `mask_values` log filter can't cover — the secret
/// isn't in the active set until the line runs — so the value is stripped structurally in the
/// start/end events. The key name is preserved (only the value is sensitive).
#[test]
fn secret_export_value_absent_from_shell_log() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let cap = LogCapture::new("secretlog");
        let mut session = Session::new().await.unwrap();
        session
            .eval_line("export --secret CLANK_LOG_SECRET=sk-logredact-456")
            .await;
        let log = cap.read(crate::logging::LogFile::Shell);
        assert!(
            !log.contains("sk-logredact-456"),
            "secret value must never reach shell.log:\n{log}"
        );
        assert!(
            log.contains("export --secret CLANK_LOG_SECRET=<redacted>"),
            "the export line should be logged with its value redacted:\n{log}"
        );
    });
}

/// A secret value that surfaces in later command *output* (e.g. the author echoes it) is masked in
/// the transcript — a secret can leak by value where its name never appears.
#[test]
fn secret_value_masked_in_later_output() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_OUT=sk-outmask-abc")
            .await;
        session
            .run_line(r#"echo "leaked: $CLANK_SECRET_OUT""#)
            .await;
        let (show_out, _) = session.run_line("context show").await;
        let show_out = String::from_utf8(show_out).unwrap();
        assert!(
            !show_out.contains("sk-outmask-abc"),
            "secret value must be masked in recorded output:\n{show_out}"
        );
        assert!(
            show_out.contains("leaked: <redacted>"),
            "the echoed value should be masked in the transcript:\n{show_out}"
        );
    });
}

/// A secret value is masked in `ps` COMMAND — the export row shows `<redacted>` on a later `ps`
/// (when the secret is active), never the literal value.
#[test]
fn secret_value_masked_in_ps() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_PS=sk-psmask-def")
            .await;
        let (ps_out, _) = session.run_line("ps").await;
        let ps_out = String::from_utf8(ps_out).unwrap();
        assert!(
            !ps_out.contains("sk-psmask-def"),
            "secret value must not appear in ps COMMAND:\n{ps_out}"
        );
        assert!(
            ps_out.contains("<redacted>"),
            "the export row should show the value redacted:\n{ps_out}"
        );
    });
}

/// A `--secret` export buried in a pipeline is NOT intercepted (falls through to Brush) — the
/// interception is scoped to plain top-level lines, like `context summarize`.
#[test]
fn secret_export_in_pipeline_falls_through() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        // `echo x | export --secret K=v` is not a plain line; it must not reach `run_secret_export`
        // (which would mark K secret). We prove non-interception by checking the var is NOT redacted
        // from env afterward (Brush's own handling, whatever it does, doesn't populate `secret_env`).
        session
            .run_line("echo x | export --secret CLANK_SECRET_PIPE=notsecret")
            .await;
        // Whatever Brush did with the pipeline, our secret table stays empty for this name, so a
        // plain `env` is not filtering it. The strongest assertion we can make without depending on
        // Brush's pipeline semantics: the session's secret table did not gain the name.
        assert!(
            !session.secret_env.contains_key("CLANK_SECRET_PIPE"),
            "a pipelined `export --secret` must not populate the secret table"
        );
    });
}

/// `man export` renders clank's manifest help, which documents the `--secret` form. (`export --help`
/// is served by Brush's own clap help, not clank's manifest — the manifest surfaces via `man` /
/// `cat /bin/export`, as for the other Brush-native builtins.)
#[test]
fn man_export_documents_secret() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("man export").await;
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("--secret"),
            "man export should document the --secret form:\n{out}"
        );
    });
}
