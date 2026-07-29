//! The eval pipeline itself: dispatch order, stream/exit-code reporting, the process
//! table, and the invariants that hold for every line regardless of which command it names.
//!
//! Fixtures live in the parent module ([`super`]).

use super::*;

/// `cd` must move the builtins, not just `pwd`.
///
/// Regression: brush keeps `cd` in its own `Shell::working_dir` and never touches the process's cwd
/// (correct for a shell library — it hands the directory to spawned children). clank's builtins are
/// never spawned; they run in-process and resolve relative paths via `std::env::current_dir()`. So
/// `pwd` and brush's redirects tracked `cd` while every builtin silently ignored it: `cd sub; ls`
/// listed the process's directory and `cd sub; cat f` read the wrong `f`. `ShellCwd` in
/// `tools::coreutils` bridges the two. Covers both dispatch paths — a `uu_*` builtin (`ls`/`cat`, via
/// `run_uu`) and a hand-rolled one (`grep`, via `run_tool`).
///
/// These assert on a **decoy** rather than exact stdout, and that isn't laziness: natively `run_uu`
/// points the real fd 1 at brush's file for the duration of a `uu_*` call, and `FD_SWAP_LOCK`
/// serializes that against other `run_uu`s but *not* against the test harness, which prints `test … ok`
/// to the same fd from other threads. So a builtin's captured stdout can legitimately contain harness
/// chatter under `cargo test` (a parallel-harness artifact only — nothing else writes stdout in
/// production). The decoy is the real signal anyway: it separates "read the right file" from "read the
/// wrong one" regardless of what else lands there.
#[test]
fn cd_is_honored_by_builtins_not_just_pwd() {
    // The process cwd is one global (see ShellCwd's cross-SESSION caveat): another test's builtin
    // entering ShellCwd with a different working_dir mid-window yanks it. Serialize against the
    // known heavy contenders (the curl-pipeline tests, whose grep stages hold cwd windows while a
    // mock server round-trips).
    let _cwd = CWD_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let dir = std::env::temp_dir().join(format!("clank_cd_{}", std::process::id()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("only-in-sub.txt"), b"CONTENT-IN-SUB\n").unwrap();
        // Decoys one level up, under the SAME relative name a builtin would use. Resolving against the
        // wrong directory silently finds these instead of erroring — precisely how the bug hid. The
        // names are distinctive so harness chatter can't fake a match.
        std::fs::write(dir.join("only-in-sub.txt"), b"DECOY-IN-PARENT\n").unwrap();
        std::fs::write(dir.join("only-in-parent.txt"), b"DECOY-IN-PARENT\n").unwrap();

        let mut session = Session::new().await.unwrap();
        session.eval_line(&format!("cd {}", sub.display())).await;

        // pwd tracked `cd` even before the fix — assert it still does.
        let pwd = session.eval_line("pwd").await;
        assert_eq!(
            String::from_utf8(pwd.stdout).unwrap().trim(),
            sub.to_string_lossy()
        );

        // `cat` (uu_* -> run_uu) must read sub's file, not the parent's decoy of the same name.
        let cat = String::from_utf8(session.eval_line("cat only-in-sub.txt").await.stdout).unwrap();
        assert!(
            cat.contains("CONTENT-IN-SUB"),
            "cat read nothing useful: {cat:?}"
        );
        assert!(
            !cat.contains("DECOY-IN-PARENT"),
            "cat resolved against the parent: {cat:?}"
        );

        // `ls` must list sub, so the parent-only file must not appear.
        let ls = String::from_utf8(session.eval_line("ls").await.stdout).unwrap();
        assert!(
            ls.contains("only-in-sub.txt"),
            "ls missed sub's file: {ls:?}"
        );
        assert!(
            !ls.contains("only-in-parent.txt"),
            "ls listed the parent: {ls:?}"
        );

        // A relative write must land in sub (a filesystem assertion — immune to stdout entirely).
        session.eval_line("touch made-here.txt").await;
        assert!(
            sub.join("made-here.txt").exists(),
            "touch wrote outside sub"
        );
        assert!(
            !dir.join("made-here.txt").exists(),
            "touch wrote to the parent"
        );

        // `grep` (hand-rolled -> run_tool, the OTHER dispatch path) resolves operands the same way.
        let grep = String::from_utf8(
            session
                .eval_line("grep CONTENT only-in-sub.txt")
                .await
                .stdout,
        )
        .unwrap();
        assert!(
            grep.contains("CONTENT-IN-SUB"),
            "grep read nothing useful: {grep:?}"
        );
        assert!(
            !grep.contains("DECOY-IN-PARENT"),
            "grep resolved against the parent: {grep:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    });
}

/// Structured evaluation exposes stdout, stderr, and exit code for agent callers.
#[test]
fn eval_line_reports_streams_and_exit_status() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();

        let result = session.eval_line("echo hi").await;
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "hi\n");
        assert!(result.stderr.is_empty());
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.flow, Flow::Continue);

        let result = session.eval_line("false").await;
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
        assert_eq!(result.exit_code, 1);
        assert_eq!(result.flow, Flow::Continue);
    });
}

/// uucore keeps a process-global exit code (`uucore::error::set_exit_code`) that upstream
/// coreutils resets by dying — each command is its own process there. clank runs `uumain`
/// IN-PROCESS, so without an explicit reset in `run_uu` a single failed command poisons every
/// later success: `ls <missing>` (exit 2) made a subsequent successful `touch` also report 2.
/// Found by the conformance suite's redirects scenario; affects both targets identically (a wasm
/// agent instance is one long-lived process too).
#[test]
fn uu_exit_code_does_not_stick_across_invocations() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let missing =
            std::env::temp_dir().join(format!("clank_sticky_missing_{}", std::process::id()));
        let created =
            std::env::temp_dir().join(format!("clank_sticky_created_{}", std::process::id()));

        let result = session
            .eval_line(&format!("ls {}", missing.display()))
            .await;
        assert_ne!(result.exit_code, 0, "ls of a missing path must fail");

        let result = session
            .eval_line(&format!("touch {}", created.display()))
            .await;
        assert!(created.exists(), "touch must actually create the file");
        assert_eq!(
            result.exit_code, 0,
            "a successful uu command must not inherit the previous failure's sticky exit code"
        );
        let _ = std::fs::remove_file(&created);
    });
}

/// End-to-end through the public API: a completed command shows `Z` in `ps`, and `ps` sees its
/// own row as `R` (spawned before execution, completed only after) — like real Unix.
#[test]
fn ps_reflects_completed_and_running_rows() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (_out, _flow) = session.run_line("echo hi").await;
        let (ps_out, _flow) = session.run_line("ps").await;
        let ps_out = String::from_utf8(ps_out).unwrap();

        // The prior `echo hi` line completed → its row is Z.
        let echo_row = ps_out
            .lines()
            .find(|l| l.contains("echo hi"))
            .expect("ps should list the completed `echo hi` line");
        assert!(
            echo_row.contains('Z'),
            "completed line should be Z, got: {echo_row}"
        );

        // The `ps` invocation itself is still running while it renders → its row is R.
        let ps_row = ps_out
            .lines()
            .find(|l| l.trim_end().ends_with("ps"))
            .expect("ps should list itself");
        assert!(
            ps_row.contains('R'),
            "ps's own row should be R, got: {ps_row}"
        );

        // The synthetic root is present.
        assert!(ps_out.contains("clank"));
    });
}

/// An absurd `COLUMNS` must not hang the column layout.
///
/// Regression: `shell_columns` parsed the value as an unbounded `usize`, and `format_columns`
/// divides the width by the column stride then iterates `0..cols` — so a huge width produced a
/// column count near `usize::MAX` and an effectively infinite loop. `set_columns` takes a `u16`, but
/// `agent shell` clients set the width by sending a literal `export COLUMNS=<w>` line, so the value
/// reaching the shell is arbitrary text. Out-of-range now reads as unset (the 80-column fallback),
/// which is what an unparseable value already did. Without the fix this test hangs rather than fails.
#[test]
fn an_absurd_columns_value_cannot_hang_the_column_layout() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let r = session
            .eval_line("export COLUMNS=18446744073709551615")
            .await;
        assert_eq!(
            r.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let (out, _) = session.run_line("ls /bin").await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("curl"), "got: {out}");
    });
}

/// PIDs persist and keep climbing across `run_line` calls (the durable-agent property, tested
/// locally): the second command gets a higher PID than the first.
#[test]
fn pids_are_monotonic_across_lines() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.run_line("echo one").await;
        session.run_line("echo two").await;
        let (ps_out, _) = session.run_line("ps").await;
        let ps_out = String::from_utf8(ps_out).unwrap();

        let pid_of = |needle: &str| -> u32 {
            ps_out
                .lines()
                .find(|l| l.contains(needle))
                .and_then(|l| l.split_whitespace().next())
                .and_then(|p| p.parse().ok())
                .unwrap_or_else(|| panic!("no pid for {needle} in:\n{ps_out}"))
        };
        assert!(pid_of("echo two") > pid_of("echo one"));
    });
}

// --- `export --secret` (README "Sensitive environment variables") ---------------------------------
//
// Each test uses a UNIQUE variable name because `export --secret` writes process-global `std::env`
// (Full env parity) and the Rust test harness runs tests on shared threads.

/// Syntactically incomplete input (unterminated heredoc) answers honestly with exit 2 and the
/// session SURVIVES — before this, the fatal parse error mapped to `ExitShell` (clank's Brush shell
/// is non-interactive) and one mistyped `cat <<EOF` ended the whole session.
#[test]
fn incomplete_input_survives_the_session() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line("cat <<EOF").await;
        assert_eq!(result.exit_code, 2);
        assert!(matches!(result.flow, Flow::Continue));
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains("incomplete input"), "got: {stderr}");
        // The session is alive and works.
        let after = session.eval_line("echo alive").await;
        assert_eq!(String::from_utf8(after.stdout).unwrap(), "alive\n");
        assert_eq!(after.exit_code, 0);
        // An unterminated quote takes the same path.
        let quote = session.eval_line("echo 'dangling").await;
        assert_eq!(quote.exit_code, 2);
        // A complete heredoc (newlines embedded in one eval) still runs normally. `contains`, not
        // exact: uu `cat` runs under the process-global fd-1 swap, which can catch a parallel
        // test-reporter line printed during the window (the documented run_uu harness leak).
        let ok = session.eval_line("cat <<EOF\nhello\nEOF").await;
        assert!(String::from_utf8(ok.stdout).unwrap().contains("hello"));
    });
}
