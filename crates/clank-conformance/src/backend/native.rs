//! The native target: an in-process [`Session`], one per scenario.
//!
//! A native Session operates on the REAL host filesystem and leans on process-global
//! state (the cwd bridge, `run_uu`'s fd swap, `export --secret`'s env writes), so:
//! - trials run at `--test-threads=1` (forced by `tests/native.rs`) — the fd swap is
//!   unsafe against ANY concurrently-printing thread, including the test reporter;
//! - `NATIVE_LOCK` serializes backends as defense-in-depth;
//! - each scenario gets a unique sandbox under the OS temp dir, removed in `finish()`;
//! - the process env is snapshotted at construction and restored in `finish()`.

use super::{Outcome, PendingView, ShellBackend};
use anyhow::Context;
use clank_core::session::Session;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

static NATIVE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// A [`ShellBackend`] backed by an in-process [`Session`] over the real host filesystem.
pub struct NativeBackend {
    // Field order is drop order: the Session (and its runtime) go before the guard.
    session: Session,
    rt: tokio::runtime::Runtime,
    tmp: PathBuf,
    tmp_str: String,
    env_snapshot: HashMap<OsString, OsString>,
    _guard: MutexGuard<'static, ()>,
}

impl NativeBackend {
    /// Construct a native backend: a fresh in-process [`Session`] and sandbox for one scenario.
    ///
    /// # Errors
    /// Returns `Err` if the sandbox directory cannot be pre-cleaned or created, if its path is
    /// not UTF-8, if the tokio runtime fails to build, or if `Session::new` fails.
    pub fn new(scenario: &str) -> anyhow::Result<Self> {
        let guard = NATIVE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let env_snapshot: HashMap<OsString, OsString> = std::env::vars_os().collect();

        let tmp = std::env::temp_dir().join(format!("clank-conf-{scenario}-{}", std::process::id()));
        // A PID can recur and a failing prior run may have leaked its sandbox — start
        // from a clean slate so stale files can't change this scenario's outcome.
        match std::fs::remove_dir_all(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("pre-cleaning sandbox {}", tmp.display())),
        }
        std::fs::create_dir_all(&tmp).with_context(|| format!("creating sandbox {}", tmp.display()))?;
        let tmp_str = tmp
            .to_str()
            .with_context(|| format!("sandbox path is not UTF-8: {}", tmp.display()))?
            .to_string();

        // The same current-thread runtime pattern the 456 in-crate tests use (`on_rt`).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building tokio runtime")?;
        let session = rt
            .block_on(Session::new())
            .map_err(|e| anyhow::anyhow!("Session::new failed: {e}"))?;

        Ok(Self { session, rt, tmp, tmp_str, env_snapshot, _guard: guard })
    }

    fn to_outcome(result: clank_core::session::LineResult) -> Outcome {
        Outcome {
            stdout: String::from_utf8_lossy(&result.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&result.stderr).into_owned(),
            exit_code: result.exit_code,
            pending: result.pending_prompt.map(|p| PendingView { question: p.question, choices: p.choices }),
        }
    }
}

impl ShellBackend for NativeBackend {
    fn eval(&mut self, line: &str) -> anyhow::Result<Outcome> {
        Ok(Self::to_outcome(self.rt.block_on(self.session.eval_line(line))))
    }

    fn answer(&mut self, response: Option<&str>) -> anyhow::Result<Outcome> {
        let response = response.map(str::to_string);
        Ok(Self::to_outcome(self.rt.block_on(self.session.answer_prompt(response))))
    }

    fn tmp(&self) -> &str {
        &self.tmp_str
    }

    fn finish(self: Box<Self>) -> anyhow::Result<()> {
        let Self { session, rt, tmp, tmp_str: _, env_snapshot, _guard } = *self;
        drop(session);
        drop(rt);

        // Restore the process env FIRST — it cannot fail, and it must not be skipped by
        // a filesystem teardown error (`export --secret` writes through to the real
        // process env; a leak here poisons every later trial). Safe: the lock is still
        // held and the suite runs single-threaded.
        let current: Vec<OsString> = std::env::vars_os().map(|(k, _)| k).collect();
        for key in current {
            if !env_snapshot.contains_key(&key) {
                std::env::remove_var(&key);
            }
        }
        for (key, value) in &env_snapshot {
            if std::env::var_os(key).as_ref() != Some(value) {
                std::env::set_var(key, value);
            }
        }

        match std::fs::remove_dir_all(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing sandbox {}", tmp.display())),
        }
        Ok(())
    }
}
