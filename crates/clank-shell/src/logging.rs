//! The `/var/log/` observability layer (README:613-634). Human-readable, append-only process logs plus
//! structured, PID/PPID-addressable audit events. Four log files under [`log_dir`]:
//!
//! - `shell.log` — per-command start/end/exit-code events and authorization pauses.
//! - `http.log`  — outbound HTTP (MCP, grease registry, curl/wget, and the `ask` LLM call), secrets redacted.
//! - `mcp.log`   — MCP JSON-RPC tool invocations and their responses.
//! - `ops.log`   — destructive operations (the `sudo-only` tier).
//!
//! ## Replay safety — why writes go through a [`LogSink`] seam
//!
//! A file **append** is NOT replay-safe on a Golem agent. The worker filesystem is ephemeral local disk
//! rebuilt from the Initial File System on every start; Golem replays the durable oplog by **re-running
//! the guest code**, and a raw `std::fs` append is a local side effect that Golem neither records to the
//! oplog nor skips on replay — so a crash-then-recovery would re-run the append and **duplicate the
//! line**. (Whole-file `std::fs::write`, as grease uses for its store, is idempotent and therefore safe;
//! only append is the hazard. See [[golem-fs-append-replay-unsafe]].)
//!
//! So writes route through a [`LogSink`] installed per-line (the thread-local pattern used by
//! `proctable`/`sysprompt`). On native there is no replay, so the [default sink](DefaultLogSink) appends
//! directly via [`write_line`]. On the Golem agent, `clank-agent` injects a `DurableLogSink` that
//! accumulates each log in an in-memory buffer (deterministically rebuilt by replay, never seeded from
//! disk) and rewrites the whole file with an **idempotent `std::fs::write`** — so replay converges to the
//! identical file with no duplicated lines. (golem-rust does not expose the durable-execution `is-live`
//! bit publicly, so the whole-file-rewrite approach is used rather than a live-vs-replay gate.) If no
//! sink is installed (off-session reads), writes are dropped.
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
///
/// Every line is first passed through [`crate::runtime::secretenv::mask_values`] so any registered
/// `export --secret` VALUE that reached this record (e.g. echoed into an http.log body or an mcp.log
/// argument while the secret set is installed) is masked to `<redacted>` — the README's "secrets
/// redacted" / "never written to logs" contract, enforced centrally at the one write choke point. It
/// is a no-op when no secret set is installed, so secret-free lines are unaffected.
pub fn append(file: LogFile, line: &str) {
    let masked = crate::runtime::secretenv::mask_values(line);
    ACTIVE.with(|a| {
        if let Some(sink) = a.borrow().as_ref() {
            sink.append(file, &masked);
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

    /// Render to a single log line: `<kind> k1=v1 k2="v with spaces" ...`. A value is quoted when it
    /// contains whitespace, a quote, a backslash, or a control character; inside quotes, `\`/`"` and the
    /// control chars `\n`/`\r`/`\t` are backslash-escaped. This guarantees exactly ONE physical line per
    /// record (an embedded newline can never split it) and that the escaped value round-trips faithfully.
    pub fn render(&self) -> String {
        let mut out = self.kind.to_string();
        for (k, v) in &self.fields {
            out.push(' ');
            out.push_str(k);
            out.push('=');
            if v.chars().any(needs_quoting) {
                out.push('"');
                for c in v.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        // Any other control char → a space, so the line stays printable + single-line.
                        c if c.is_control() => out.push(' '),
                        c => out.push(c),
                    }
                }
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

/// Whether a character forces its field value to be quoted in [`Record::render`] — whitespace (so a
/// space/newline can't split the record), a quote or backslash (the escape chars), or any control char.
fn needs_quoting(c: char) -> bool {
    c.is_whitespace() || c == '"' || c == '\\' || c.is_control()
}

/// Trim `buf` (in place) to at most `max_bytes`, dropping WHOLE leading lines so no partial line is left.
/// Used by the agent's whole-file-rewrite log sink to keep a bounded rolling tail — deterministic, so
/// oplog replay reproduces the identical tail and the whole-file write stays idempotent. A single line
/// longer than `max_bytes` is kept intact (never split mid-line).
pub fn bound_tail(buf: &mut String, max_bytes: usize) {
    if buf.len() <= max_bytes {
        return;
    }
    let cut = buf.len() - max_bytes;
    // Advance to just past the next newline so the retained tail starts on a line boundary.
    let start = buf[cut..].find('\n').map_or(0, |i| cut + i + 1);
    // If the only newline is the very last byte (start == buf.len()), keep the last line rather than
    // emptying the buffer.
    let start = if start >= buf.len() {
        buf[..buf.len().saturating_sub(1)].rfind('\n').map_or(0, |i| i + 1)
    } else {
        start
    };
    buf.drain(..start);
}

/// Header names whose values are secrets and must never reach `http.log`. Matched case-insensitively.
const SECRET_HEADERS: &[&str] = &["authorization", "x-api-key", "api-key", "cookie", "set-cookie"];

/// Query-parameter names whose values are secrets and must be masked out of a logged URL. Matched
/// case-insensitively against the part before `=`.
const SECRET_QUERY_PARAMS: &[&str] =
    &["token", "access_token", "api_key", "apikey", "key", "secret", "password", "sig", "signature"];

/// Redact a header value if its name is a known secret-bearing header, else pass it through. Used by the
/// http.log emitter so LLM/MCP/registry calls never log API keys or auth tokens.
pub fn redact_header<'a>(name: &str, value: &'a str) -> std::borrow::Cow<'a, str> {
    if SECRET_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(name)) {
        std::borrow::Cow::Borrowed("<redacted>")
    } else {
        std::borrow::Cow::Borrowed(value)
    }
}

/// Mask secret query-parameter values in a URL before it is logged, e.g.
/// `https://h/mcp?token=sk-abc&x=1` → `https://h/mcp?token=<redacted>&x=1`. Anything before the `?` is
/// untouched. A parameter whose name (case-insensitive) is in [`SECRET_QUERY_PARAMS`] has its value
/// replaced. Non-secret params and a URL with no query string pass through unchanged.
pub fn redact_url(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let masked = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, _)) if SECRET_QUERY_PARAMS.iter().any(|p| p.eq_ignore_ascii_case(name)) => {
                format!("{name}=<redacted>")
            }
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{masked}")
}

/// Redact any occurrence of the given secret substrings inside a free-text blob (e.g. a request body or
/// an error message that may echo a token). Each non-empty secret is replaced with `<redacted>`.
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
    fn record_render_is_always_one_physical_line() {
        // A value with a newline, a quote, and a backslash must not break the single-line format.
        let line = Record::new("start")
            .field("line", "echo \"hi\"\nrm -rf /\tC:\\path")
            .render();
        assert!(!line.contains('\n'), "no raw newline may survive: {line:?}");
        assert!(line.contains(r#"\n"#), "the newline is escaped: {line}");
        assert!(line.contains(r#"\""#), "the quote is escaped: {line}");
        assert!(line.contains(r#"\\"#), "the backslash is escaped: {line}");
        assert!(line.contains(r#"\t"#), "the tab is escaped: {line}");
        // Exactly one line.
        assert_eq!(line.lines().count(), 1);
    }

    #[test]
    fn secret_headers_are_redacted() {
        assert_eq!(redact_header("Authorization", "Bearer sk-abc"), "<redacted>");
        assert_eq!(redact_header("x-api-key", "sk-123"), "<redacted>");
        assert_eq!(redact_header("Content-Type", "application/json"), "application/json");
    }

    #[test]
    fn bound_tail_keeps_whole_recent_lines_under_the_cap() {
        // Under the cap → untouched.
        let mut s = "a\nb\nc\n".to_string();
        bound_tail(&mut s, 1000);
        assert_eq!(s, "a\nb\nc\n");

        // Over the cap → drops whole leading lines, keeps a line-aligned tail under the cap.
        let mut s = String::new();
        for i in 0..100 {
            s.push_str(&format!("line{i}\n"));
        }
        bound_tail(&mut s, 30);
        assert!(s.len() <= 30, "tail must be under the cap, got {}", s.len());
        assert!(s.starts_with("line"), "tail starts on a line boundary, got {s:?}");
        assert!(s.ends_with("line99\n"), "the newest line survives, got {s:?}");
        // Determinism: bounding an already-bounded buffer is a no-op (replay idempotency).
        let once = s.clone();
        bound_tail(&mut s, 30);
        assert_eq!(s, once);
    }

    #[test]
    fn bound_tail_keeps_a_single_oversized_last_line() {
        // A lone line longer than the cap is kept intact (never split mid-line).
        let mut s = "x".repeat(100);
        s.push('\n');
        bound_tail(&mut s, 10);
        assert_eq!(s, format!("{}\n", "x".repeat(100)));
    }

    #[test]
    fn secret_query_params_are_masked_in_urls() {
        assert_eq!(
            redact_url("https://h/mcp?token=sk-abc123&repo=x"),
            "https://h/mcp?token=<redacted>&repo=x"
        );
        // Case-insensitive param name; multiple secrets; non-secret params kept.
        assert_eq!(
            redact_url("https://h/p?API_KEY=k&page=2&Signature=zz"),
            "https://h/p?API_KEY=<redacted>&page=2&Signature=<redacted>"
        );
        // No query string / no secret param → unchanged.
        assert_eq!(redact_url("https://h/repo/README.md"), "https://h/repo/README.md");
        assert_eq!(redact_url("https://h/p?page=2"), "https://h/p?page=2");
    }

    #[test]
    fn redact_text_masks_known_secrets() {
        let out = redact_text("url?token=sk-abc123&x=1", &["sk-abc123"]);
        assert_eq!(out, "url?token=<redacted>&x=1");
        // An empty secret is a no-op (doesn't blank the whole string).
        assert_eq!(redact_text("hello", &[""]), "hello");
    }
}
