//! `ask.toml`: the AI configuration file — the default model and provider entries. Read by `ask` to
//! resolve the model (and, on native, the provider API key), written by `model default` / `model add`.
//!
//! Location: `~/.config/ask/ask.toml` (README). On the Golem agent `$HOME` is seeded to `/home/user`;
//! natively it's the host's real home.
//!
//! **Provider API keys** (README §444: "stored in `~/.config/ask/ask.toml` on native or in Golem's
//! secrets API when running inside Golem"): on the **agent**, the key comes only from the environment
//! (`ANTHROPIC_API_KEY`, golem.yaml passthrough) — a key value is never written to the durable FS. On
//! **native**, the key may be stored in `[providers.<name>].key` (written by `model add <p> --key …`)
//! and is read by the native `ask` provider, falling back to the env var. `key-env` remains a
//! documentation field naming which env var a provider uses.
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
    /// Per-provider config: the `key-env` doc field and, on native, an optional stored `key` value.
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

/// A `[providers.<name>]` section: the doc-only `key-env` (which env var holds the key) plus, on
/// native, an optional stored `key` value the native `ask` provider reads (README §444).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProviderSection {
    /// Documentation: the env var this provider's key comes from (e.g. `ANTHROPIC_API_KEY`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    /// The stored API key (native only; written by `model add <p> --key …`). Never written by the
    /// agent, which reads the key from its environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
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

/// The stored API key for `provider` from `ask.toml` `[providers.<provider>].key`, if present. A
/// parse/read error is swallowed to `None` — a missing/broken key file is not an error here; the
/// caller falls back to the environment. Native only in practice (the agent never stores a key).
pub fn provider_key(home: &str, provider: &str) -> Option<String> {
    load(home)
        .ok()
        .flatten()
        .and_then(|c| c.providers.get(provider).and_then(|p| p.key.clone()))
}

/// Store `key` for `provider` in `ask.toml` `[providers.<provider>].key`, preserving the default
/// model and any other provider entries. Creates `~/.config/ask/` as needed. Native only — the agent
/// reads its key from the environment and never calls this.
pub fn save_provider_key(home: &str, provider: &str, key: &str) -> Result<(), String> {
    let mut config = load(home)?.unwrap_or_default();
    config
        .providers
        .entry(provider.to_string())
        .or_default()
        .key = Some(key.to_string());
    let path = config_path(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("ask.toml: cannot create {dir:?}: {e}"))?;
    }
    let text = toml::to_string_pretty(&config).map_err(|e| format!("ask.toml serialize error: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("ask.toml write error: {e}"))
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

    #[test]
    fn provider_key_round_trips() {
        let home = temp_home();
        let h = home.to_str().unwrap();
        assert_eq!(provider_key(h, "anthropic"), None);
        save_provider_key(h, "anthropic", "sk-native-key-123").unwrap();
        assert_eq!(provider_key(h, "anthropic").as_deref(), Some("sk-native-key-123"));
        // A different provider is unaffected.
        assert_eq!(provider_key(h, "openai"), None);
    }

    #[test]
    fn save_provider_key_preserves_default_model_and_key_env() {
        let home = temp_home();
        let h = home.to_str().unwrap();
        save_default_model(h, "claude-opus-4-8").unwrap();
        // Seed a key-env doc, then store a key — both survive.
        let path = config_path(h);
        let existing = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            format!("{existing}\n[providers.anthropic]\nkey-env = \"ANTHROPIC_API_KEY\"\n"),
        )
        .unwrap();
        save_provider_key(h, "anthropic", "sk-abc").unwrap();
        let cfg = load(h).unwrap().unwrap();
        assert_eq!(cfg.ask.default_model.as_deref(), Some("claude-opus-4-8"));
        let anthropic = cfg.providers.get("anthropic").unwrap();
        assert_eq!(anthropic.key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(anthropic.key.as_deref(), Some("sk-abc"));
    }
}
