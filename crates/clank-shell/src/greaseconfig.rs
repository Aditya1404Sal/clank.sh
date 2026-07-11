//! `grease` on-disk state: the registry URL list and the per-package config, under `/etc/grease/`.
//!
//! Mirrors [`crate::mcpconfig`]. Three directories, each overridable via an env var so native tests
//! are hermetic; on the agent the defaults are used (and the agent FS is durable, so what grease
//! writes survives across invocations):
//!
//! - `$CLANK_GREASE_ETC`   (default `/etc/grease`)        — `registries.toml` + one `<name>.toml`/pkg.
//! - `$CLANK_GREASE_STORE` (default `/var/lib/grease`)    — the versioned payload store (Phase 2).
//! - `$CLANK_GREASE_BIN`   (default `/usr/lib/prompts/bin`) — the inert bin stubs (already on `$PATH`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default config directory (`registries.toml` + one `<name>.toml` per installed package).
pub const DEFAULT_ETC: &str = "/etc/grease";
/// Default payload store (`<store>/<name>/prompt.md`), the source of truth for derived executables.
pub const DEFAULT_STORE: &str = "/var/lib/grease";
/// Default generated-command directory (`/usr/lib/prompts/bin/<name>`), already on `$PATH`.
pub const DEFAULT_BIN: &str = "/usr/lib/prompts/bin";

/// The registry list — configured registry URLs `grease install`/`search` fetch from, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registries {
    #[serde(default)]
    pub registry: Vec<RegistryEntry>,
}

/// One configured registry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub url: String,
}

/// A process-global lock serializing tests that set `$CLANK_GREASE_*` (process-global env vars, so
/// parallel native tests must not race). Mirrors [`crate::mcpconfig::TEST_ENV_LOCK`].
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The config directory, honoring `$CLANK_GREASE_ETC`.
pub fn etc_dir() -> PathBuf {
    PathBuf::from(std::env::var("CLANK_GREASE_ETC").unwrap_or_else(|_| DEFAULT_ETC.to_string()))
}

/// The payload store directory, honoring `$CLANK_GREASE_STORE`.
pub fn store_dir() -> PathBuf {
    PathBuf::from(std::env::var("CLANK_GREASE_STORE").unwrap_or_else(|_| DEFAULT_STORE.to_string()))
}

/// The generated-command directory, honoring `$CLANK_GREASE_BIN`.
pub fn bin_dir() -> PathBuf {
    PathBuf::from(std::env::var("CLANK_GREASE_BIN").unwrap_or_else(|_| DEFAULT_BIN.to_string()))
}

/// The `registries.toml` path.
fn registries_path() -> PathBuf {
    etc_dir().join("registries.toml")
}

/// Whether `name` is a valid kebab-case package name (`[a-z0-9-]+`, not empty, no leading/trailing `-`).
/// Same rule as [`crate::mcpconfig::is_valid_name`].
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Load the registry list. `Ok(default)` (empty) if the file doesn't exist.
pub fn load_registries() -> Result<Registries, String> {
    match std::fs::read_to_string(registries_path()) {
        Ok(text) => toml::from_str(&text).map_err(|e| format!("registries.toml parse error: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registries::default()),
        Err(e) => Err(format!("registries.toml read error: {e}")),
    }
}

/// Persist the registry list (creating `/etc/grease/` as needed).
fn save_registries(regs: &Registries) -> Result<(), String> {
    let dir = etc_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
    let text = toml::to_string_pretty(regs).map_err(|e| format!("serialize error: {e}"))?;
    std::fs::write(registries_path(), text).map_err(|e| format!("write error: {e}"))
}

/// Add a registry URL (idempotent — a duplicate URL is a no-op). Returns whether it was newly added.
pub fn add_registry(url: &str) -> Result<bool, String> {
    let mut regs = load_registries()?;
    if regs.registry.iter().any(|r| r.url == url) {
        return Ok(false);
    }
    regs.registry.push(RegistryEntry { url: url.to_string() });
    save_registries(&regs)?;
    Ok(true)
}

/// Remove a registry URL. Returns whether it was present.
pub fn remove_registry(url: &str) -> Result<bool, String> {
    let mut regs = load_registries()?;
    let before = regs.registry.len();
    regs.registry.retain(|r| r.url != url);
    let removed = regs.registry.len() != before;
    if removed {
        save_registries(&regs)?;
    }
    Ok(removed)
}

/// The configured registry URLs, in order.
pub fn list_registries() -> Vec<String> {
    load_registries()
        .map(|r| r.registry.into_iter().map(|e| e.url).collect())
        .unwrap_or_default()
}

/// Write the inert `/usr/lib/prompts/bin/<name>` stub file (a managed-by header + help text). A real
/// file so `which`/`ls`/`cat`/`type` see it with no virtual-fs code — but it is NEVER executed (wasip2
/// has no process spawn); the command runs via Session interception. Mirrors
/// [`crate::mcpconfig::write_bin_stub`].
pub fn write_bin_stub(name: &str, help: &str) -> Result<(), String> {
    let dir = bin_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
    let content = format!(
        "# clank prompt (package: {name}, managed by `grease`; runs at the session layer)\n{help}"
    );
    std::fs::write(dir.join(name), content).map_err(|e| format!("write stub error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique() -> u64 {
        static C: AtomicU64 = AtomicU64::new(0);
        C.fetch_add(1, Ordering::Relaxed)
    }

    /// Point the grease dirs at fresh temp dirs for the duration of `f`.
    pub(crate) fn with_temp_dirs<F: FnOnce()>(f: F) {
        let _guard = super::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let n = unique();
        let base = std::env::temp_dir().join(format!("clank_grease_{}_{n}", std::process::id()));
        for sub in ["etc", "store", "bin"] {
            std::fs::create_dir_all(base.join(sub)).unwrap();
        }
        std::env::set_var("CLANK_GREASE_ETC", base.join("etc"));
        std::env::set_var("CLANK_GREASE_STORE", base.join("store"));
        std::env::set_var("CLANK_GREASE_BIN", base.join("bin"));
        f();
        std::env::remove_var("CLANK_GREASE_ETC");
        std::env::remove_var("CLANK_GREASE_STORE");
        std::env::remove_var("CLANK_GREASE_BIN");
    }

    #[test]
    fn valid_name_rules() {
        assert!(is_valid_name("summarize"));
        assert!(is_valid_name("code-review-2"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("-lead"));
        assert!(!is_valid_name("trail-"));
        assert!(!is_valid_name("Upper"));
        assert!(!is_valid_name("has_underscore"));
    }

    #[test]
    fn registry_add_list_remove_round_trip() {
        with_temp_dirs(|| {
            assert!(list_registries().is_empty());
            assert!(add_registry("https://reg.example").unwrap());
            // Duplicate is a no-op.
            assert!(!add_registry("https://reg.example").unwrap());
            assert!(add_registry("https://other.example").unwrap());
            assert_eq!(
                list_registries(),
                vec!["https://reg.example".to_string(), "https://other.example".to_string()]
            );
            assert!(remove_registry("https://reg.example").unwrap());
            assert!(!remove_registry("https://reg.example").unwrap()); // already gone
            assert_eq!(list_registries(), vec!["https://other.example".to_string()]);
        });
    }

    #[test]
    fn write_bin_stub_creates_readable_file() {
        with_temp_dirs(|| {
            write_bin_stub("summarize", "usage: summarize [--file F]\n").unwrap();
            let content = std::fs::read_to_string(bin_dir().join("summarize")).unwrap();
            assert!(content.contains("managed by `grease`"));
            assert!(content.contains("usage: summarize"));
        });
    }
}
