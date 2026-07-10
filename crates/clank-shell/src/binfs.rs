//! The virtual `/bin` namespace: clank's builtins presented as read-only files.
//!
//! The README describes `/bin/` as a virtual read-only namespace for the shell's special builtins —
//! *not* file-backed (no bytes on disk), so the AI can `ls /bin` to enumerate every capability and
//! `cat /bin/<name>` to read its help. This module is that namespace: a pure resolver from a `/bin`
//! path to content, computed from clank's [`CommandRegistry`].
//!
//! **Static, unlike `/proc`.** The [`crate::procfs`] namespace reflects the *current* process table
//! (per-session, mutable) and is reached through a thread-local slot. The builtin *set* never changes
//! at runtime, so `/bin` resolves against a single lazily-built static snapshot of
//! [`crate::registry::build`] — no thread-local, no `Session` access needed. clank's own `cat`/`ls`
//! shim `/bin` reads through here exactly as they shim `/proc`, so the namespace stays virtual while
//! still composing with pipes (`ls /bin | grep`).
//!
//! Serves:
//! - `/bin`               — the directory: `ls /bin` lists every registered command name (sorted).
//! - `/bin/<name>`        — the command's manifest `help_text` (`cat`-able, `grep`-able).
//!
//! `/bin` is virtual, so it is deliberately NOT on `$PATH` and `which` never reports `/bin/<name>`
//! (which only walks real `$PATH` entries via `Path::exists`). `type` is the resolver for builtins;
//! `which` is for file-backed `$PATH` executables. This mirrors the README's split.

use std::sync::OnceLock;

use crate::registry::CommandRegistry;

/// Error resolving a `/bin` path — maps to a `cat`/`grep` "No such file or directory".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinError {
    NotFound(String),
}

/// The virtual root prefix.
const BIN_ROOT: &str = "/bin/";

/// The lazily-built static registry snapshot. Built once from [`crate::registry::build`], which is
/// pure (no host/native-only calls) and therefore sound on wasm.
fn registry() -> &'static CommandRegistry {
    static REGISTRY: OnceLock<CommandRegistry> = OnceLock::new();
    REGISTRY.get_or_init(crate::registry::build)
}

/// Whether `path` is under the virtual `/bin` namespace. (`/bin` itself and `/bin/` count too.)
pub fn is_bin_path(path: &str) -> bool {
    path == "/bin" || path.starts_with(BIN_ROOT)
}

/// Resolve a virtual `/bin/<name>` path to its content: the command's manifest `help_text` with a
/// trailing newline (so `cat /bin/curl` prints a clean help block). Unknown name → `NotFound`.
pub fn resolve(path: &str) -> Result<String, BinError> {
    let not_found = || BinError::NotFound(path.to_string());
    let rest = path.strip_prefix(BIN_ROOT).ok_or_else(not_found)?;
    let mut parts = rest.split('/').filter(|s| !s.is_empty());
    let name = parts.next().ok_or_else(not_found)?;
    // No deeper nesting: `/bin/<name>` is a leaf file, not a directory.
    if parts.next().is_some() {
        return Err(not_found());
    }
    match registry().get(name) {
        Some(m) => Ok(format!("{}\n", m.help_text)),
        None => Err(not_found()),
    }
}

/// List the children of a `/bin` directory: for `/bin` (or `/bin/`), every registered command name,
/// sorted. Returns `None` if `dir` isn't the `/bin` directory itself (e.g. `/bin/curl` is a file,
/// not a listable directory) — the caller then treats it as a file/not-found.
pub fn list_children(dir: &str) -> Option<Vec<String>> {
    let trimmed = dir.trim_end_matches('/');
    if trimmed != "/bin" {
        return None;
    }
    let mut names: Vec<String> = registry().names().map(String::from).collect();
    names.sort();
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_bin_path_recognizes_bin_only() {
        assert!(is_bin_path("/bin"));
        assert!(is_bin_path("/bin/"));
        assert!(is_bin_path("/bin/curl"));
        assert!(!is_bin_path("/tmp/x"));
        assert!(!is_bin_path("bin/curl"));
        assert!(!is_bin_path("/binary"));
    }

    #[test]
    fn resolve_returns_manifest_help_for_registered_command() {
        // An intercepted command...
        let out = resolve("/bin/curl").unwrap();
        assert!(out.contains("fetch a URL over"), "got: {out}");
        assert!(out.ends_with('\n'));
        // ...and a Brush-registered builtin (cat has a coreutils manifest).
        let out = resolve("/bin/cat").unwrap();
        assert!(!out.is_empty());
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn resolve_unknown_name_is_not_found() {
        assert_eq!(
            resolve("/bin/nope"),
            Err(BinError::NotFound("/bin/nope".into()))
        );
        // Nested path under a name is not served.
        assert!(matches!(resolve("/bin/curl/extra"), Err(BinError::NotFound(_))));
    }

    #[test]
    fn list_children_lists_every_command_sorted() {
        let names = list_children("/bin").unwrap();
        // Sorted.
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        // Contains both intercepted and registered builtins.
        assert!(names.iter().any(|n| n == "curl"));
        assert!(names.iter().any(|n| n == "prompt-user"));
        assert!(names.iter().any(|n| n == "cat"));
        // Matches the registry's size.
        assert_eq!(names.len(), registry().len());
    }

    #[test]
    fn list_children_only_lists_the_bin_directory() {
        assert!(list_children("/bin").is_some());
        assert!(list_children("/bin/").is_some());
        // A file, not a directory.
        assert!(list_children("/bin/curl").is_none());
        assert!(list_children("/proc").is_none());
    }
}
