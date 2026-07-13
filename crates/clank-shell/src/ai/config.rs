//! `ask.toml`: the AI configuration file — the default model and (documentation-only) provider
//! entries. Read by `ask` to resolve the model, written by `model default`.
//!
//! Location: `~/.config/ask/ask.toml` (README). On the Golem agent `$HOME` is seeded to `/home/user`;
//! natively it's the host's real home. **Provider API keys are never stored here** — the agent reads
//! `ANTHROPIC_API_KEY` from its environment (golem.yaml passthrough); the `[providers]` section is
//! purely documentation of which env var a provider uses.
//!
//! The schema is forgiving: unknown keys are tolerated (forward-compatible), a missing file is
//! `Ok(None)` (fall back to the built-in default), and a parse error is surfaced so callers can warn
//! and fall back rather than silently mis-resolve.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The parsed `ask.toml`. All sections default so a partial file round-trips.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskConfig {
    #[serde(default)]
    pub ask: AskSection,
    /// Documentation-only: which env var each provider's key comes from. Never holds a key value.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderSection>,
}

/// The `[ask]` section: the default model.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AskSection {
    /// The default model id (`provider/model` or a bare id). `None` ⇒ use the built-in default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}

/// A `[providers.<name>]` section: documentation of the key's env var (never the key itself).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProviderSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
}

/// The `ask.toml` path under a given home directory: `<home>/.config/ask/ask.toml`.
pub fn config_path(home: &str) -> PathBuf {
    PathBuf::from(home).join(".config").join("ask").join("ask.toml")
}

/// Load `ask.toml` from `home`. `Ok(None)` if the file doesn't exist (use the built-in default);
/// `Err(msg)` on an unreadable or unparseable file (the caller should warn and fall back).
pub fn load(home: &str) -> Result<Option<AskConfig>, String> {
    let path = config_path(home);
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text)
            .map(Some)
            .map_err(|e| format!("ask.toml parse error: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("ask.toml read error: {e}")),
    }
}

/// Set the default model in `ask.toml` under `home`, preserving the existing `[providers]` entries.
/// Creates `~/.config/ask/` as needed. Never writes a key value.
pub fn save_default_model(home: &str, model: &str) -> Result<(), String> {
    let mut config = load(home)?.unwrap_or_default();
    config.ask.default_model = Some(model.to_string());
    let path = config_path(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("ask.toml: cannot create {dir:?}: {e}"))?;
    }
    let text = toml::to_string_pretty(&config).map_err(|e| format!("ask.toml serialize error: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("ask.toml write error: {e}"))
}

/// The default model resolved from `ask.toml`, if any. A parse error returns `Err` so the caller can
/// warn once and fall back to the built-in default.
pub fn default_model(home: &str) -> Result<Option<String>, String> {
    Ok(load(home)?.and_then(|c| c.ask.default_model))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh temp home dir, unique per call (atomic counter — collision-free across parallel tests).
    fn temp_home() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("clank_askcfg_{}_{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_is_none() {
        let home = temp_home();
        assert_eq!(load(home.to_str().unwrap()).unwrap(), None);
        assert_eq!(default_model(home.to_str().unwrap()).unwrap(), None);
    }

    #[test]
    fn save_then_load_round_trips_the_default() {
        let home = temp_home();
        let h = home.to_str().unwrap();
        save_default_model(h, "anthropic/claude-sonnet-4-5").unwrap();
        assert_eq!(
            default_model(h).unwrap().as_deref(),
            Some("anthropic/claude-sonnet-4-5")
        );
    }

    #[test]
    fn save_preserves_provider_docs() {
        let home = temp_home();
        let h = home.to_str().unwrap();
        // Seed a config with a provider doc entry, then update the default model.
        let path = config_path(h);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "[providers.anthropic]\nkey-env = \"ANTHROPIC_API_KEY\"\n",
        )
        .unwrap();
        save_default_model(h, "claude-opus-4-8").unwrap();
        let cfg = load(h).unwrap().unwrap();
        assert_eq!(cfg.ask.default_model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(
            cfg.providers.get("anthropic").and_then(|p| p.key_env.as_deref()),
            Some("ANTHROPIC_API_KEY")
        );
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        let home = temp_home();
        let h = home.to_str().unwrap();
        let path = config_path(h);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "[ask]\ndefault-model = \"x\"\nfuture-field = 7\n[unknown-section]\nk = 1\n",
        )
        .unwrap();
        // Forward-compatible: unknown keys/sections don't break parsing.
        assert_eq!(default_model(h).unwrap().as_deref(), Some("x"));
    }

    #[test]
    fn garbage_file_is_an_error() {
        let home = temp_home();
        let h = home.to_str().unwrap();
        let path = config_path(h);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "this is not = = toml [[[").unwrap();
        assert!(load(h).is_err());
    }
}
