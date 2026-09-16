//! What the plug-in contributes to a session's surface, tested against the hooks themselves: the
//! `$PATH` half it supplies and the system prompt it renders. Both were pinned against the core
//! before the families moved behind the seam.
//!
//! Fixtures live in the parent module ([`super`]).

use crate::clank::Clank;
use crate::plugin::Plugin as _;

/// `$PATH` with clank installed is the documented README default, byte for byte: the core's
/// `/usr/local/bin` followed by the plug-in's [`path_dirs`] in order. The drift guard for a
/// `$PATH` that is now built in two halves.
///
/// House lock order: grease, then mcp (see `super::resolution`) — a test that reads the
/// env-overridable dirs must hold both, and always in that order, or the suite deadlocks AB-BA.
///
/// [`path_dirs`]: crate::plugin::Plugin::path_dirs
#[test]
fn installed_path_is_the_readme_default() {
    let _grease = crate::grease::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _mcp = crate::mcp::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let clank = Clank::default();
    assert_eq!(
        crate::session::env::effective_path(&clank.path_dirs()),
        crate::config::vfs::DEFAULT_PATH
    );
}

/// A `CLANK_MCP_BIN` override lands in `$PATH`, so a native session RESOLVES what `mcp add`
/// installs — before this, the launcher went to the override dir while `$PATH` kept the
/// hardcoded default, and `which <server>` never saw it.
#[test]
fn path_dirs_honor_the_mcp_bin_override() {
    let _lock = crate::mcp::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("mcp-bin");
    std::env::set_var("CLANK_MCP_BIN", &bin);
    let path = crate::session::env::effective_path(&Clank::default().path_dirs());
    std::env::remove_var("CLANK_MCP_BIN");
    assert!(
        path.contains(bin.to_str().unwrap()),
        "PATH should contain the override: {path}"
    );
    assert!(
        !path.contains("/usr/lib/mcp/bin"),
        "default entry should be replaced: {path}"
    );
}

/// The system prompt `/proc/clank/system-prompt` serves is the plug-in's to render: the fixed
/// preamble plus the rendered command surface over the session registry (core + plug-in). `ask`
/// itself is a Subprocess command with a `[confirm]` marker; `shell` is the one tool.
#[test]
fn capabilities_render_the_system_prompt_over_the_command_surface() {
    let clank = Clank::default();
    let mut registry = crate::registry::build();
    for manifest in clank.manifests() {
        registry.insert(manifest);
    }
    let out = clank
        .capabilities(&registry)
        .system_prompt
        .expect("clank renders a system prompt");
    assert!(out.contains("You are clank"), "got: {out}");
    assert!(
        out.contains("`shell`"),
        "should describe the shell tool, got: {out}"
    );
    assert!(
        out.contains("ask —"),
        "should list ask in the surface, got: {out}"
    );
    assert!(out.contains("[confirm]"), "ask is confirm-tier, got: {out}");
}
