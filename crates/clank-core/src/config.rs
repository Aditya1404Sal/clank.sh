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
