//! The replay-safe `/var/log` sink for the Golem agent.
//!
//! `clank-shell` defines the [`LogSink`](clank_shell::logging::LogSink) seam and a default sink that
//! **appends** directly to the log file. Appends are correct on native but NOT on the Golem agent: the
//! worker filesystem is ephemeral local disk rebuilt from the Initial File System, and Golem replays the
//! durable oplog by *re-running the guest code* — a raw `std::fs` append is a local side effect Golem
//! neither records nor skips, so a crash-then-recovery would re-run the append and duplicate the line.
//!
//! This sink is replay-safe by using **idempotent whole-file writes** instead of appends (the mitigation
//! the durability model recommends for raw side effects). The sink lives on the durable `Session` and
//! accumulates every log line for the agent's lifetime in an in-memory buffer; each emit rewrites the
//! whole file with `std::fs::write`.
//!
//! Why this is replay-safe: Golem reconstructs a recovered agent by re-running `new` + every past
//! `eval`/`run_line` from the start of the oplog. That re-runs every `append` call in the same order, so
//! the in-memory buffer is rebuilt to the identical content it had before the crash — and each idempotent
//! whole-file `std::fs::write` simply overwrites the file with that same content. No line is duplicated,
//! because nothing is appended to whatever bytes happen to already be on the ephemeral disk. (Crucially
//! the buffer is NEVER seeded from the on-disk file — doing so would re-add already-written lines on
//! replay. The buffer is pure in-memory state, deterministically reproduced by replay, exactly like the
//! transcript and process table.) This matches how grease persists its store: whole-file `std::fs::write`,
//! safe under replay. Only `std` + the portable `logging::log_dir` are used.
//!
//! (On the durability API: `golem_rust::durability::Durability::is_live()` IS public, but the only public
//! path to it is constructing a `Durability`, whose `new()` opens a durable-function region — so peeking
//! `is_live` to gate a raw append would leave a dangling region. The cheap raw execution-state accessor
//! (`current_durable_execution_state`) is `pub(crate)` in the SDK. Hence the idempotent whole-file rewrite
//! is the right mitigation here, not an is-live gate; see [[golem-fs-append-replay-unsafe]].)

use std::cell::RefCell;
use std::collections::HashMap;

use clank_shell::logging::{bound_tail, log_dir, LogFile, LogSink};

/// Per-log in-memory buffer cap. The whole-file-rewrite approach costs one full write per line, so an
/// unbounded buffer would grow without limit and make each write O(total-log-size). Keeping only a
/// bounded tail bounds both memory and per-write I/O; because the cap is applied deterministically, oplog
/// replay reproduces the same tail, so the write stays idempotent. `/var/log` is a rolling recent view,
/// not a permanent archive (the full history lives in the Golem oplog).
const MAX_LOG_BYTES: usize = 256 * 1024;

/// A replay-safe log sink: buffers each log file's recent lines in memory (bounded, rolling) and rewrites
/// the whole file on every append via idempotent `std::fs::write`.
pub struct DurableLogSink {
    /// Per-file accumulated contents (filename → bounded recent text). `RefCell` because `LogSink::append`
    /// takes `&self`; the agent is single-threaded (wasip2), so there is no cross-thread contention.
    buffers: RefCell<HashMap<&'static str, String>>,
}

impl DurableLogSink {
    pub fn new() -> Self {
        Self { buffers: RefCell::new(HashMap::new()) }
    }
}

impl LogSink for DurableLogSink {
    fn append(&self, file: LogFile, line: &str) {
        let filename = file.filename();
        let mut buffers = self.buffers.borrow_mut();
        // Pure in-memory accumulation — NEVER seeded from disk (see the module docs: seeding would
        // re-add already-written lines under replay). Replay rebuilds this buffer deterministically.
        let buf = buffers.entry(filename).or_default();
        buf.push_str(line);
        if !line.ends_with('\n') {
            buf.push('\n');
        }
        // Bound the buffer to a rolling tail (whole leading lines dropped). Deterministic, so replay
        // reproduces the identical tail (keeps the whole-file write idempotent).
        bound_tail(buf, MAX_LOG_BYTES);
        // Idempotent whole-file write — safe to re-execute on replay (converges to identical content).
        let dir = log_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(filename), buf.as_bytes());
    }
}
