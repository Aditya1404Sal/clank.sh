//! T1 PROBE FIXTURE — THROWAWAY. Delete with the probe.
//!
//! Two Golem tools, deliberately minimal, existing only so `dev-docs/research/agent-tools-probes.md`
//! can be written against a real `tool-rpc` call rather than a stand-in:
//!
//! - `probe` (from `Probe`) — one command with no streams at all, one that writes a stdout stream,
//!   and one that returns declared error cases. Enough to answer findings (a), (b), (c) and (e).
//! - `capable-probe` (from `CapableProbe`) — filesystem-capable (its manifest declaration carries a
//!   provisioned marker file, which implies the grant per gol-33). Two calls on one line answer
//!   finding (d): whether the owner filesystem lane serialises them.
//!
//! Command paths on the wire are the kebab-cased method names and EXCLUDE the tool name, so
//! `no_stream` is reached as `["no-stream"]` and the root would be `[]`.

// The tool macros expand to dispatch items that carry no doc comments; everything hand-written
// below is documented. Throwaway fixture, mirroring `fixtures/greeter-agent`'s allow.
// `too_many_arguments` fires on the macros' generated per-trait dispatcher (one argument per
// command parameter), not on anything hand-written here — same allow as fixtures/echo-tool.
#![allow(missing_docs, clippy::too_many_arguments)]

use golem_rust::agentic::OutputStream;
use golem_rust::{FromSchema, IntoSchema, ToolError, tool_definition, tool_implementation};

/// What a streaming command reports once its stream is finished.
#[derive(Debug, Clone, IntoSchema, FromSchema)]
pub struct Summary {
    pub chunks: u32,
    pub bytes: u64,
}

/// Two declared error cases with distinct exit codes, so the probe can confirm that a tool's own
/// exit code reaches the shell rather than being flattened to 1.
#[derive(Debug, Clone, ToolError)]
pub enum ProbeError {
    #[tool_error(kind = "usage-error", exit_code = 2)]
    Bad { reason: String },
    #[tool_error(kind = "runtime-error", exit_code = 7)]
    Boom,
}

#[tool_definition(version = "1.0.0")]
pub trait Probe {
    /// No streams at all: a structured result only. The cheapest possible round trip, and the one
    /// the latency numbers in finding (e) are measured against.
    async fn no_stream(&self, value: String) -> Result<String, ProbeError>;

    /// Stdout only. Writes `marker:` as its own chunk *before* the body, so the probe can tell
    /// whether the caller sees bytes while the call is still running (live) or only once the
    /// terminal arrives — finding (b).
    async fn produce(
        &self,
        count: u32,
        text: String,
        stdout: OutputStream,
    ) -> Result<Summary, ProbeError>;

    /// Declared error cases: `--usage` selects the usage-kind case (exit 2), otherwise the
    /// runtime-kind case (exit 7).
    async fn fail(&self, usage: bool) -> Result<String, ProbeError>;
}

#[tool_definition(version = "1.0.0")]
pub trait CapableProbe {
    /// Filesystem-capable. Writes `text` to `path` in the *caller's* filesystem, then records `tag`
    /// in `/probe-order.log` with a read-modify-whole-file-write (never an append: an append
    /// duplicates itself under oplog replay). If two calls on one line overlapped rather than
    /// serialising on the owner lane, one tag would clobber the other and only one would survive.
    async fn write_tag(
        &self,
        path: String,
        text: String,
        tag: String,
    ) -> Result<String, ProbeError>;
}

struct ProbeImpl;

#[tool_implementation]
impl Probe for ProbeImpl {
    // Nothing to await in these bodies; the trait declares them async.
    #[allow(clippy::unused_async_trait_impl)]
    async fn no_stream(&self, value: String) -> Result<String, ProbeError> {
        Ok(format!("no-stream:{value}"))
    }

    async fn produce(
        &self,
        count: u32,
        text: String,
        mut stdout: OutputStream,
    ) -> Result<Summary, ProbeError> {
        let mut chunks = 0;
        let mut bytes = 0u64;
        // The marker goes out first and alone: if the caller can read it before the terminal
        // arrives, stdout is genuinely live for this (filesystem-incapable) tool.
        let marker = b"marker:".to_vec();
        bytes += marker.len() as u64;
        chunks += 1;
        if stdout.write(marker).await.is_err() {
            return Err(ProbeError::Boom);
        }
        for _ in 0..count {
            let chunk = text.clone().into_bytes();
            bytes += chunk.len() as u64;
            chunks += 1;
            if stdout.write(chunk).await.is_err() {
                return Err(ProbeError::Boom);
            }
        }
        Ok(Summary { chunks, bytes })
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn fail(&self, usage: bool) -> Result<String, ProbeError> {
        if usage {
            Err(ProbeError::Bad {
                reason: "asked for a usage error".to_string(),
            })
        } else {
            Err(ProbeError::Boom)
        }
    }
}

struct CapableProbeImpl;

#[tool_implementation]
impl CapableProbe for CapableProbeImpl {
    #[allow(clippy::unused_async_trait_impl)]
    async fn write_tag(
        &self,
        path: String,
        text: String,
        tag: String,
    ) -> Result<String, ProbeError> {
        // std::fs, not the wasi crate: a filesystem-capable tool body gets the owner root as its
        // one preopened directory, so ordinary Rust file APIs land in the caller's filesystem.
        std::fs::write(&path, text.as_bytes()).map_err(|e| ProbeError::Bad {
            reason: format!("write {path}: {e}"),
        })?;
        let mut log = std::fs::read_to_string("/probe-order.log").unwrap_or_default();
        log.push_str(&tag);
        log.push('\n');
        std::fs::write("/probe-order.log", log.as_bytes()).map_err(|e| ProbeError::Bad {
            reason: format!("write /probe-order.log: {e}"),
        })?;
        let back = std::fs::read_to_string(&path).map_err(|e| ProbeError::Bad {
            reason: format!("read back {path}: {e}"),
        })?;
        Ok(back)
    }
}
