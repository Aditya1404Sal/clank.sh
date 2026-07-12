//! The `/var/log/` observability layer (README:613-634). Human-readable, append-only process logs plus
//! structured, PID/PPID-addressable audit events. Four log files under [`log_dir`]:
//!
//! - `shell.log` — per-command start/end/exit-code events and authorization pauses.
//! - `http.log`  — outbound HTTP (MCP, grease registry, curl/wget, and the `ask` LLM call), secrets redacted.
//! - `mcp.log`   — MCP JSON-RPC tool invocations and their responses.
//! - `ops.log`   — destructive operations (the `sudo-only` tier).
//!
//! ## Replay safety — why appends go through a [`LogSink`] seam
//!
//! A file **append** is NOT replay-safe on a Golem agent. The worker filesystem is ephemeral local disk
//! rebuilt from the Initial File System on every start; Golem replays the durable oplog by **re-running
//! the guest code**, and a raw `std::fs` append is a local side effect that Golem neither records to the
//! oplog nor skips on replay — so a crash-then-recovery would re-run the append and **duplicate the
//! line**. (Whole-file `std::fs::write`, as grease uses for its store, is idempotent and therefore safe;
//! only append is the hazard.)
//!
//! So writes route through a [`LogSink`] installed per-line (the thread-local pattern used by
//! `proctable`/`sysprompt`). On native there is no replay, so the [default sink](DefaultLogSink) appends
//! directly. On the Golem agent, `clank-agent` injects a sink that appends only when the durable
//! execution state is **live** (via `golem:durability`'s `current-durable-execution-state().is-live`),
//! skipping the write during replay — exactly-once, no duplicates. If no sink is installed (off-session
//! reads), writes are dropped.
//!
//! The log directory is overridable via `CLANK_LOG_DIR` (mirrors the `CLANK_GREASE_*` seams) so tests
//! can assert on a temp dir.

use std::cell::RefCell;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

/// Default log directory (README filesystem layout).
pub const DEFAULT_LOG_DIR: &str = "/var/log";

/// The one env override, mirroring `greaseconfig`'s `CLANK_GREASE_*` seams — lets tests point the log
/// layer at a temp dir instead of the real `/var/log`.
pub const LOG_DIR_ENV: &str = "CLANK_LOG_DIR";

/// The log directory: `$CLANK_LOG_DIR` if set, else `/var/log`.
pub fn log_dir() -> PathBuf {
    PathBuf::from(std::env::var(LOG_DIR_ENV).unwrap_or_else(|_| DEFAULT_LOG_DIR.to_string()))
}

/// A process-wide lock any test that mutates the global `CLANK_LOG_DIR` env var must hold — shared
/// across the `logging` and `session` test modules so their env mutations never race. Test-only.
#[cfg(test)]
pub fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A destination for log lines. Implemented by [`DefaultLogSink`] (native/tests: append directly) and by
/// `clank-agent`'s durability-gated sink (append only when live, to avoid replay duplication). `?Send` —
/// wasip2 is single-threaded, matching the other clank seams.
pub trait LogSink {
    /// Append `line` to `file`. The line already carries no trailing newline; the sink adds one.
    fn append(&self, file: LogFile, line: &str);
}

thread_local! {
    /// The active log sink for the current line (installed by the Session in `eval_line`, restored on
    /// drop). `None` off-session → writes are dropped.
    static ACTIVE: RefCell<Option<Arc<dyn LogSink>>> = const { RefCell::new(None) };
}

/// Install `sink` as the active log sink for the current line; the returned guard restores the previous
/// sink on drop (RAII, mirroring `proctable::install`/`sysprompt::install`).
pub fn install(sink: Arc<dyn LogSink>) -> InstallGuard {
    let prev = ACTIVE.with(|a| a.borrow_mut().replace(sink));
    InstallGuard { prev }
}

/// Restores the previously-active log sink when dropped.
pub struct InstallGuard {
    prev: Option<Arc<dyn LogSink>>,
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        ACTIVE.with(|a| *a.borrow_mut() = self.prev.take());
    }
}

/// The default sink: append directly to the file. Correct on native (no replay) and used by tests. The
/// Golem agent replaces this with a durability-gated sink.
pub struct DefaultLogSink;

impl LogSink for DefaultLogSink {
    fn append(&self, file: LogFile, line: &str) {
        write_line(file, line);
    }
}

/// The raw file append — create the dir + file, append the line and a newline. Best-effort (a logging
/// failure never breaks command execution). Callable by any [`LogSink`] impl (including the agent's
/// live-gated one) to perform the actual write once it has decided to.
pub fn write_line(file: LogFile, line: &str) {
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(file.filename());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
        if !line.ends_with('\n') {
            let _ = f.write_all(b"\n");
        }
    }
}

/// The four log files. Each maps to a fixed filename under [`log_dir`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFile {
    Shell,
    Http,
    Mcp,
    Ops,
}

impl LogFile {
    /// The fixed filename under [`log_dir`].
    pub fn filename(self) -> &'static str {
        match self {
            LogFile::Shell => "shell.log",
            LogFile::Http => "http.log",
            LogFile::Mcp => "mcp.log",
            LogFile::Ops => "ops.log",
        }
    }
}

/// Append one already-formatted line to `file` via the active [`LogSink`] (see the module docs on replay
/// safety). No-op if no sink is installed (off-session). The sink adds the trailing newline.
pub fn append(file: LogFile, line: &str) {
    ACTIVE.with(|a| {
        if let Some(sink) = a.borrow().as_ref() {
            sink.append(file, line);
        }
    });
}

/// A structured log record: a leading ISO-ordinal-free timestamp is deliberately omitted (the agent's
/// clock is a replay-nondeterministic host call — see [[golem-per-agent-serialization]]); instead each
/// line is `key=value` fields prefixed by the log's event kind, machine-parseable and PID-addressable.
/// Fields are rendered in order; values with spaces are quoted.
pub struct Record {
    kind: &'static str,
    fields: Vec<(String, String)>,
}

impl Record {
    pub fn new(kind: &'static str) -> Self {
        Self { kind, fields: Vec::new() }
    }

    /// Add a field. Empty values are skipped (keeps lines tight).
    pub fn field(mut self, key: &str, value: impl AsRef<str>) -> Self {
        let value = value.as_ref();
        if !value.is_empty() {
            self.fields.push((key.to_string(), value.to_string()));
        }
        self
    }

    /// Render to a single log line: `<kind> k1=v1 k2="v with spaces" ...`.
    pub fn render(&self) -> String {
        let mut out = self.kind.to_string();
        for (k, v) in &self.fields {
            out.push(' ');
            out.push_str(k);
            out.push('=');
            if v.chars().any(|c| c.is_whitespace() || c == '"') {
                out.push('"');
                out.push_str(&v.replace('"', "'"));
                out.push('"');
            } else {
                out.push_str(v);
            }
        }
        out
    }

    /// Render and append to `file`.
    pub fn emit(self, file: LogFile) {
        append(file, &self.render());
    }
}

/// Header names whose values are secrets and must never reach `http.log`. Matched case-insensitively.
const SECRET_HEADERS: &[&str] = &["authorization", "x-api-key", "api-key", "cookie", "set-cookie"];

/// Redact a header value if its name is a known secret-bearing header, else pass it through. Used by the
/// http.log emitter so LLM/MCP/registry calls never log API keys or auth tokens.
pub fn redact_header<'a>(name: &str, value: &'a str) -> std::borrow::Cow<'a, str> {
    if SECRET_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(name)) {
        std::borrow::Cow::Borrowed("<redacted>")
    } else {
        std::borrow::Cow::Borrowed(value)
    }
}

/// Redact any occurrence of the given secret substrings inside a free-text blob (e.g. a URL that carries
/// a token as a query param, or a request body). Each non-empty secret is replaced with `<redacted>`.
pub fn redact_text(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if !s.is_empty() {
            out = out.replace(s, "<redacted>");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp-dir guard that points `CLANK_LOG_DIR` at a fresh directory for the test AND installs the
    /// default log sink, restoring both on drop. Serializes via the shared [`test_env_lock`].
    struct LogDirGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        _sink: InstallGuard,
        dir: std::path::PathBuf,
    }
    impl LogDirGuard {
        fn new(tag: &str) -> Self {
            let lock = test_env_lock();
            let dir = std::env::temp_dir().join(format!("clank-log-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::env::set_var(LOG_DIR_ENV, &dir);
            let sink = install(Arc::new(DefaultLogSink));
            Self { _lock: lock, _sink: sink, dir }
        }
        fn read(&self, file: LogFile) -> String {
            std::fs::read_to_string(self.dir.join(file.filename())).unwrap_or_default()
        }
    }
    impl Drop for LogDirGuard {
        fn drop(&mut self) {
            std::env::remove_var(LOG_DIR_ENV);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn append_creates_the_file_and_writes_lines() {
        let g = LogDirGuard::new("creates");
        append(LogFile::Shell, "line one");
        append(LogFile::Shell, "line two");
        assert_eq!(g.read(LogFile::Shell), "line one\nline two\n");
        // Different logs are separate files.
        assert!(g.read(LogFile::Http).is_empty());
    }

    #[test]
    fn append_without_a_sink_is_dropped() {
        // Hold the shared env lock but do NOT install a sink → append writes nothing.
        let _lock = test_env_lock();
        let dir = std::env::temp_dir().join(format!("clank-nosink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var(LOG_DIR_ENV, &dir);
        append(LogFile::Shell, "should be dropped");
        assert!(!dir.join("shell.log").exists());
        std::env::remove_var(LOG_DIR_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_renders_kv_fields_with_quoting() {
        let line = Record::new("cmd")
            .field("pid", "7")
            .field("line", "echo hello world")
            .field("exit", "0")
            .field("skip_empty", "")
            .render();
        assert_eq!(line, r#"cmd pid=7 line="echo hello world" exit=0"#);
        assert!(!line.contains("skip_empty"));
    }

    #[test]
    fn secret_headers_are_redacted() {
        assert_eq!(redact_header("Authorization", "Bearer sk-abc"), "<redacted>");
        assert_eq!(redact_header("x-api-key", "sk-123"), "<redacted>");
        assert_eq!(redact_header("Content-Type", "application/json"), "application/json");
    }

    #[test]
    fn redact_text_masks_known_secrets() {
        let out = redact_text("url?token=sk-abc123&x=1", &["sk-abc123"]);
        assert_eq!(out, "url?token=<redacted>&x=1");
        // An empty secret is a no-op (doesn't blank the whole string).
        assert_eq!(redact_text("hello", &[""]), "hello");
    }
}
