//! The external Golem cluster configuration for the native+cluster path (README §161-163: "Golem
//! cluster config is external to the shell — a concern only for the native binary, living outside
//! the shell's filesystem").
//!
//! Read from the environment so it lives outside the shell's virtual FS. When no configuration is
//! present, [`load`] returns `None` and native keeps the honest "needs a cluster" error — Tier C is
//! inert unless deliberately configured. The `/v1/agents/invoke-agent` REST call needs the Golem
//! application + environment names in addition to the endpoint + token (the agent-native API is
//! addressed by app/env/agent-type, not a raw worker id).

/// A resolved Golem cluster configuration for the native REST path.
#[derive(Clone, PartialEq, Eq)]
pub struct ClusterConfig {
    /// Base URL of the Golem worker/agent service, e.g. `http://localhost:9881` (no trailing slash).
    pub url: String,
    /// Bearer token for the Golem API, if the cluster requires auth. `None` for a token-less local
    /// server.
    pub token: Option<String>,
    /// The Golem application name the agents belong to (`appName` in the invoke request).
    pub app_name: String,
    /// The Golem environment name (`envName` in the invoke request), e.g. `local`.
    pub env_name: String,
}

impl std::fmt::Debug for ClusterConfig {
    /// Redacts the bearer `token` so it can never reach a `{:?}` sink (a log line, `dbg!`, a panic
    /// message, or the `Debug` of any containing struct). Presence is preserved; the value is not.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterConfig")
            .field("url", &self.url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("app_name", &self.app_name)
            .field("env_name", &self.env_name)
            .finish()
    }
}

/// Environment variables that configure the native cluster path.
const ENV_URL: &str = "GOLEM_URL";
const ENV_TOKEN: &str = "GOLEM_TOKEN";
const ENV_APP: &str = "GOLEM_APP";
const ENV_ENV: &str = "GOLEM_ENV";

/// Load the cluster config from the environment. Returns `None` (native stays honest-error) unless at
/// least `GOLEM_URL` is set; the app/env default to `default`/`local` (the usual local-server names)
/// when unset, and the token is optional. A trailing slash on the URL is trimmed so path joins are
/// unambiguous.
#[must_use]
pub fn load() -> Option<ClusterConfig> {
    let url = std::env::var(ENV_URL).ok().filter(|u| !u.is_empty())?;
    let url = url.trim_end_matches('/').to_string();
    Some(ClusterConfig {
        url,
        token: std::env::var(ENV_TOKEN).ok().filter(|t| !t.is_empty()),
        app_name: std::env::var(ENV_APP).ok().filter(|a| !a.is_empty()).unwrap_or_else(|| "default".to_string()),
        env_name: std::env::var(ENV_ENV).ok().filter(|e| !e.is_empty()).unwrap_or_else(|| "local".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate process-global env, so they run as ONE sequential test to avoid racing the
    // shared environment with each other and with other modules' env-reading tests.
    #[test]
    fn load_from_env_and_defaults() {
        // Snapshot + clear the relevant vars so the test is hermetic regardless of the host env.
        let saved: Vec<(&str, Option<String>)> = [ENV_URL, ENV_TOKEN, ENV_APP, ENV_ENV]
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        for (k, _) in &saved {
            std::env::remove_var(k);
        }

        // No GOLEM_URL → None (native stays honest-error).
        assert_eq!(load(), None);

        // Only URL → app/env default, no token, trailing slash trimmed.
        std::env::set_var(ENV_URL, "http://localhost:9881/");
        let cfg = load().unwrap();
        assert_eq!(cfg.url, "http://localhost:9881");
        assert_eq!(cfg.token, None);
        assert_eq!(cfg.app_name, "default");
        assert_eq!(cfg.env_name, "local");

        // Full config.
        std::env::set_var(ENV_TOKEN, "tok-123");
        std::env::set_var(ENV_APP, "myapp");
        std::env::set_var(ENV_ENV, "prod");
        let cfg = load().unwrap();
        assert_eq!(cfg.token.as_deref(), Some("tok-123"));
        assert_eq!(cfg.app_name, "myapp");
        assert_eq!(cfg.env_name, "prod");

        // Restore the original environment.
        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }
}
