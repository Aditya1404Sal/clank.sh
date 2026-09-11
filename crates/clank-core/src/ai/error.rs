//! The structured failure type for the `ai` subsystem: model configuration, provider resolution, and
//! the model call itself.
//!
//! The distinction that matters most here is **rate limiting**. A 429 and a bad API key both used to
//! arrive as "ask: model call failed: …" at exit 4, so a driver could not tell "wait and try again"
//! from "your credentials are wrong, waiting will not help". `RateLimited` carries `retry_after`
//! when the provider supplies it, which is the one piece of information that makes a backoff policy
//! possible rather than guessed.

/// An `ai` operation failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// No provider is configured, or the configured one has no key.
    #[error("{0}")]
    NotConfigured(String),

    /// An unknown provider prefix or an unusable model id — the caller's to fix.
    #[error("{0}")]
    Model(String),

    /// Reading or writing `ask.toml` failed.
    #[error("{0}")]
    Config(String),

    /// The provider rejected our credentials.
    #[error("{0}")]
    Unauthorized(String),

    /// The provider rate-limited us. `retry_after` is the provider's advertised wait, in seconds,
    /// when it supplied one.
    #[error("{message}")]
    RateLimited {
        /// The provider's message.
        message: String,
        /// Seconds to wait before retrying, if the provider said.
        retry_after: Option<u64>,
    },

    /// The model call failed for any other reason (transport, a 5xx, a malformed response).
    #[error("{0}")]
    Request(String),
}

impl Error {
    /// The clank exit code this failure should surface.
    ///
    /// `2` for a model id the caller can correct; `4` for everything else, matching what `ask` has
    /// always returned for a failed model call.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Model(_) => 2,
            Error::Config(_) => 1,
            Error::NotConfigured(_)
            | Error::Unauthorized(_)
            | Error::RateLimited { .. }
            | Error::Request(_) => 4,
        }
    }

    /// Whether retrying could plausibly succeed.
    ///
    /// A rate limit and a generic request failure both can. A missing key, a wrong key, and a bad
    /// model id cannot — those need a human to change something.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::RateLimited { .. } | Error::Request(_))
    }

    /// How long the provider asked us to wait before retrying, if it said.
    #[must_use]
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            Error::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// The shared "no model provider is installed for this session" message — the ONE condition
    /// `ask`, `ask repl`, and `context summarize` all degrade to when no
    /// [`crate::ai::ask::AskProvider`] was ever injected (native with none set, or an embed that
    /// skips a `with_default_golem_providers`-style setup). Before this existed, each call site hand-
    /// wrote its own wording and drifted (`ask repl`'s lost the "(available on the Golem agent)"
    /// aside that the other two carried). `command` is the invoking command's display prefix
    /// (`"ask"`, `"ask repl"`, `"context summarize"`) — the one part that legitimately varies, so a
    /// single frozen `&str` constant can't serve every call site; a function can.
    ///
    /// Distinct from a provider being installed but unable to authenticate (missing API key) — that
    /// condition is also [`Error::NotConfigured`], but the remediation text differs per provider (which
    /// env var, which `model add` invocation) and is constructed at each provider's own call site
    /// instead.
    #[must_use]
    pub fn not_configured(command: &str) -> Self {
        Error::NotConfigured(format!(
            "{command}: no model provider configured (available on the Golem agent)\n"
        ))
    }
}

/// An `ai` operation's result.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn a_rate_limit_is_retryable_and_a_bad_key_is_not() {
        // The distinction the String form could not make: both used to be exit-4 strings.
        let limited = Error::RateLimited {
            message: "429 slow down".into(),
            retry_after: Some(30),
        };
        assert!(limited.is_retryable());
        assert_eq!(limited.retry_after(), Some(30));

        let bad_key = Error::Unauthorized("401 invalid api key".into());
        assert!(!bad_key.is_retryable(), "waiting will not fix a bad key");
        assert_eq!(bad_key.retry_after(), None);

        assert!(!Error::NotConfigured("no provider".into()).is_retryable());
        assert!(!Error::Model("unknown provider 'foo'".into()).is_retryable());
    }

    #[test]
    fn exit_codes_split_a_fixable_model_id_from_everything_else() {
        assert_eq!(Error::Model("bad id".into()).exit_code(), 2);
        assert_eq!(Error::Config("cannot write ask.toml".into()).exit_code(), 1);
        assert_eq!(Error::NotConfigured("no key".into()).exit_code(), 4);
        assert_eq!(Error::Request("500".into()).exit_code(), 4);
    }

    #[test]
    fn not_configured_varies_only_the_command_prefix() {
        // The one difference between call sites is the leading command name; the rest of the
        // wording (and the NotConfigured variant, hence exit code 4) is shared.
        let ask = Error::not_configured("ask");
        assert_eq!(
            ask.to_string(),
            "ask: no model provider configured (available on the Golem agent)\n"
        );
        assert_eq!(ask.exit_code(), 4);

        let repl = Error::not_configured("ask repl");
        assert_eq!(
            repl.to_string(),
            "ask repl: no model provider configured (available on the Golem agent)\n"
        );

        let summarize = Error::not_configured("context summarize");
        assert!(summarize.to_string().starts_with("context summarize: "));
        // Same suffix every time — only the prefix moves.
        assert!(ask
            .to_string()
            .ends_with("no model provider configured (available on the Golem agent)\n"));
        assert!(repl
            .to_string()
            .ends_with("no model provider configured (available on the Golem agent)\n"));
        assert!(summarize
            .to_string()
            .ends_with("no model provider configured (available on the Golem agent)\n"));
    }
}
