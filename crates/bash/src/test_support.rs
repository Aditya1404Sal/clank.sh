// Fixture code: `unwrap` on a known-good runtime build or a temp path is correct test style, but
// this module also compiles outside `cfg(test)` (behind the `test-support` feature), where clippy's
// allow-unwrap-in-tests does not apply — so scope the two lints explicitly here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Test fixtures shared by the shell core's own suite and by the suites of crates that drive a
//! [`Session`](crate::session::Session) from outside — the plug-in crate's family tests.
//!
//! Everything here exists because the things these tests touch are **process-global**, not
//! per-test: the async runtime a `Session` needs, the process working directory
//! (`tools::coreutils::ShellCwd` moves it for the duration of a builtin call), and `$CLANK_LOG_DIR`.
//! The `static` locks that serialize them only serialize within one process, so every suite that
//! needs them must take the same ones — which is why they live in the library behind a feature
//! rather than being copied per crate.

/// Drive a closure on a fresh current-thread runtime (mirrors how `Session` is used natively).
///
/// # Panics
/// Panics if the tokio runtime cannot be built — a broken test environment, not a condition a test
/// should carry a branch for.
pub fn on_rt<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

/// Serializes the cwd-sensitive `cd` test against the curl-pipeline tests: their grep stages hold
/// process-cwd windows (`ShellCwd`) while a mock server round-trips, and the process cwd is one
/// global across Sessions. Test-parallelism artifact only; production runs one line at a time.
pub static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Points `CLANK_LOG_DIR` at a fresh temp dir for a Session logging test, restoring the env on drop.
/// Serializes via a process-wide lock (env is global). The default `DefaultLogSink` (installed by
/// `eval_line`) then writes real files under this dir.
pub struct LogCapture {
    _lock: std::sync::MutexGuard<'static, ()>,
    dir: std::path::PathBuf,
}

impl LogCapture {
    /// Redirect `$CLANK_LOG_DIR` to a fresh temp dir named after `tag`, holding the logging env lock
    /// for the life of the returned guard.
    #[must_use]
    pub fn new(tag: &str) -> Self {
        let lock = crate::logging::test_env_lock();
        let dir = std::env::temp_dir().join(format!("clank-sesslog-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var(crate::config::env::LOG_DIR, &dir);
        Self { _lock: lock, dir }
    }

    /// The captured contents of one log file (empty when the command wrote nothing).
    #[must_use]
    pub fn read(&self, file: crate::logging::LogFile) -> String {
        std::fs::read_to_string(self.dir.join(file.filename())).unwrap_or_default()
    }
}

impl Drop for LogCapture {
    fn drop(&mut self) {
        std::env::remove_var(crate::config::env::LOG_DIR);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A one-shot localhost HTTP server (raw `std::net`, no dep) that replies `200 <body>` once.
/// Hermetic — the `curl`/`wget` interception is exercised end-to-end without real internet.
///
/// # Panics
/// Panics if no loopback port can be bound — a broken test environment.
#[must_use]
pub fn http_mock(body: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{addr}")
}
