//! The structured failure type for MCP: the JSON-RPC client, the transport seam, and `mcp` command
//! handling.
//!
//! This one started closer than the others — `McpError { message, exit_code }` already paired a
//! message with a code, which is why MCP has the most legible failures in the shell. What it could
//! not express is the *kind*: a caller could read the exit code, but nothing distinguished "the
//! server is unreachable" from "the server answered with nonsense" from "the tool itself reported a
//! failure". Those are three different situations for a driver — the first is worth retrying, the
//! second means the server is broken, and the third is a normal negative result the model should
//! read and act on.
//!
//! The constructors keep their original names (`transport`, `usage`, `tool`) so the client's call
//! sites read unchanged; `protocol` and `io` are new.

/// An MCP operation failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The server could not be reached, or answered with a non-success status. Retryable.
    #[error("{0}")]
    Transport(String),

    /// The server answered, but not with something that makes sense: malformed JSON-RPC, an
    /// unparseable SSE frame, a response carrying neither `result` nor `error`, or a paginated
    /// listing that exceeded its page budget.
    #[error("{0}")]
    Protocol(String),

    /// The request was wrong: a bad `mcp` command line, or a JSON-RPC `-32601`/`-32602` (unknown
    /// method / invalid params).
    #[error("{0}")]
    Usage(String),

    /// The tool ran and reported a failure (`isError: true`). Not a malfunction — a negative result.
    #[error("{0}")]
    Tool(String),

    /// Reading or writing MCP configuration on disk failed.
    #[error("{0}")]
    Io(String),
}

impl Error {
    /// A transport failure (unreachable server, unreadable body).
    ///
    /// Public because [`crate::mcp::client::McpHttp`] is implemented outside this crate — the wasm
    /// `wstd` transport lives in `clank-agent` — and this is the one kind a transport may construct.
    #[must_use]
    pub fn transport(msg: impl Into<String>) -> Self {
        Error::Transport(msg.into())
    }

    /// A protocol violation by the server.
    pub(crate) fn protocol(msg: impl Into<String>) -> Self {
        Error::Protocol(msg.into())
    }

    /// A malformed request or command line.
    pub(crate) fn usage(msg: impl Into<String>) -> Self {
        Error::Usage(msg.into())
    }

    /// The tool itself reported a failure.
    pub(crate) fn tool(msg: impl Into<String>) -> Self {
        Error::Tool(msg.into())
    }

    /// A configuration read/write failure.
    pub(crate) fn io(msg: impl Into<String>) -> Self {
        Error::Io(msg.into())
    }

    /// The clank exit code this failure should surface. Preserves the codes the previous
    /// `McpError { exit_code }` carried, so nothing downstream shifts.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Tool(_) | Error::Io(_) => 1,
            Error::Usage(_) => 2,
            Error::Transport(_) | Error::Protocol(_) => 4,
        }
    }

    /// Whether retrying could plausibly succeed.
    ///
    /// Only a transport failure. A protocol violation means the server is broken in a way another
    /// identical request will reproduce; a usage error and a tool-reported failure are both answers,
    /// not outages.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Transport(_))
    }

    /// Whether this is the tool's own negative result rather than a malfunction.
    ///
    /// Worth separating for the `ask` loop: a tool that reports failure is information the model
    /// should read and act on, not an infrastructure problem to surface as a broken tool.
    #[must_use]
    pub fn is_tool_failure(&self) -> bool {
        matches!(self, Error::Tool(_))
    }
}

/// An MCP operation's result.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn exit_codes_match_the_previous_struct_contract() {
        assert_eq!(Error::transport("down").exit_code(), 4);
        assert_eq!(Error::usage("bad params").exit_code(), 2);
        assert_eq!(Error::tool("search failed").exit_code(), 1);
        // New variants slot into the same scheme.
        assert_eq!(Error::protocol("bad JSON-RPC").exit_code(), 4);
        assert_eq!(Error::io("cannot write config").exit_code(), 1);
    }

    #[test]
    fn only_transport_is_retryable() {
        assert!(Error::transport("connect refused").is_retryable());
        // A server that speaks nonsense will speak the same nonsense next time.
        assert!(!Error::protocol("garbage").is_retryable());
        assert!(!Error::usage("bad").is_retryable());
        assert!(!Error::tool("no results").is_retryable());
    }

    #[test]
    fn a_tool_failure_is_distinguishable_from_a_malfunction() {
        assert!(Error::tool("no results").is_tool_failure());
        assert!(!Error::transport("down").is_tool_failure());
        assert!(!Error::protocol("garbage").is_tool_failure());
    }
}
