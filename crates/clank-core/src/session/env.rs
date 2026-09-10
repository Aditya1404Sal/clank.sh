//! How a `Session`'s environment and filesystem namespace are established.
//!
//! Three steps that run once at construction and are easy to reason about together: the `$PATH`
//! clank installs ([`effective_path`]), the directory tree it expects to exist
//! ([`ensure_fs_layout`]), and the Brush shell it hands them to ([`build_shell`]).
//!
//! `effective_path` and `ensure_fs_layout` must agree: the path names the directories, the layout
//! creates them. Both resolve through the env-overridable accessors in `grease::config`/`mcp::config`
//! rather than through [`crate::config::vfs`] directly, so a session pointed at writable dirs
//! RESOLVES what it installs.

use super::{BuiltinSet, Shell, ShellBuilderExt, DEFAULT_HOME};

/// The README `$PATH` with every package dir resolved through its env-overridable config fn, so a
/// native session pointed at writable dirs (`CLANK_MCP_BIN=~/.clank/mcp-bin` etc. — required on
/// macOS, where `/usr/lib/...` isn't writable) RESOLVES what it installs: before this, `mcp add`
/// wrote its launcher into the override dir while `$PATH` kept the hardcoded default, so
/// `which`/`type`/`ls /bin` never saw the installed command. With no overrides set this is
/// byte-identical to [`DEFAULT_PATH`] (unit-pinned).
pub(super) fn effective_path() -> String {
    format!(
        "/usr/local/bin:{}:{}:{}:{}:{}/*/bin",
        crate::grease::config::script_bin_dir().display(), // default /usr/bin
        crate::mcp::config::bin_dir().display(),           // default /usr/lib/mcp/bin
        crate::grease::config::agent_bin_dir().display(),  // default /usr/lib/agents/bin
        crate::grease::config::bin_dir().display(),        // default /usr/lib/prompts/bin
        crate::grease::config::skills_dir().display()      // default /usr/share/skills
    )
}

/// One-time, best-effort filesystem layout at session start.
///
/// The agent's per-instance VFS starts EMPTY — before this, `/tmp` existed only if a uu builtin's
/// capture path happened to run first, so a fresh agent's very first `curl -o /tmp/f` or
/// `echo x > /tmp/f` failed with "No such file or directory (os error 44)" until someone typed
/// `mkdir -p /tmp` (a live-demo gotcha). Create the whole README namespace up front. Idempotent
/// (`create_dir_all`) and replay-safe on the durable agent — whole-state directory creation, not an
/// append.
///
/// Native creates ONLY the clank-owned dirs the operator explicitly pointed somewhere writable via
/// a `CLANK_*` env override — never absolute system paths on the host (`/usr/lib/...` on macOS is
/// not clank's to create), and `/tmp` already exists on every host.
pub(super) fn ensure_fs_layout() {
    #[cfg(target_arch = "wasm32")]
    {
        for d in ["/tmp", "/var/log", DEFAULT_HOME, "/usr/local/bin"] {
            let _ = std::fs::create_dir_all(d);
        }
        for d in [
            crate::mcp::config::etc_dir(),
            crate::mcp::config::bin_dir(),
            crate::grease::config::etc_dir(),
            crate::grease::config::store_dir(),
            crate::grease::config::bin_dir(),
            crate::grease::config::script_bin_dir(),
            crate::grease::config::skills_dir(),
            crate::grease::config::agent_bin_dir(),
        ] {
            let _ = std::fs::create_dir_all(&d);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        for (var, dir) in [
            ("CLANK_MCP_ETC", crate::mcp::config::etc_dir()),
            ("CLANK_MCP_BIN", crate::mcp::config::bin_dir()),
            ("CLANK_GREASE_ETC", crate::grease::config::etc_dir()),
            ("CLANK_GREASE_STORE", crate::grease::config::store_dir()),
            ("CLANK_GREASE_BIN", crate::grease::config::bin_dir()),
            (
                "CLANK_GREASE_SCRIPT_BIN",
                crate::grease::config::script_bin_dir(),
            ),
            ("CLANK_GREASE_SKILLS", crate::grease::config::skills_dir()),
            (
                "CLANK_GREASE_AGENT_BIN",
                crate::grease::config::agent_bin_dir(),
            ),
        ] {
            if std::env::var_os(var).is_some() {
                let _ = std::fs::create_dir_all(&dir);
            }
        }
    }
}

pub(super) async fn build_shell() -> Result<Shell, brush_core::Error> {
    // NB: clank's builtins are registered here AND their manifests in `registry::build()`; the two
    // must stay in lockstep (the registry drift-guard test enforces it). Adding a builtin via
    // `Shell::register_builtin` directly would bypass the manifest — don't.
    let mut shell = Shell::builder()
        .default_builtins(BuiltinSet::BashMode)
        .builtins(crate::tools::coreutils::builtins())
        .builtins(crate::tools::texttools::builtins())
        .builtins(crate::runtime::ps::builtins())
        .builtins(crate::tools::which::builtins())
        .builtins(crate::tools::man::builtins())
        .builtins(crate::tools::stat::builtins())
        .builtins(crate::tools::find::builtins())
        .builtins(crate::tools::xargs::builtins())
        .builtins(crate::ai::model::builtins())
        .builtins(crate::builtins::context::builtins())
        .builtins(crate::builtins::interceptstub::builtins())
        .build()
        .await?;

    // Set clank's `$PATH` explicitly, overriding whatever Brush's init seeded (empty on the wasm
    // stub, the host's real PATH on native — both wrong for clank's virtual namespace). Read by
    // `$PATH` expansion and by `type`/`which` path resolution alike.
    shell.env_mut().set_global(
        "PATH",
        brush_core::variables::ShellVariable::new(effective_path()),
    )?;

    // Seed `$HOME` to the README layout (`/home/user`) only when unset — the agent's wasm env is
    // empty, so `~` expansion and `~/.config/ask/ask.toml` need it; native keeps the host's real
    // `$HOME` (ask.toml is a native location too, per the README).
    if shell.env().get("HOME").is_none() {
        shell.env_mut().set_global(
            "HOME",
            brush_core::variables::ShellVariable::new(DEFAULT_HOME),
        )?;
    }

    Ok(shell)
}
