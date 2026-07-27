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
