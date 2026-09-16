//! Command resolution with the plug-in installed: the names `type` gains, the `$PATH` it extends,
//! and the commands it contributes to the virtual `/bin`. The core half of this — resolution that
//! holds with no plug-in at all — lives in `bash::session::tests::resolution`.
//!
//! Fixtures live in the parent module ([`super`]).

use super::*;

/// `type` for a clank-intercepted command resolves through clank's own dispatch (Brush's `type`
/// can't see it): `type curl` → "curl is a shell builtin", exit 0. This is the README's "type
/// authoritative for all commands" made true end-to-end through `eval_line`.
#[test]
fn type_resolves_intercepted_command_as_builtin() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.install_clank();
        // The core's intercepted names plus the installed plug-in's — `type` is authoritative for
        // both, and the session under test has clank installed.
        let intercepted = bash::builtins::typecmd::CORE_INTERCEPTED
            .iter()
            .copied()
            .chain(["ask", "mcp", "grease", "golem"]);
        for name in intercepted {
            let result = session.eval_line(&format!("type {name}")).await;
            assert_eq!(result.exit_code, 0, "type {name} should exit 0");
            assert_eq!(
                String::from_utf8(result.stdout).unwrap(),
                format!("{name} is a shell builtin\n"),
                "type {name} should report a shell builtin"
            );
        }

        // `-t` prints the bare word, like Brush.
        let result = session.eval_line("type -t curl").await;
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "builtin\n");
    });
}

/// `$PATH` is set to clank's README default (the virtual package-resolution namespace).
/// Both env locks — the PATH is now built from the mcp AND grease dir overrides, so a concurrent
/// test holding either set of `CLANK_*` vars would leak its temp dirs into this session's PATH.
/// Lock order is the house order, GREASE then MCP (`set_grease_dirs` before `set_mcp_dirs`
/// everywhere) — the reverse deadlocks the suite AB-BA.
#[test]
fn path_is_the_readme_default() {
    let _grease = crate::grease::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _mcp = crate::mcp::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.install_clank();
        let (out, _) = session.run_line("echo $PATH").await;
        let expected = format!("{}\n", crate::config::vfs::DEFAULT_PATH);
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    });
}

/// `ls /bin` enumerates every registered command name — intercepted (`curl`, `prompt-user`),
/// Brush-registered (`cat`) and plug-in-installed (`ask`) alike — so the AI can discover the full
/// capability set. Virtual `/bin`, resolved against the SESSION registry (core + plug-in), which is
/// why a plug-in's commands appear here at all.
#[test]
fn ls_bin_lists_all_commands() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.install_clank();
        let (out, _) = session.run_line("ls /bin").await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("curl"), "got: {out}");
        assert!(out.contains("prompt-user"));
        assert!(out.contains("cat"));
        assert!(out.contains("ask"), "plug-in commands too, got: {out}");
    });
}
