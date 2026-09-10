//! The structured failure type for grease: package parsing, integrity verification, the registry,
//! and the on-disk store.
//!
//! The variant set is deliberately small and fixed — one per *kind of thing that went wrong*, not one
//! per message. A package that fails to install can fail for reasons a caller genuinely treats
//! differently: a hash mismatch is a supply-chain rejection, a missing registry is a configuration
//! problem, and a malformed `--arg` is the user's typo. Those want different exit codes and, for the
//! integrity family, an audit record; before this they were one `String`.

/// A grease operation failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The command line was wrong: unknown subcommand, missing operand, bad flag.
    #[error("{0}")]
    Usage(String),

    /// A required argument was not supplied when running a prompt/script package.
    #[error("{0}")]
    MissingArgument(String),

    /// The package payload could not be parsed (bad JSON, bad `.md` frontmatter, unknown kind).
    #[error("{0}")]
    Malformed(String),

    /// The payload's sha256 did not match what the registry index advertised, or the index carried
    /// no hash at all for an indexed package. A supply-chain rejection.
    #[error("{0}")]
    Integrity(String),

    /// An ed25519 signature or public key failed to verify. A supply-chain rejection.
    #[error("{0}")]
    Signature(String),

    /// A transparency-log inclusion proof failed to verify. A supply-chain rejection.
    #[error("{0}")]
    LogProof(String),

    /// The package, registry, or installed name does not exist.
    #[error("{0}")]
    NotFound(String),

    /// Reading or writing the store, the markers, or a materialized file failed.
    #[error("{0}")]
    Io(String),

    /// The registry could not be reached, or answered with a non-200.
    #[error("{0}")]
    Transport(String),
}

impl Error {
    /// The clank exit code this failure should surface.
    ///
    /// Keeps the existing contract: `2` for a usage error the caller can correct, `4` for anything
    /// that failed out in the world (transport) or failed verification, `1` otherwise. The integrity
    /// family maps to 4 because that is what `grease install` has always returned for a rejected
    /// package, and the conformance corpus pins it.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Usage(_) | Error::MissingArgument(_) => 2,
            Error::Malformed(_)
            | Error::Integrity(_)
            | Error::Signature(_)
            | Error::LogProof(_)
            | Error::Transport(_) => 4,
            Error::NotFound(_) | Error::Io(_) => 1,
        }
    }

    /// Whether this failure is a supply-chain verification rejection.
    ///
    /// These are the ones worth auditing to `ops.log` regardless of how the caller reports them: a
    /// package was refused because it could not be trusted, which is a security event, not a typo.
    #[must_use]
    pub fn is_integrity_failure(&self) -> bool {
        matches!(
            self,
            Error::Integrity(_) | Error::Signature(_) | Error::LogProof(_)
        )
    }
}

/// A grease operation's result.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn exit_codes_match_the_established_contract() {
        assert_eq!(Error::Usage("bad flag".into()).exit_code(), 2);
        assert_eq!(Error::MissingArgument("--file".into()).exit_code(), 2);
        // Verification and transport failures are the exit-4 tier `grease install` already used.
        assert_eq!(Error::Integrity("sha mismatch".into()).exit_code(), 4);
        assert_eq!(Error::Signature("bad sig".into()).exit_code(), 4);
        assert_eq!(Error::LogProof("bad proof".into()).exit_code(), 4);
        assert_eq!(Error::Malformed("bad json".into()).exit_code(), 4);
        assert_eq!(Error::Transport("502".into()).exit_code(), 4);
        assert_eq!(Error::NotFound("nope".into()).exit_code(), 1);
        assert_eq!(Error::Io("disk full".into()).exit_code(), 1);
    }

    #[test]
    fn the_integrity_family_is_identifiable() {
        assert!(Error::Integrity("x".into()).is_integrity_failure());
        assert!(Error::Signature("x".into()).is_integrity_failure());
        assert!(Error::LogProof("x".into()).is_integrity_failure());
        // A typo or a missing file is not a supply-chain event.
        assert!(!Error::Usage("x".into()).is_integrity_failure());
        assert!(!Error::Io("x".into()).is_integrity_failure());
        assert!(!Error::Transport("x".into()).is_integrity_failure());
    }

    #[test]
    fn display_passes_the_message_through() {
        // Every variant renders its message verbatim: these strings are already complete, tested
        // sentences at their call sites, and the conformance corpus pins several of them.
        assert_eq!(
            Error::Integrity("grease install: integrity check failed".into()).to_string(),
            "grease install: integrity check failed"
        );
    }
}
