//! `grease` on-disk state: the registry URL list and the per-package config, under `/etc/grease/`.
//!
//! Mirrors [`crate::mcpconfig`]. Three directories, each overridable via an env var so native tests
//! are hermetic; on the agent the defaults are used (and the agent FS is durable, so what grease
//! writes survives across invocations):
//!
//! - `$CLANK_GREASE_ETC`        (default `/etc/grease`)          — `registries.toml` + one `<name>.toml`/pkg.
//! - `$CLANK_GREASE_STORE`      (default `/var/lib/grease`)      — the versioned payload store.
//! - `$CLANK_GREASE_BIN`        (default `/usr/lib/prompts/bin`) — prompt bin stubs (already on `$PATH`).
//! - `$CLANK_GREASE_SCRIPT_BIN` (default `/usr/bin`)             — script bin stubs (already on `$PATH`).
//! - `$CLANK_GREASE_SKILLS`     (default `/usr/share/skills`)    — skill dirs (`<name>/`, `<name>/bin/`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default config directory (`registries.toml` + one `<name>.toml` per installed package).
pub const DEFAULT_ETC: &str = "/etc/grease";
/// Default payload store (`<store>/<name>/<kind>.json`), the source of truth for derived executables.
pub const DEFAULT_STORE: &str = "/var/lib/grease";
/// Default prompt bin directory (`/usr/lib/prompts/bin/<name>`), already on `$PATH`.
pub const DEFAULT_BIN: &str = "/usr/lib/prompts/bin";
/// Default script bin directory (`/usr/bin/<name>`), already on `$PATH`.
pub const DEFAULT_SCRIPT_BIN: &str = "/usr/bin";
/// Default skills directory (`/usr/share/skills/<name>/`), whose `*/bin` glob is already on `$PATH`.
pub const DEFAULT_SKILLS: &str = "/usr/share/skills";
/// Default MCP resource mount root (`/mnt/mcp/<server>/`) — where an MCP server's resources are
/// materialized (static files) and its dynamic/template stubs are surfaced (README:669).
pub const DEFAULT_MCP_MOUNT: &str = "/mnt/mcp";

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
    /// The registry's trusted ed25519 public key (base64), if configured via `grease registry add
    /// --key`. When present, `grease install` verifies each package's detached signature against it;
    /// absent ⇒ the registry is unsigned (record-only integrity). Defaults `None` for entries written
    /// before signing existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
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

/// The prompt bin directory, honoring `$CLANK_GREASE_BIN`.
pub fn bin_dir() -> PathBuf {
    PathBuf::from(std::env::var("CLANK_GREASE_BIN").unwrap_or_else(|_| DEFAULT_BIN.to_string()))
}

/// The script bin directory (`/usr/bin`), honoring `$CLANK_GREASE_SCRIPT_BIN`.
pub fn script_bin_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("CLANK_GREASE_SCRIPT_BIN").unwrap_or_else(|_| DEFAULT_SCRIPT_BIN.to_string()),
    )
}

/// The skills directory (`/usr/share/skills`), honoring `$CLANK_GREASE_SKILLS`.
pub fn skills_dir() -> PathBuf {
    PathBuf::from(std::env::var("CLANK_GREASE_SKILLS").unwrap_or_else(|_| DEFAULT_SKILLS.to_string()))
}

/// The MCP resource mount root (`/mnt/mcp`), honoring `$CLANK_GREASE_MCP_MOUNT`.
pub fn mcp_mount_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("CLANK_GREASE_MCP_MOUNT").unwrap_or_else(|_| DEFAULT_MCP_MOUNT.to_string()),
    )
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

/// Add a registry URL with an optional trusted signing key (base64 ed25519 public key). Idempotent on
/// the URL, but re-adding a known URL WITH a `key` updates its key (so `grease registry add <url>
/// --key <k>` can attach a key to an existing entry). Returns whether the list changed.
pub fn add_registry(url: &str, key: Option<&str>) -> Result<bool, String> {
    let mut regs = load_registries()?;
    if let Some(existing) = regs.registry.iter_mut().find(|r| r.url == url) {
        // Known URL: update the key if one was supplied and it differs; otherwise a no-op.
        let new_key = key.map(|k| k.to_string());
        if key.is_some() && existing.key != new_key {
            existing.key = new_key;
            save_registries(&regs)?;
            return Ok(true);
        }
        return Ok(false);
    }
    regs.registry.push(RegistryEntry { url: url.to_string(), key: key.map(|k| k.to_string()) });
    save_registries(&regs)?;
    Ok(true)
}

/// The trusted signing key (base64 ed25519 public key) configured for `url`, if any.
pub fn registry_key(url: &str) -> Option<String> {
    load_registries().ok()?.registry.into_iter().find(|r| r.url == url).and_then(|r| r.key)
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

/// Write an inert bin stub at `<dir>/<name>` (a managed-by header + help text). A real file so
/// `which`/`ls`/`cat`/`type` see it with no virtual-fs code — but it is NEVER executed (wasip2 has no
/// process spawn); the command runs via Session interception. `label` names the kind in the header
/// (`prompt`/`script`). Mirrors [`crate::mcpconfig::write_bin_stub`].
pub fn write_bin_stub(dir: &Path, name: &str, help: &str, label: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
    let content = format!(
        "# clank {label} (package: {name}, managed by `grease`; runs at the session layer)\n{help}"
    );
    std::fs::write(dir.join(name), content).map_err(|e| format!("write stub error: {e}"))
}

/// Materialize a skill's on-disk surface under `<skills>/<name>/`: write each reference document
/// verbatim (relative to the skill dir) and deposit each bundled script to `<name>/bin/<script>` (on
/// `$PATH`). Best-effort; the durable payload is already persisted in the store. Any document `path`
/// that escapes the skill dir (`..` / absolute) is skipped (defense-in-depth against a hostile
/// registry).
pub fn materialize_skill(sk: &crate::greasepkg::SkillPackage) -> Result<(), String> {
    let root = skills_dir().join(&sk.name);
    std::fs::create_dir_all(&root).map_err(|e| format!("cannot create {root:?}: {e}"))?;
    for doc in &sk.documents {
        let Some(dest) = safe_join(&root, &doc.path) else {
            continue; // path escapes the skill dir — skip
        };
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&dest, &doc.content);
    }
    if !sk.scripts.is_empty() {
        let bin = root.join("bin");
        let _ = std::fs::create_dir_all(&bin);
        for script in &sk.scripts {
            // Bundled script names must be simple (no path components) — skip anything suspicious.
            if script.name.is_empty() || script.name.contains('/') || script.name.contains("..") {
                continue;
            }
            let _ = std::fs::write(bin.join(&script.name), &script.body);
        }
    }
    Ok(())
}

/// Join `rel` onto `root`, returning `None` if the result escapes `root` (absolute path or `..`
/// component). Keeps a skill's documents confined to its own directory.
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return None;
    }
    if rel_path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return None;
    }
    Some(root.join(rel_path))
}

/// Public path-confinement join for MCP resource materialization under `/mnt/mcp/<server>/` — same
/// `..`/absolute-escape guard as [`safe_join`], keeping a server's resources inside its mount dir.
pub fn mcp_safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    safe_join(root, rel)
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
        for sub in ["etc", "store", "bin", "script-bin", "skills"] {
            std::fs::create_dir_all(base.join(sub)).unwrap();
        }
        std::env::set_var("CLANK_GREASE_ETC", base.join("etc"));
        std::env::set_var("CLANK_GREASE_STORE", base.join("store"));
        std::env::set_var("CLANK_GREASE_BIN", base.join("bin"));
        std::env::set_var("CLANK_GREASE_SCRIPT_BIN", base.join("script-bin"));
        std::env::set_var("CLANK_GREASE_SKILLS", base.join("skills"));
        f();
        std::env::remove_var("CLANK_GREASE_ETC");
        std::env::remove_var("CLANK_GREASE_STORE");
        std::env::remove_var("CLANK_GREASE_BIN");
        std::env::remove_var("CLANK_GREASE_SCRIPT_BIN");
        std::env::remove_var("CLANK_GREASE_SKILLS");
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
            assert!(add_registry("https://reg.example", None).unwrap());
            // Duplicate (no key) is a no-op.
            assert!(!add_registry("https://reg.example", None).unwrap());
            assert!(add_registry("https://other.example", None).unwrap());
            assert_eq!(
                list_registries(),
                vec!["https://reg.example".to_string(), "https://other.example".to_string()]
            );
            // Re-adding a known URL WITH a key updates it (returns changed=true); the key is readable.
            assert!(add_registry("https://reg.example", Some("KEYDATA")).unwrap());
            assert_eq!(registry_key("https://reg.example").as_deref(), Some("KEYDATA"));
            assert_eq!(registry_key("https://other.example"), None);
            assert!(remove_registry("https://reg.example").unwrap());
            assert!(!remove_registry("https://reg.example").unwrap()); // already gone
            assert_eq!(list_registries(), vec!["https://other.example".to_string()]);
        });
    }

    #[test]
    fn write_bin_stub_creates_readable_file() {
        with_temp_dirs(|| {
            write_bin_stub(&bin_dir(), "summarize", "usage: summarize [--file F]\n", "prompt").unwrap();
            let content = std::fs::read_to_string(bin_dir().join("summarize")).unwrap();
            assert!(content.contains("managed by `grease`"));
            assert!(content.contains("clank prompt"));
            assert!(content.contains("usage: summarize"));
        });
    }

    #[test]
    fn script_stub_lands_in_the_script_bin_dir() {
        with_temp_dirs(|| {
            write_bin_stub(&script_bin_dir(), "deploy", "usage: deploy\n", "script").unwrap();
            let content = std::fs::read_to_string(script_bin_dir().join("deploy")).unwrap();
            assert!(content.contains("clank script"));
            // It goes to the script bin dir, not the prompt bin dir.
            assert!(!bin_dir().join("deploy").exists());
        });
    }

    #[test]
    fn materialize_skill_writes_docs_and_scripts_and_confines_paths() {
        use crate::greasepkg::{SkillDocument, SkillPackage, SkillScript};
        with_temp_dirs(|| {
            let sk = SkillPackage {
                name: "code-review".into(),
                description: "d".into(),
                intended_use: None,
                documents: vec![
                    SkillDocument { path: "SKILL.md".into(), content: "# how".into() },
                    SkillDocument { path: "reference/api.md".into(), content: "api".into() },
                    // Escaping paths are skipped.
                    SkillDocument { path: "../escape.md".into(), content: "nope".into() },
                    SkillDocument { path: "/abs.md".into(), content: "nope".into() },
                ],
                scripts: vec![SkillScript { name: "lint-all".into(), body: "echo lint".into() }],
            };
            materialize_skill(&sk).unwrap();
            let root = skills_dir().join("code-review");
            assert_eq!(std::fs::read_to_string(root.join("SKILL.md")).unwrap(), "# how");
            assert_eq!(std::fs::read_to_string(root.join("reference/api.md")).unwrap(), "api");
            assert_eq!(std::fs::read_to_string(root.join("bin/lint-all")).unwrap(), "echo lint");
            // The escaping (`..`) doc was skipped — not written into the skills dir's parent.
            assert!(!skills_dir().join("escape.md").exists());
            // The absolute-path doc was skipped too — only the two confined docs and the bin script
            // exist under the skill root.
            let mut entries: Vec<String> = std::fs::read_dir(&root)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            entries.sort();
            assert_eq!(entries, vec!["SKILL.md", "bin", "reference"]);
        });
    }
}
