//! The replay-safe `/var/log` sink, duplicated from `clank-embed` on purpose.
//!
//! `clank-embed::log_sink::DurableLogSink` is this exact sink, byte-for-byte in behaviour. This
//! crate does not depend on `clank-embed` and must not start now — `bash-tool` builds the shipped
//! `clank:bash` tool component, and the whole point of that component is that it links only the
//! shell core (`bash`), with none of clank's ai/mcp/grease/golem plug-in code pulled in behind it.
//! One small, self-contained sink duplicated here is a far better trade than a `pub use` that would
//! quietly drag `clank-core` (and everything under it) back into this build.
//!
//! `bash::logging`'s default sink ([`bash::logging::DefaultLogSink`]) **appends** directly to the
//! log file, which is correct on native (no replay) but not here: this tool runs as a Golem agent's
//! tool call, and a host call inside `run` that suspends and replays re-runs every line already
//! evaluated in this invocation, appending each one again. This sink avoids that by rewriting the
//! whole file from an in-memory buffer on every emit — idempotent, so a replay converges to the
//! same content instead of duplicating it. See `docs/architecture/replay-safety.md` and
//! [[golem-fs-append-replay-unsafe]] for the general hazard this works around.

use std::cell::RefCell;
use std::collections::HashMap;

use bash::config::limits::MAX_LOG_BYTES;
use bash::logging::{LogFile, LogSink, bound_tail, log_dir};

/// A replay-safe log sink: buffers each log file's recent lines in memory (bounded, rolling) and
/// rewrites the whole file via idempotent `std::fs::write` on every append.
///
/// Internal to this crate — nothing outside `bash-tool` needs to name it, unlike `clank-embed`'s
/// copy, which is re-exported for embedders that hand-pick their own provider mix.
pub(crate) struct DurableLogSink {
    /// Per-file accumulated contents (filename → bounded recent text). `RefCell` because
    /// `LogSink::append` takes `&self`; this tool runs single-threaded (wasip2), so there is no
    /// cross-thread contention to guard against.
    buffers: RefCell<HashMap<&'static str, String>>,
}

impl DurableLogSink {
    /// A sink with empty buffers — the state a fresh invocation starts from. There is no replay
    /// concern across invocations to worry about (this tool keeps no session state of its own, see
    /// the crate doc), only within one: a suspend-and-replay mid-`run` must reproduce the identical
    /// buffer content, which starting empty and re-running the same appends guarantees.
    pub(crate) fn new() -> Self {
        Self {
            buffers: RefCell::new(HashMap::new()),
        }
    }
}

impl LogSink for DurableLogSink {
    fn append(&self, file: LogFile, line: &str) {
        let filename = file.filename();
        let mut buffers = self.buffers.borrow_mut();
        // Pure in-memory accumulation — never seeded from disk, so a replay that re-runs this same
        // append doesn't re-add a line that a previous, unreplayed attempt already wrote to disk.
        let buf = buffers.entry(filename).or_default();
        buf.push_str(line);
        if !line.ends_with('\n') {
            buf.push('\n');
        }
        // Bound the buffer to a rolling tail (whole leading lines dropped). Deterministic, so a
        // replay reproduces the identical tail and the whole-file write below stays idempotent.
        bound_tail(buf, MAX_LOG_BYTES);
        // Idempotent whole-file write — safe to re-execute on replay (converges to identical content).
        let dir = log_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(filename), buf.as_bytes());
    }
}
