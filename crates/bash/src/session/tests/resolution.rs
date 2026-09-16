//! Command resolution: `$PATH`, `which`/`type`, the virtual `/bin` and `/proc` namespaces,
//! and `--help` for intercepted commands. "What does this name resolve to, and what does it say?"
//!
//! The half of this that depends on an installed plug-in — the names `type` adds, the `$PATH` it
//! extends, the commands it contributes to `/bin` — lives with the plug-in.
//!
//! Fixtures live in the parent module ([`super`]).

use super::*;

/// `type` for a Brush-registered builtin (`cat`) falls through to Brush unchanged — clank does
/// NOT intercept it. Proves the fallthrough half of the design: clank owns only the intercepted
/// names, Brush keeps everything else.
#[test]
fn type_falls_through_to_brush_for_registered_builtin() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line("type cat").await;
        let out = String::from_utf8(result.terminal_output()).unwrap();
        // Brush's `type` resolves `cat` (a registered builtin) — clank didn't short-circuit it.
        assert!(
            out.contains("cat") && out.contains("builtin"),
            "Brush's type should resolve cat as a builtin, got: {out}"
        );
    });
}

/// `<cmd> --help` for an intercepted command prints its manifest help text and exits 0, through
/// `eval_line`. These commands never reach Brush's dispatch, so this is the only place they get
/// `--help`. Crucially, `curl --help` does NOT surface the outbound-HTTP confirmation — it's a
/// help query, handled before the authz gate.
#[test]
fn help_flag_prints_help_for_intercepted_commands() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();

        let result = session.eval_line("curl --help").await;
        assert_eq!(result.exit_code, 0);
        assert!(
            result.pending_prompt.is_none(),
            "curl --help must not confirm"
        );
        let out = String::from_utf8(result.stdout).unwrap();
        assert!(out.contains("fetch a URL over"), "got: {out}");

        let result = session.eval_line("prompt-user --help").await;
        assert_eq!(result.exit_code, 0);
        assert!(
            result.pending_prompt.is_none(),
            "help must not surface a prompt"
        );
        assert!(String::from_utf8(result.stdout)
            .unwrap()
            .contains("pause the"));

        let result = session.eval_line("wget --help").await;
        assert_eq!(result.exit_code, 0);
        assert!(String::from_utf8(result.stdout)
            .unwrap()
            .contains("download a URL"));

        let result = session.eval_line("context --help").await;
        assert_eq!(result.exit_code, 0);
        assert!(String::from_utf8(result.stdout)
            .unwrap()
            .contains("session transcript"));
    });
}

/// `which` finds nothing for a name with no file-backed form, and does NOT report a phantom
/// path (the bug caught on the agent: Brush's wasm `executable()` returns true unconditionally,
/// so `which` must verify existence itself). Chained with a marker to prove no wedge/error.
#[test]
fn which_finds_nothing_for_a_nonexistent_command() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session
            .run_line("which clank-no-such-cmd-xyz; echo done")
            .await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.trim_end().ends_with("done"), "got: {out}");
        assert!(
            !out.contains("clank-no-such-cmd-xyz"),
            "which must not report a phantom path for a missing command: {out}"
        );
    });
}

/// `which` finds a real executable file placed on `$PATH`.
#[test]
fn which_finds_a_real_path_file() {
    on_rt(async {
        let dir = std::env::temp_dir().join(format!("clank_which_bin_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("clank-which-probe");
        std::fs::write(&exe, b"#!/bin/sh\ntrue\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut session = Session::new().await.unwrap();
        // Prepend our dir to $PATH, then `which` should find the probe file.
        session
            .run_line(&format!("export PATH={}:$PATH", dir.display()))
            .await;
        let (out, _) = session.run_line("which clank-which-probe").await;
        let out = String::from_utf8(out).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            out.contains("clank-which-probe"),
            "which should find a real $PATH file, got: {out}"
        );
    });
}

/// Brush's own `type` still works (now with a clank manifest, but unchanged behavior) — it
/// reports a clank builtin as a builtin. Guards the manifest-registration change against
/// accidentally breaking Brush's builtin dispatch.
#[test]
fn type_reports_a_builtin() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("type ls").await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("ls"), "got: {out}");
        assert!(
            out.contains("builtin"),
            "type should call ls a builtin, got: {out}"
        );
    });
}

/// `type`/`command`/`which` have registry manifests (the resolution surface sees them).
#[test]
fn resolution_commands_have_manifests() {
    on_rt(async {
        let session = Session::new().await.unwrap();
        for name in ["type", "command", "which"] {
            assert!(
                session.registry().get(name).is_some(),
                "{name} should have a manifest"
            );
        }
    });
}

#[test]
fn cat_reads_virtual_proc_status() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("cat /proc/1/status").await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("Pid:"), "got: {out}");
        assert!(out.contains("State:"));
        assert!(out.contains("clank"));
    });
}

/// `cat /bin/<name>` prints the command's help text — the virtual file is `cat`-able like a
/// `/proc` file. Covers an intercepted command (`curl`, invisible to Brush's own resolution).
#[test]
fn cat_bin_curl_shows_help() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("cat /bin/curl").await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("fetch a URL over"), "got: {out}");
    });
}

/// `cat /bin/<unknown>` reports "No such file or directory" (like a real missing file), not a
/// spurious success — the virtual namespace only serves registered names.
#[test]
fn cat_bin_unknown_is_not_found() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("cat /bin/does-not-exist").await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("No such file or directory"), "got: {out}");
    });
}

/// `grep` output is captured via `context.stdout()` (the `run_tool` path) — this is
/// parallel-safe (no process-global fd swap) and verifies the wasm output-capture fix on the
/// native side too.
#[test]
fn grep_captures_output() {
    on_rt(async {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("clank_grep_test_{}", std::process::id()));
        std::fs::write(&path, b"alpha\nbeta\ngamma\n").unwrap();
        let mut session = Session::new().await.unwrap();
        let (out, _) = session
            .run_line(&format!("grep beta {}", path.display()))
            .await;
        let _ = std::fs::remove_file(&path);
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("beta"),
            "grep output should contain the match, got: {out}"
        );
        assert!(
            !out.contains("alpha"),
            "grep should not emit non-matching lines: {out}"
        );
    });
}

/// `grep` over a virtual `/proc` file works and its output is captured.
#[test]
fn grep_matches_virtual_proc_file() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("grep State /proc/1/status").await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("State"), "got: {out}");
    });
}

/// A real-file `cat` still works after `cat` became a `/proc`-aware shim (delegation intact).
#[test]
fn cat_still_reads_real_files() {
    on_rt(async {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("clank_cat_test_{}", std::process::id()));
        std::fs::write(&path, b"real-file-contents\n").unwrap();
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line(&format!("cat {}", path.display())).await;
        let _ = std::fs::remove_file(&path);
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("real-file-contents"), "got: {out}");
    });
}
