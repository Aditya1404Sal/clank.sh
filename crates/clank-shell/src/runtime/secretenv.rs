//! The live secret-environment slot: the set of `export --secret`-marked variables for the line
//! currently executing, so every synchronous render path can honor the README's redaction contract
//! without reaching the live [`Session`](crate::session::Session).
//!
//! `export --secret KEY=value` marks `KEY` sensitive. The value must never appear in `env`, the
//! logs, `ps`, or the transcript — but those surfaces are rendered from free functions and Brush
//! builtins that have no `Session` handle (`env` reads [`crate::runtime::procfs::current_environ`];
//! `ps` renders [`crate::runtime::proctable`] rows; the transcript is recorded in
//! [`crate::session`]). So the `Session` installs the current secret set here for the duration of one
//! `run_line`, and each render path consults [`active`] to filter or mask.
//!
//! The slot carries both the variable **names** (to drop secret keys entirely from `env` /
//! `/proc/environ`) and their **values** (to mask any occurrence of the literal value in `ps`
//! COMMAND, log text, or transcript output — a secret can leak by *value* even where its name never
//! appears, e.g. `echo "$API_KEY"` or a URL that embeds it).
//!
//! ## Why process-global, not thread-local
//!
//! The `env` builtin filters through [`filter_environ`], and natively Brush runs a non-final
//! pipeline stage (`env | grep …`) on a `tokio::spawn_blocking` worker thread — a DIFFERENT thread
//! from the one that installed the set. A thread-local set is invisible there, so the secret leaked
//! through `env | grep` on native (wasm runs stages inline and was unaffected). The set therefore
//! lives in a process-global slot, read from whatever thread the render path runs on.
//!
//! This matches how the secret *value* is already stored: `export --secret` writes it to the
//! process-global environment via `std::env::set_var`, and `filter_environ` reads the process-global
//! `std::env::vars()`. Secrets are a process-wide concept; only the filter set had (wrongly) been
//! thread-scoped. In production there is exactly one Session running one line at a time, so the
//! global is unambiguous; parallel-Session native tests serialize via a lock (see the test module).

use std::sync::{Arc, RwLock};

/// The placeholder every redaction path substitutes for a secret value. Matches the
/// [`crate::logging`] convention so redacted secrets read consistently across channels.
pub const REDACTED: &str = "<redacted>";

/// The set of secret variables active for the current line: `(name, value)` pairs. Held behind an
/// `Arc` so installing it is a cheap ref-count bump (the `Session` owns the canonical map).
pub type SecretSet = Arc<Vec<(String, String)>>;

/// The secret env-vars for the line currently executing. Populated by [`install`] for the duration
/// of one `run_line` and read by the env / ps / transcript render paths via [`active`] — including
/// from Brush's native pipeline worker threads, which a thread-local could not reach.
static ACTIVE: RwLock<Option<SecretSet>> = RwLock::new(None);

/// Install `secrets` as the active secret set for the current line; the guard restores the previous
/// slot on drop.
///
/// An **empty** set is a no-op that never touches the global: secret-free lines install an empty set
/// on every `run_line`, and letting those writes through would race a concurrent secret test's set to
/// `None`. Empty means "nothing to filter" anyway, so skipping is also semantically correct.
#[must_use]
pub fn install(secrets: SecretSet) -> InstallGuard {
    if secrets.is_empty() {
        return InstallGuard { previous: None };
    }
    let mut slot = ACTIVE.write().unwrap_or_else(|e| e.into_inner());
    let previous = slot.replace(secrets);
    InstallGuard { previous: Some(previous) }
}

/// The active secret set, if a line is executing.
pub fn active() -> Option<SecretSet> {
    ACTIVE.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Whether `name` is a secret-marked variable in the active set.
pub fn is_secret_name(name: &str) -> bool {
    active().is_some_and(|s| s.iter().any(|(k, _)| k == name))
}

/// Drop every secret-marked key from an `(name, value)` environment list. Used by `env` and
/// `/proc/<pid>/environ` so a secret var never appears in the environment display. When no secret set
/// is installed (native off-session reads, tests), the environment passes through unchanged.
pub fn filter_environ(environ: Vec<(String, String)>) -> Vec<(String, String)> {
    match active() {
        Some(secrets) => environ
            .into_iter()
            .filter(|(k, _)| !secrets.iter().any(|(sk, _)| sk == k))
            .collect(),
        None => environ,
    }
}

/// Mask every occurrence of an active secret **value** in `text`, replacing it with [`REDACTED`].
/// Used by the transcript recorder, `ps` COMMAND rendering, and any log text — a secret can leak by
/// value where its name is absent (e.g. `echo "$KEY"`, a URL that embeds it). Empty secret values are
/// skipped (masking `""` would corrupt the whole string). No-op when no secret set is installed.
pub fn mask_values(text: &str) -> String {
    match active() {
        Some(secrets) => {
            let mut out = text.to_string();
            for (_, value) in secrets.iter() {
                if !value.is_empty() {
                    out = out.replace(value.as_str(), REDACTED);
                }
            }
            out
        }
        None => text.to_string(),
    }
}

/// Test-only serialization lock. `ACTIVE` is process-global, so ANY test that installs a non-empty
/// secret set — the unit tests here AND the `export --secret` tests in [`crate::session`] — must hold
/// this for its duration, or a concurrent installer's set races into the slot mid-read. (Empty
/// installs are no-ops that never touch the slot, so secret-free tests need not hold it.)
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Restores the previous active secret set when dropped. `previous` is `None` for the no-op guard an
/// empty [`install`] returns (it wrote nothing, so it restores nothing); `Some(prev)` for a real
/// install that must put `prev` back.
pub struct InstallGuard {
    previous: Option<Option<SecretSet>>,
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            *ACTIVE.write().unwrap_or_else(|e| e.into_inner()) = previous;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    fn secret_test_lock() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn secrets(pairs: &[(&str, &str)]) -> SecretSet {
        Arc::new(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn install_and_active_round_trip_with_restore() {
        let _lock = secret_test_lock();
        assert!(active().is_none());
        {
            let _g = install(secrets(&[("K", "v")]));
            assert!(is_secret_name("K"));
            {
                let _g2 = install(secrets(&[("OTHER", "w")]));
                assert!(is_secret_name("OTHER"));
                assert!(!is_secret_name("K"));
            }
            // Inner guard dropped → outer restored.
            assert!(is_secret_name("K"));
        }
        assert!(active().is_none());
        assert!(!is_secret_name("K"));
    }

    #[test]
    fn empty_install_is_a_no_op_that_preserves_the_active_set() {
        let _lock = secret_test_lock();
        let _g = install(secrets(&[("K", "v")]));
        {
            // A secret-free line installs an empty set — it must NOT clear the outer secret.
            let _empty = install(Arc::new(Vec::new()));
            assert!(is_secret_name("K"), "empty install clobbered the active set");
        }
        assert!(is_secret_name("K"));
    }

    #[test]
    fn filter_environ_drops_secret_keys_only() {
        let _lock = secret_test_lock();
        let _g = install(secrets(&[("API_KEY", "sk-abc")]));
        let env = vec![
            ("API_KEY".to_string(), "sk-abc".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
        ];
        let filtered = filter_environ(env);
        assert_eq!(filtered, vec![("PATH".to_string(), "/usr/bin".to_string())]);
    }

    #[test]
    fn filter_environ_passthrough_without_install() {
        let _lock = secret_test_lock();
        let env = vec![("API_KEY".to_string(), "sk-abc".to_string())];
        assert_eq!(filter_environ(env.clone()), env);
    }

    #[test]
    fn mask_values_replaces_literal_secret() {
        let _lock = secret_test_lock();
        let _g = install(secrets(&[("API_KEY", "sk-abc123")]));
        assert_eq!(
            mask_values("curl -H 'x: sk-abc123' https://h"),
            "curl -H 'x: <redacted>' https://h"
        );
    }

    #[test]
    fn mask_values_skips_empty_secret() {
        let _lock = secret_test_lock();
        let _g = install(secrets(&[("EMPTY", "")]));
        assert_eq!(mask_values("hello world"), "hello world");
    }

    #[test]
    fn mask_values_passthrough_without_install() {
        let _lock = secret_test_lock();
        assert_eq!(mask_values("sk-abc"), "sk-abc");
    }
}
