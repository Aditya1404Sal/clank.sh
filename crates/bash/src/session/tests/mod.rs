// The session test suite (included via `#[cfg(all(test, not(wasm)))] mod tests` in mod.rs).
// unwrap/expect on known-good fixtures is correct test style; clippy's allow-unwrap-in-tests does not
// recognize the compound cfg gate, so scope it explicitly here — it covers the submodules too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Shared test harness, plus one submodule per concern.
//!
//! Everything in THIS file is fixture: the seeded temp file the `rm` authorization tests operate
//! on. The fixtures every suite needs — the runtime helper, the cwd lock, the log capture and the
//! one-shot localhost server — live in [`crate::test_support`] instead, because the plug-in crate's
//! family suites need the same ones and a `static` lock only serializes within the process that
//! declares it. The submodules hold the assertions, and reach the fixtures through `use super::*`.
//!
//! The split mirrors `session/*.rs` where a concern has its own module there, and adds a file where
//! a concern is spread across several (`secrets`, `authz`, `resolution`, `logging`).
//!
//! ## Why submodules of one `mod tests`, and not files under `tests/` at the crate root
//!
//! This matters more than it looks, and an earlier attempt at this split was abandoned over test
//! races that turned out to be a consequence of getting it wrong.
//!
//! Crate-root integration tests compile to **separate binaries**, so each runs in its own process.
//! Several things this suite depends on are **process-global**, not per-test:
//!
//! - the process working directory, which `tools::coreutils::ShellCwd` moves for the duration of a
//!   builtin call (brush keeps `cd` in its own state and never touches the process cwd);
//! - `$CLANK_GREASE_*`, `$CLANK_MCP_*` and `$CLANK_LOG_DIR`, which the hermetic-dir guards set and
//!   restore;
//! - the `export --secret` table the synchronous render paths read;
//! - uucore's exit code, an `AtomicI32` upstream only ever resets at process exit;
//! - the `SIGPIPE` disposition that `run_uu` flips around a `uumain` call.
//!
//! The locks that serialize all of that — [`CWD_TEST_LOCK`], `runtime::secretenv::TEST_LOCK`,
//! `grease::config::TEST_ENV_LOCK`, `mcp::config::TEST_ENV_LOCK`, `logging::test_env_lock` — are
//! `static`s, so they serialize **within one process and not across processes**. Splitting into
//! separate binaries silently removes every one of those guarantees while leaving the code that
//! assumes them untouched, which is exactly the shape that produces intermittent, load-dependent
//! failures.
//!
//! Submodules keep one binary, one process, one set of locks. The tests are unchanged and so are
//! their guarantees — verified by the test-name list being identical across the split and by 12
//! consecutive clean runs.
//!
//! Note the lock ORDER convention, which the split preserves: **grease, then mcp**. Taking them in
//! the other order deadlocks against a test that takes them in this one.

use super::*;
use crate::test_support::{http_mock, on_rt, LogCapture, CWD_TEST_LOCK};

mod authz;
mod ctx;
mod eval;
mod http;
mod logging;
mod plugin;
mod prompt;
mod resolution;
mod secrets;

/// A seeded temp file for `rm` tests: returns its path. Uses a unique name per test.
fn seed_file(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("clank_authz_{tag}_{}", std::process::id()));
    std::fs::write(&path, b"x").unwrap();
    path
}
