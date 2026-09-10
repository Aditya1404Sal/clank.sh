//! The Golem-agent invocation seam: `clank-core` defines the [`AgentInvoker`] trait; the durable
//! Golem binding (`golem-rust`'s generic `WasmRpc`) is implemented in the `clank-agent` crate and
//! injected via `Session::set_agent_invoker`. Mirrors the `AskProvider`→`DurableAnthropicProvider` and
//! `McpHttp`→`WstdMcpHttp` seams.
//!
//! A grease-installed Golem agent (README:671, 795) becomes a `/usr/lib/agents/bin/<name>` command;
//! running `<agent> [--<ctor> val] [<wrapper-flags>] <method> [-- --<arg> val]` builds an
//! [`AgentInvocation`] and dispatches it through the injected invoker, which invokes the agent type by
//! name in the configured Golem cluster and returns the rendered result. Native / no-cluster returns
//! an honest error.
//!
//! Modes (README:842): **await** (default, returns the result), **trigger** (`--trigger`,
//! fire-and-forget), **schedule** (`--schedule <iso8601>`, deferred). Trigger/schedule return a handle
//! the caller retains for `kill`-cancel. `--phantom <uuid>` addresses a phantom instance;
//! `--revision <n>` is honest-stubbed (no wasm-rpc constructor slot — a `golem:api` concern).

/// The invocation mode selected by the wrapper flags (README:842).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvokeMode {
    /// Await the result (default).
    Await,
    /// Fire-and-forget (`--trigger`).
    Trigger,
    /// Schedule for a future ISO-8601 time (`--schedule <iso8601>`).
    Schedule(String),
}

/// One invocation of a Golem agent, parsed from an agent-executable command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentInvocation {
    /// The Golem agent **type** name to invoke (the reflected type, e.g. `ShoppingCart`), NOT the
    /// installed command name.
    pub agent_type: String,
    /// The constructor parameters (`--<name> value`) that identify the agent instance. Order-preserving
    /// (Golem's agent identity is the ordered constructor tuple).
    pub constructor: Vec<(String, String)>,
    /// The method (kebab-case subcommand) to invoke.
    pub method: String,
    /// The method arguments (`--<name> value`), order-preserving.
    pub args: Vec<(String, String)>,
    /// The invocation mode (await / trigger / schedule).
    pub mode: InvokeMode,
    /// The `--phantom <uuid>` instance selector, if given.
    pub phantom: Option<String>,
}

impl AgentInvocation {
    /// A bare await-mode invocation (no wrapper flags) — the common case + test helper.
    #[must_use]
    pub fn new(
        agent_type: String,
        constructor: Vec<(String, String)>,
        method: String,
        args: Vec<(String, String)>,
    ) -> Self {
        Self {
            agent_type,
            constructor,
            method,
            args,
            mode: InvokeMode::Await,
            phantom: None,
        }
    }
}

/// The outcome of a non-await invocation: an opaque cancellation handle the caller retains so a later
/// `kill <pid>` can cancel it (README:850). Empty when there's nothing to cancel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InvokeHandle {
    /// An opaque token the invoker understands (e.g. an idempotency key / scheduled-invocation id).
    /// `None` when the mode isn't cancelable or the invoker doesn't support cancel.
    pub cancel_token: Option<String>,
    /// A short human description for the `ps`/output line (e.g. "triggered", "scheduled for …").
    pub note: String,
}

/// Injected Golem-agent invoker. `?Send` (wasip2 is single-threaded), `dyn`-compatible as
/// `Box<dyn AgentInvoker>`.
#[async_trait::async_trait(?Send)]
pub trait AgentInvoker {
    /// Invoke `inv` in await mode and return the rendered result string.
    ///
    /// # Errors
    /// See [`crate::golem::Error`] — the variant distinguishes an unreachable cluster (retryable)
    /// from a remote fault, a rejected credential, and a malformed request.
    async fn invoke(&self, inv: &AgentInvocation) -> crate::golem::error::Result<String>;

    /// Invoke `inv` in trigger or schedule mode (fire-and-forget / deferred). Returns an
    /// [`InvokeHandle`] carrying a cancel token (for `kill`) + a note. Default: not supported.
    ///
    /// # Errors
    /// [`crate::golem::Error::Unsupported`] by default; see [`crate::golem::Error`] otherwise.
    async fn invoke_async(
        &self,
        _inv: &AgentInvocation,
    ) -> crate::golem::error::Result<InvokeHandle> {
        Err(crate::golem::Error::Unsupported(
            "this invoker does not support trigger/schedule mode".to_string(),
        ))
    }

    /// Cancel a previously-triggered/scheduled invocation by its token (README:850). Returns `Ok(true)`
    /// if the cancel took effect, `Ok(false)` if it was already in-progress/completed (a no-op).
    ///
    /// # Errors
    /// [`crate::golem::Error::Unsupported`] by default; see [`crate::golem::Error`] otherwise.
    async fn cancel(&self, _token: &str) -> crate::golem::error::Result<bool> {
        Err(crate::golem::Error::Unsupported(
            "this invoker does not support cancel".to_string(),
        ))
    }
}

/// Parse an ISO-8601 / RFC-3339 timestamp (e.g. `2026-06-01T09:00:00Z`) to Unix epoch seconds.
/// Dependency-free (the agent crate has no chrono), `YYYY-MM-DDThh:mm:ss[Z]`, UTC only.
///
/// Lives here rather than in the agent crate so it can actually be tested: `clank-agent` is a
/// wasm-only `cdylib` with no test target, and this is target-agnostic arithmetic.
///
/// **Every field is range-checked before the civil-calendar arithmetic runs.** The values come
/// straight off an LLM-supplied `--schedule` string, and `days * 86400` overflows `i64` for a large
/// year. `golem.yaml` builds the local environment with the `debug` component preset, where overflow
/// checks are ON — so an unvalidated year was a hard panic, and a panic on the durable agent traps
/// the instance.
///
/// # Errors
/// Returns a human-readable message if the string is malformed, a field is out of range, or the
/// instant is before the Unix epoch.
pub fn parse_epoch_secs(s: &str) -> crate::golem::error::Result<u64> {
    let err = || {
        crate::golem::Error::Invalid(format!(
            "invalid --schedule time '{s}' (expected YYYY-MM-DDThh:mm:ssZ)"
        ))
    };
    let trimmed = s.trim().trim_end_matches('Z');
    let (date, time) = trimmed.split_once('T').ok_or_else(err)?;
    let mut d = date.split('-');
    let year: i64 = d.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let month: i64 = d.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let day: i64 = d.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let mut t = time.split(':');
    let hh: i64 = t.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let mm: i64 = t.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let ss: i64 = t.next().unwrap_or("0").parse().map_err(|_| err())?;
    // The overflow guard. 9999 keeps `days * 86400` ~2.5e11, four orders of magnitude inside i64.
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hh)
        || !(0..=59).contains(&mm)
        || !(0..=60).contains(&ss)
    // 60 = leap second
    {
        return Err(err());
    }
    // Days from the civil calendar (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86400 + hh * 3600 + mm * 60 + ss;
    if secs < 0 {
        return Err(crate::golem::Error::Invalid(
            "scheduled time is before the epoch".to_string(),
        ));
    }
    // guarded by the `secs < 0` check immediately above
    #[allow(clippy::cast_sign_loss)]
    Ok(secs as u64)
}

#[cfg(test)]
mod tests {
    use super::parse_epoch_secs;

    #[test]
    fn parses_known_instants() {
        assert_eq!(parse_epoch_secs("1970-01-01T00:00:00Z"), Ok(0));
        assert_eq!(parse_epoch_secs("2026-06-01T09:00:00Z"), Ok(1_780_304_400));
        // The trailing `Z` and the seconds field are both optional.
        assert_eq!(parse_epoch_secs("2026-06-01T09:00"), Ok(1_780_304_400));
    }

    #[test]
    fn out_of_range_fields_are_rejected_not_overflowed() {
        // Regression: these reached `days * 86400` unchecked. Under the `debug` component preset
        // (overflow-checks on) that is a panic — a trap that wedges the durable agent.
        for bad in [
            "9999999999999999-01-01T00:00:00Z",
            "-9999999999999999-01-01T00:00:00Z",
            "2026-99-01T00:00:00Z",
            "2026-01-99T00:00:00Z",
            "2026-01-01T99:00:00Z",
            "2026-01-01T00:99:00Z",
            "2026-01-01T00:00:99Z",
        ] {
            assert!(
                parse_epoch_secs(bad).is_err(),
                "{bad} must be rejected, not overflowed"
            );
        }
    }

    #[test]
    fn pre_epoch_and_malformed_are_rejected() {
        assert!(parse_epoch_secs("1969-12-31T23:59:59Z").is_err());
        assert!(parse_epoch_secs("not-a-time").is_err());
        assert!(parse_epoch_secs("2026-06-01").is_err());
        assert!(parse_epoch_secs("").is_err());
    }
}
