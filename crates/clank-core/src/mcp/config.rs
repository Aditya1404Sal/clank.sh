//! MCP server configuration: one TOML file per server under `/etc/mcp/`.
//!
//! A server config records the HTTPS `url`, whether it's `enabled`, and (optionally) which environment
//! variable holds its auth token and which header to send it in. **Token values are never stored** —
//! only the env var name; the value is read from the environment at request time.
//!
//! Directories are overridable via `$CLANK_MCP_ETC` / `$CLANK_MCP_BIN` (read per call) so native tests
//! are hermetic; on the agent the defaults (`/etc/mcp`, `/usr/lib/mcp/bin`) are used.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default config directory (one `<name>.toml` per server).
pub const DEFAULT_ETC: &str = "/etc/mcp";
/// Default generated-command directory (`/usr/lib/mcp/bin/<server>`), already on `$PATH`.
pub const DEFAULT_BIN: &str = "/usr/lib/mcp/bin";

/// A parsed `<server>.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct McpServerConfig {
    /// The server's HTTPS endpoint.
    pub url: String,
    /// Whether the server is active (default true).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The environment variable NAME holding the auth token (never a value). `None` ⇒ no auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_env: Option<String>,
    /// The header to send the token in. Default `Authorization` (sent as `Bearer <token>`); any other
    /// header sends the raw env value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<String>,
    /// The server's tool list as fetched at install time — the cache that lets a NEW process
    /// reconstruct the server without network (see `Session::reconstruct_mcp_from_configs`).
    /// Absent in configs written before this field existed (serde default = empty = no cache).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<StoredTool>,
}

/// A cached tool definition inside a server config — name, description, and the JSON
/// `inputSchema` stored as a JSON string (the same representation the grease tool cache uses).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct StoredTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub input_schema: String,
}

fn default_true() -> bool {
    true
}

impl McpServerConfig {
    /// A minimal config for `mcp add <name> <url>`.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            enabled: true,
            auth_env: None,
            auth_header: None,
            tools: Vec::new(),
        }
    }

    /// Resolve the auth (header + value) from the environment, if `auth_env` is set and present. The
    /// default header is `Authorization` with a `Bearer ` prefix; a custom header sends the raw value.
    pub fn resolve_auth(&self) -> Option<crate::mcp::client::McpAuth> {
        let var = self.auth_env.as_ref()?;
        let value = std::env::var(var).ok()?;
        let header = self.auth_header.clone().unwrap_or_else(|| "Authorization".to_string());
        let value = if header.eq_ignore_ascii_case("Authorization") {
            format!("Bearer {value}")
        } else {
            value
        };
        Some(crate::mcp::client::McpAuth { header, value })
    }
}

/// A process-global lock serializing tests that set `$CLANK_MCP_ETC/$CLANK_MCP_BIN` (both this
/// module's and the Session's MCP tests share it, since the env vars are process-global).
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The config directory, honoring `$CLANK_MCP_ETC`.
pub fn etc_dir() -> PathBuf {
    PathBuf::from(std::env::var("CLANK_MCP_ETC").unwrap_or_else(|_| DEFAULT_ETC.to_string()))
}

/// The generated-command directory, honoring `$CLANK_MCP_BIN`.
pub fn bin_dir() -> PathBuf {
    PathBuf::from(std::env::var("CLANK_MCP_BIN").unwrap_or_else(|_| DEFAULT_BIN.to_string()))
}

/// The config path for a server name.
pub fn config_path(name: &str) -> PathBuf {
    etc_dir().join(format!("{name}.toml"))
}

/// Whether `name` is a valid kebab-case server name (`[a-z0-9-]+`, not empty, no leading/trailing `-`).
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Write a server config (creating `/etc/mcp/` as needed).
pub fn save(name: &str, config: &McpServerConfig) -> Result<(), String> {
    let dir = etc_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
    let text = toml::to_string_pretty(config).map_err(|e| format!("serialize error: {e}"))?;
    std::fs::write(config_path(name), text).map_err(|e| format!("write error: {e}"))
}

/// Remove a server's config file (and, if present, its generated `/usr/lib/mcp/bin` stub).
pub fn remove(name: &str) -> Result<(), String> {
    let path = config_path(name);
    std::fs::remove_file(&path).map_err(|e| format!("cannot remove {path:?}: {e}"))?;
    let _ = std::fs::remove_file(bin_dir().join(name));
    Ok(())
}

/// Load one server config by name. `Ok(None)` if the file doesn't exist.
pub fn load(name: &str) -> Result<Option<McpServerConfig>, String> {
    match std::fs::read_to_string(config_path(name)) {
        Ok(text) => toml::from_str(&text)
            .map(Some)
            .map_err(|e| format!("{name}.toml parse error: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{name}.toml read error: {e}")),
    }
}

/// All configured server names (the stems of `*.toml` in the config dir), sorted.
pub fn list_names() -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(etc_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

/// Write the generated `/usr/lib/mcp/bin/<name>` stub file (help text + a managed-by header). This is
/// a real file so `which`/`ls`/`cat`/`type` see it with no new virtual-fs code.
pub fn write_bin_stub(name: &str, help: &str) -> Result<(), String> {
    let dir = bin_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
    let content = format!(
        "# clank MCP command (server: {name}, managed by `mcp`; runs at the session layer)\n{help}"
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

    /// Point the config/bin dirs at fresh temp dirs for the duration of `f`.
    fn with_temp_dirs<F: FnOnce(&str)>(f: F) {
        let _guard = super::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let n = unique();
        let base = std::env::temp_dir().join(format!("clank_mcpcfg_{}_{n}", std::process::id()));
        let etc = base.join("etc");
        let bin = base.join("bin");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::env::set_var("CLANK_MCP_ETC", &etc);
        std::env::set_var("CLANK_MCP_BIN", &bin);
        f(base.to_str().unwrap());
        std::env::remove_var("CLANK_MCP_ETC");
        std::env::remove_var("CLANK_MCP_BIN");
    }

    #[test]
    fn valid_name_rules() {
        assert!(is_valid_name("github"));
        assert!(is_valid_name("my-server-2"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("-lead"));
        assert!(!is_valid_name("trail-"));
        assert!(!is_valid_name("Upper"));
        assert!(!is_valid_name("has_underscore"));
    }

    #[test]
    fn save_load_list_round_trip() {
        with_temp_dirs(|_base| {
            let cfg = McpServerConfig::new("https://x.example/mcp");
            save("demo", &cfg).unwrap();
            assert_eq!(load("demo").unwrap().unwrap().url, "https://x.example/mcp");
            assert!(load("demo").unwrap().unwrap().enabled);
            assert_eq!(list_names(), vec!["demo".to_string()]);
            remove("demo").unwrap();
            assert!(load("demo").unwrap().is_none());
            assert!(list_names().is_empty());
        });
    }

    #[test]
    fn auth_defaults_to_bearer_authorization() {
        with_temp_dirs(|_base| {
            std::env::set_var("CLANK_TEST_MCP_TOKEN", "tok123");
            let mut cfg = McpServerConfig::new("https://x/mcp");
            cfg.auth_env = Some("CLANK_TEST_MCP_TOKEN".into());
            let auth = cfg.resolve_auth().unwrap();
            assert_eq!(auth.header, "Authorization");
            assert_eq!(auth.value, "Bearer tok123");
            std::env::remove_var("CLANK_TEST_MCP_TOKEN");
        });
    }

    #[test]
    fn custom_header_sends_raw_value() {
        with_temp_dirs(|_base| {
            std::env::set_var("CLANK_TEST_MCP_KEY", "rawkey");
            let mut cfg = McpServerConfig::new("https://x/mcp");
            cfg.auth_env = Some("CLANK_TEST_MCP_KEY".into());
            cfg.auth_header = Some("X-Api-Key".into());
            let auth = cfg.resolve_auth().unwrap();
            assert_eq!(auth.header, "X-Api-Key");
            assert_eq!(auth.value, "rawkey");
            std::env::remove_var("CLANK_TEST_MCP_KEY");
        });
    }

    #[test]
    fn write_bin_stub_creates_readable_file() {
        with_temp_dirs(|_base| {
            write_bin_stub("demo", "usage: demo <tool>\n").unwrap();
            let content = std::fs::read_to_string(bin_dir().join("demo")).unwrap();
            assert!(content.contains("managed by `mcp`"));
            assert!(content.contains("usage: demo"));
        });
    }
}
