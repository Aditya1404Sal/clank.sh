//! `SessionCtx`: what a plug-in may reach while it runs.

use super::*;

#[test]
fn ctx_reaches_the_process_table_the_transcript_and_the_registry() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let mut ctx = SessionCtx::new(&mut session);

        let pid = ctx.proc_spawn_bg(
            crate::runtime::proctable::ProcessKind::Builtin,
            vec!["sleep".to_string()],
            crate::runtime::proctable::SHELL_ROOT_PID,
        );
        ctx.proc_complete(pid);
        // Output lands in the transcript's trailing command entry, so record the command first —
        // the order `eval_line` itself uses.
        ctx.with_transcript(|t| t.record_command("plug demo"));
        ctx.record_output(b"from-a-plugin\n");

        assert!(ctx.manifest("cat").is_some());
        assert!(!ctx.allow_all());
        ctx.set_allow_all(true);
        assert!(ctx.allow_all());
        assert!(
            ctx.with_transcript(|t| String::from_utf8_lossy(&t.render()).contains("from-a-plugin"))
        );

        let ran = ctx.execute("echo via-ctx").await;
        assert_eq!(ran.stdout, b"via-ctx\n");
    });
}
