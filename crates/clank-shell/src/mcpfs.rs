//! The virtual `/mnt/mcp/<server>/` namespace: MCP resources surfaced as files.
//!
//! An MCP server installed by `grease` (with `--resources`) mounts its resources here. Two flavors:
//!
//! - **Static** resources are materialized as **real files** at install time — `cat`/`grep` read them
//!   directly through uutils, and they compose in pipes with no MCP awareness (this module doesn't
//!   serve them; the filesystem does).
//! - **Dynamic** resources are **not** file-backed: their content must be fetched live via
//!   `resources/read` on each access. Because clank's `cat` is a synchronous Brush builtin with no
//!   reactor (the "Wall-C" wall — the same reason `ask`/`curl` can't run inside a pipeline), a dynamic
//!   read is served at the **Session interception layer** for a top-level `cat /mnt/mcp/...` line; it
//!   is NOT available inside `$(...)`/pipes.
//!
//! This module is a pure resolver from a path + the per-line **resource index** (a snapshot of the
//! installed servers' cached resources, installed thread-locally like [`crate::proctable`]) to the
//! virtual directory structure. It powers `ls /mnt/mcp/...` (listing static + dynamic entries) and
//! classifies a path as a static file, a dynamic resource (with its URI + server), or a directory.

use std::cell::RefCell;
use std::sync::Arc;

/// The virtual mount root.
const MCP_ROOT: &str = "/mnt/mcp";

/// One resource entry in the per-line index (a flattened view of an installed server's resources).
#[derive(Clone, Debug)]
pub struct ResourceEntry {
    /// The server name (the `/mnt/mcp/<server>/` segment).
    pub server: String,
    /// The path relative to `/mnt/mcp/<server>/` (e.g. `repo/README.md`).
    pub rel_path: String,
    /// The MCP resource URI (`resources/read` target) — used for a dynamic read.
    pub uri: String,
    /// `true` = a real static file already on disk; `false` = dynamic (read live).
    pub is_static: bool,
}

/// The classification of a `/mnt/mcp` path against the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpPathKind {
    /// A directory (the root, a server dir, or an intermediate dir) — its listed children.
    Directory(Vec<String>),
    /// A dynamic resource file: its `(server, uri)` — the caller fetches it live.
    Dynamic { server: String, uri: String },
    /// A static resource file: it's a real file on disk (the caller delegates to uutils).
    Static,
    /// No such path in the index.
    NotFound,
}

/// Whether `path` is under the virtual `/mnt/mcp` namespace.
pub fn is_mcp_path(path: &str) -> bool {
    path == MCP_ROOT || path.starts_with(&format!("{MCP_ROOT}/"))
}

/// Classify a `/mnt/mcp` path against `index` (the installed resources). Directories list their
/// children; a resource file is static or dynamic; anything else is `NotFound`.
pub fn classify(path: &str, index: &[ResourceEntry]) -> McpPathKind {
    let rel = path.trim_start_matches(MCP_ROOT).trim_start_matches('/').trim_end_matches('/');

    // The root: list the distinct server names.
    if rel.is_empty() {
        let mut servers: Vec<String> = index.iter().map(|e| e.server.clone()).collect();
        servers.sort();
        servers.dedup();
        return McpPathKind::Directory(servers);
    }

    // Split into `<server>/<subpath>`.
    let (server, subpath) = match rel.split_once('/') {
        Some((s, sub)) => (s.to_string(), sub.to_string()),
        None => (rel.to_string(), String::new()),
    };

    // Resources under this server.
    let under_server: Vec<&ResourceEntry> = index.iter().filter(|e| e.server == server).collect();
    if under_server.is_empty() {
        return McpPathKind::NotFound;
    }

    // An exact resource match → static file or dynamic resource.
    if let Some(entry) = under_server.iter().find(|e| e.rel_path == subpath) {
        return if entry.is_static {
            McpPathKind::Static
        } else {
            McpPathKind::Dynamic { server, uri: entry.uri.clone() }
        };
    }

    // Otherwise a directory: list the immediate children of `subpath` (or the server root).
    let prefix = if subpath.is_empty() { String::new() } else { format!("{subpath}/") };
    let mut children: Vec<String> = Vec::new();
    for e in &under_server {
        if let Some(rest) = e.rel_path.strip_prefix(&prefix) {
            // The next path segment under `prefix`.
            let seg = rest.split('/').next().unwrap_or(rest);
            if !seg.is_empty() {
                children.push(seg.to_string());
            }
        }
    }
    children.sort();
    children.dedup();
    if children.is_empty() {
        McpPathKind::NotFound
    } else {
        McpPathKind::Directory(children)
    }
}

thread_local! {
    /// The transient per-line MCP resource index. Populated by [`install`] for the duration of one
    /// `run_line` and read by `cat`/`ls` via [`active`]. Thread-local so parallel Sessions (native
    /// tests) don't collide. Mirrors [`crate::proctable`]'s ACTIVE slot.
    static ACTIVE: RefCell<Option<Arc<Vec<ResourceEntry>>>> = const { RefCell::new(None) };
}

/// Install `index` as the active MCP resource index for the current line; the guard restores the
/// previous slot on drop.
#[must_use]
pub fn install(index: Arc<Vec<ResourceEntry>>) -> InstallGuard {
    let previous = ACTIVE.with(|slot| slot.borrow_mut().replace(index));
    InstallGuard { previous }
}

/// The active index, if a line is executing on this thread.
pub fn active() -> Option<Arc<Vec<ResourceEntry>>> {
    ACTIVE.with(|slot| slot.borrow().clone())
}

/// Restores the previous active-index slot when dropped.
pub struct InstallGuard {
    previous: Option<Arc<Vec<ResourceEntry>>>,
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        ACTIVE.with(|slot| *slot.borrow_mut() = previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx() -> Vec<ResourceEntry> {
        vec![
            ResourceEntry {
                server: "github".into(),
                rel_path: "repo/README.md".into(),
                uri: "file:///repo/README.md".into(),
                is_static: true,
            },
            ResourceEntry {
                server: "metrics".into(),
                rel_path: "current/cpu-usage".into(),
                uri: "metrics://current/cpu-usage".into(),
                is_static: false,
            },
        ]
    }

    #[test]
    fn root_lists_servers() {
        assert_eq!(
            classify("/mnt/mcp", &idx()),
            McpPathKind::Directory(vec!["github".into(), "metrics".into()])
        );
    }

    #[test]
    fn server_and_subdir_listings() {
        assert_eq!(classify("/mnt/mcp/github", &idx()), McpPathKind::Directory(vec!["repo".into()]));
        assert_eq!(
            classify("/mnt/mcp/github/repo", &idx()),
            McpPathKind::Directory(vec!["README.md".into()])
        );
    }

    #[test]
    fn static_and_dynamic_files() {
        assert_eq!(classify("/mnt/mcp/github/repo/README.md", &idx()), McpPathKind::Static);
        assert_eq!(
            classify("/mnt/mcp/metrics/current/cpu-usage", &idx()),
            McpPathKind::Dynamic {
                server: "metrics".into(),
                uri: "metrics://current/cpu-usage".into()
            }
        );
    }

    #[test]
    fn unknown_paths() {
        assert_eq!(classify("/mnt/mcp/nope", &idx()), McpPathKind::NotFound);
        assert_eq!(classify("/mnt/mcp/github/missing.txt", &idx()), McpPathKind::NotFound);
    }

    #[test]
    fn is_mcp_path_matches_root_and_children() {
        assert!(is_mcp_path("/mnt/mcp"));
        assert!(is_mcp_path("/mnt/mcp/github/repo/README.md"));
        assert!(!is_mcp_path("/mnt/other"));
        assert!(!is_mcp_path("/proc/1/status"));
    }
}
