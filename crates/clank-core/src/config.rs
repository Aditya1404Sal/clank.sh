//! Cross-cutting tunables: the numbers that bound clank's behaviour in one reviewable place.
//!
//! Scope rule: a constant belongs here when it is a **policy knob** — a bound, budget or deadline
//! someone might reasonably want to reason about or change together with its siblings. A constant
//! stays with its implementation when it is part of that item's **identity** (a tool's `NAME` and
//! `SYNOPSIS`, a protocol's version string, a filesystem path a single module owns).
//!
//! Bounds matter more here than in an ordinary program: clank runs as a durable Golem agent that
//! never restarts, and Golem serializes invocations per instance — so an unbounded wait doesn't just
//! stall one command, it wedges everything queued behind it.

use std::time::Duration;

/// Outbound HTTP deadlines.
///
/// These apply to the transports that do **not** go through `whttp` (which carries its own matching
/// defaults): the LLM providers, the MCP clients, and the Golem REST client. Every one of them
/// previously built a `reqwest`/`wstd` client with no timeout at all, so a black-holing endpoint
/// parked the invocation forever.
pub mod net {
    use super::Duration;

    /// Bound on establishing a connection.
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

    /// Bound on an ordinary request/response exchange (MCP, registry, Golem REST).
    pub const REQUEST_TIMEOUT: Duration = Duration::from_mins(2);

    /// Bound on a single model call. Longer than [`REQUEST_TIMEOUT`] because a large completion
    /// legitimately takes minutes; still finite, because "wait forever" is never the right answer on
    /// an agent that cannot be restarted.
    pub const LLM_TIMEOUT: Duration = Duration::from_mins(5);
}

/// Reject a URL that would carry package payloads or tool traffic over cleartext.
///
/// The README promises "MCP server tools (HTTPS only)", but `grease registry add` and `mcp add`
/// accepted any string — so `grease registry add http://attacker` was one confirmed `grease install`
/// away from an attacker-authored script on `$PATH`, with the transport offering no integrity at all
/// underneath the signature checks.
///
/// **Loopback is allowed.** `http://localhost:<port>` is the documented workflow for the bundled
/// registry authoring tool (`grease-populate` serves on `127.0.0.1`), and traffic that never leaves
/// the machine has no meaningful interception surface.
///
/// # Errors
/// Returns the REASON as text, not a typed error: this is a shared validator, and its two callers
/// (`grease registry add`, `mcp add`) each embed the reason in their own subsystem error. A type
/// here would have to belong to neither subsystem, which is worse than a string that is only ever
/// interpolated.
pub fn require_secure_url(url: &str) -> Result<(), String> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(format!("'{url}' is not an absolute http(s) URL"));
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@') // skip any userinfo
        .next()
        .unwrap_or("");
    let bare = host.rsplit_once(':').map_or(host, |(h, _port)| h);
    let is_loopback = matches!(bare, "localhost" | "127.0.0.1" | "::1" | "[::1]");
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Ok(()),
        "http" if is_loopback => Ok(()),
        "http" => Err(format!(
            "'{url}' uses plain http; use https (loopback is exempt for local registries)"
        )),
        other => Err(format!("'{url}' uses unsupported scheme '{other}'")),
    }
}

/// Bounds on the work a single command can be made to do.
///
/// These exist because the shell is driven by a model, not only by a person. A malformed format
/// string or a pathologically nested expression is a routine LLM emission, and on the agent the
/// failure mode for "allocate 10 GB" or "recurse until the stack ends" is a trap, not an error
/// message — so each of these turns an unbounded operation into a refusal.
pub mod limits {
    /// Largest field width or precision an `awk` `printf` conversion will honour.
    ///
    /// `awk 'BEGIN{printf "%9999999999d", 1}'` parsed its width as a bare `usize` and then built a
    /// pad string that long. 64 KiB is far past any legitimate column layout.
    pub const MAX_PRINTF_WIDTH: usize = 64 * 1024;

    /// Maximum expression-nesting depth the `awk` parser accepts.
    ///
    /// The recursive-descent cycle `parse_expr → … → parse_primary → parse_expr` had no depth
    /// limit, so deeply parenthesised input overflowed the stack — an abort, which unlike a panic
    /// cannot even be caught.
    pub const MAX_AWK_PARSE_DEPTH: usize = 200;

    /// Largest terminal width honoured from `$COLUMNS` when laying out multi-column output.
    ///
    /// The value was parsed as an unbounded `usize`, so `COLUMNS=18446744073709551615` produced a
    /// column count near `usize::MAX` and an effectively infinite layout loop.
    pub const MAX_COLUMNS: usize = 1000;

    /// Cap on concurrently-open MCP sessions retained by a session.
    ///
    /// The table had no bound and nothing reaped it, on an instance that never restarts.
    pub const MAX_MCP_SESSIONS: usize = 64;
}
