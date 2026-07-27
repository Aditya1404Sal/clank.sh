//! The structured failure type for Golem interaction — remote invocation and cluster operations.
//!
//! Both seams previously returned `Result<String, String>`, which collapsed genuinely different
//! outcomes into one shape. The durable invoker's entire failure surface was
//! `Err(format!("agent invocation failed: {e:?}"))`, so "the cluster is unreachable", "the remote
//! agent trapped", and "the cluster rejected your credentials" arrived identically, at exit 1, as a
//! `Debug` rendering of an SDK type. A caller could not tell which had happened, and neither could
//! the model reading the tool result.
//!
//! The variants exist to answer the questions a caller actually asks:
//! - **Is it worth retrying?** ([`Error::is_retryable`]) — only [`Error::Unreachable`] is. That
//!   distinction is the precondition for any retry/backoff policy; clank has none today, and could
//!   not have written one against a `String`.
//! - **Whose fault is it?** — which decides the exit code, via [`Error::exit_code`]: a malformed
//!   request is a usage error (2), a remote or transport fault is not the caller's to fix (4).

/// A Golem invocation or cluster-operation failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The cluster could not be reached at all (DNS, connect, TLS, timeout). Retryable.
    #[error("cluster unreachable: {0}")]
    Unreachable(String),

    /// The request reached the cluster and it refused us (bad or missing token).
    #[error("cluster rejected the request: {0}")]
    Unauthorized(String),

    /// The remote agent ran and failed — a trap, a panic, or a returned error. NOT retryable: a
    /// deterministic failure replays identically, so retrying only re-runs the same trap.
    #[error("remote agent failed: {0}")]
    Remote(String),

    /// The invocation was malformed before it left: wrong arity, an argument that would not encode,
    /// an unparseable `--schedule`. The caller can fix this one.
    #[error("{0}")]
    Invalid(String),

    /// The operation is not available on this build or SDK surface (an honest stub, not a failure).
    #[error("{0}")]
    Unsupported(String),
}

impl Error {
    /// The clank exit code this failure should surface.
    ///
    /// `2` for anything the caller can correct, `4` for a transport/remote fault they cannot — the
    /// same split the rest of the shell uses.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Invalid(_) | Error::Unsupported(_) => 2,
            Error::Unreachable(_) | Error::Unauthorized(_) | Error::Remote(_) => 4,
        }
    }

    /// Whether retrying the same request could plausibly succeed.
    ///
    /// Only an unreachable cluster qualifies. A remote fault is deterministic under Golem's replay
    /// model — retrying re-runs the identical trap — and a rejected credential or a malformed
    /// request will not fix itself.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Unreachable(_))
    }
}

/// A Golem operation's result.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn only_an_unreachable_cluster_is_retryable() {
        assert!(Error::Unreachable("connect refused".into()).is_retryable());
        // A remote fault replays identically under Golem, so retrying re-runs the same trap.
        assert!(!Error::Remote("trap".into()).is_retryable());
        assert!(!Error::Unauthorized("bad token".into()).is_retryable());
        assert!(!Error::Invalid("wrong arity".into()).is_retryable());
        assert!(!Error::Unsupported("not on this build".into()).is_retryable());
    }

    #[test]
    fn exit_codes_split_caller_fault_from_remote_fault() {
        assert_eq!(Error::Invalid("bad".into()).exit_code(), 2);
        assert_eq!(Error::Unsupported("nope".into()).exit_code(), 2);
        assert_eq!(Error::Unreachable("down".into()).exit_code(), 4);
        assert_eq!(Error::Unauthorized("401".into()).exit_code(), 4);
        assert_eq!(Error::Remote("trap".into()).exit_code(), 4);
    }

    #[test]
    fn display_names_the_kind() {
        assert_eq!(
            Error::Unreachable("timed out".into()).to_string(),
            "cluster unreachable: timed out"
        );
        assert_eq!(
            Error::Remote("trap".into()).to_string(),
            "remote agent failed: trap"
        );
        // Invalid/Unsupported pass their message through: they already read as complete sentences at
        // the call sites that build them.
        assert_eq!(
            Error::Invalid("wrong arity".into()).to_string(),
            "wrong arity"
        );
    }
}
