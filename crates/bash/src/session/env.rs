//! How a `Session`'s environment and filesystem namespace are established.
//!
//! Three steps that run once at construction and are easy to reason about together: the `$PATH`
//! clank installs ([`effective_path`]), the directory tree it expects to exist
//! ([`ensure_fs_layout`]), and the Brush shell it hands them to ([`build_shell`]).
//!
//! `effective_path` and `ensure_fs_layout` must agree: the path names the directories, the layout
//! creates them. Neither knows any of those directories by name — both take them from the installed
//! plug-in ([`path_dirs`](crate::plugin::Plugin::path_dirs) and
//! [`layout_dirs`](crate::plugin::Plugin::layout_dirs)), which resolves them through its own
//! env-overridable accessors, so a session pointed at writable dirs RESOLVES what it installs.

use std::path::PathBuf;

use super::{BuiltinSet, Shell, ShellBuilderExt, DEFAULT_HOME};

/// The `$PATH` clank installs: `/usr/local/bin`, then one entry per directory in `dirs` (the
/// installed plug-in's [`path_dirs`](crate::plugin::Plugin::path_dirs), in its order).
///
/// The plug-in resolves each package dir through its own env-overridable config fn, so a native
/// session pointed at writable dirs (`CLANK_MCP_BIN=~/.clank/mcp-bin` etc. — required on macOS,
/// where `/usr/lib/...` isn't writable) RESOLVES what it installs: before this, `mcp add` wrote its
/// launcher into the override dir while `$PATH` kept the hardcoded default, so `which`/`type`/`ls
/// /bin` never saw the installed command. With the clank plug-in installed and no overrides set,
/// this is byte-identical to [`DEFAULT_PATH`](crate::config::vfs::DEFAULT_PATH) (unit-pinned in
/// `clank`). With no plug-in it is just `/usr/local/bin` — the core installs nothing.
#[must_use]
pub fn effective_path(dirs: &[PathBuf]) -> String {
    let mut path = String::from("/usr/local/bin");
    for dir in dirs {
        path.push(':');
        path.push_str(&dir.display().to_string());
    }
    path
}

/// One-time, best-effort filesystem layout at session start, for the core namespace plus `dirs` (the
/// installed plug-in's [`layout_dirs`](crate::plugin::Plugin::layout_dirs)).
///
/// The agent's per-instance VFS starts EMPTY — before this, `/tmp` existed only if a uu builtin's
/// capture path happened to run first, so a fresh agent's very first `curl -o /tmp/f` or
/// `echo x > /tmp/f` failed with "No such file or directory (os error 44)" until someone typed
/// `mkdir -p /tmp` (a live-demo gotcha). Create the whole README namespace up front. Idempotent
/// (`create_dir_all`) and replay-safe on the durable agent — whole-state directory creation, not an
/// append.
///
/// Native creates no absolute system path of its own (`/usr/lib/...` on macOS is not clank's to
/// create, and `/tmp` already exists on every host) — only what it is handed, which is why the
/// plug-in filters `layout_dirs` to the dirs the operator explicitly pointed somewhere writable via
/// a `CLANK_*` env override.
pub(super) fn ensure_fs_layout(dirs: &[PathBuf]) {
    #[cfg(target_arch = "wasm32")]
    for d in ["/tmp", "/var/log", DEFAULT_HOME, "/usr/local/bin"] {
        let _ = std::fs::create_dir_all(d);
    }
    for d in dirs {
        let _ = std::fs::create_dir_all(d);
    }
}

pub(super) async fn build_shell() -> Result<Shell, brush_core::Error> {
    // NB: the core's builtins are registered here AND their manifests in `registry::build()`; the
    // two must stay in lockstep (the registry drift-guard test enforces it). Adding a builtin via
    // `Shell::register_builtin` directly would bypass the manifest — don't. (A plug-in's builtins
    // and manifests arrive together through `Session::set_plugin`, guarded the same way.)
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
        .builtins(crate::builtins::context::builtins())
        .builtins(crate::builtins::interceptstub::builtins())
        .build()
        .await?;

    // Set clank's `$PATH` explicitly, overriding whatever Brush's init seeded (empty on the wasm
    // stub, the host's real PATH on native — both wrong for clank's virtual namespace). Read by
    // `$PATH` expansion and by `type`/`which` path resolution alike. The core half only; installing
    // a plug-in appends its own `path_dirs` (see `Session::set_plugin`).
    shell.env_mut().set_global(
        "PATH",
        brush_core::variables::ShellVariable::new(effective_path(&[])),
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
