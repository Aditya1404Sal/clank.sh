//! clank.sh shell core.
//!
//! A long-running, terminal-like read/eval/print loop that runs on two targets:
//!
//! - **wasm32** — a `wasi:cli/run` component (WASI 0.3, async p3 streams). Uses the simple
//!   [`eval`] builtin set. See [`wasm`].
//! - **native** — an ordinary executable over blocking `std::io`, with real command execution
//!   via `brush-core` (external programs, pipes, variables, `$?`). See [`native`].
//!
//! Two things are shared and target-agnostic:
//!
//! - [`Transcript`] — the shell-owned, in-memory record of the whole session (every command
//!   typed and the output it produced). This is the value `ask` will later read as context.
//! - [`dispatch_context`] — the clank-specific `context` builtin over that transcript.
//!
//! POC scope: the transcript is in-memory for the session (no compaction, no disk); Brush runs
//! on native only.

mod coreutils;
pub mod session;

#[cfg(target_arch = "wasm32")]
mod wasm;

#[cfg(not(target_arch = "wasm32"))]
pub mod native;

/// The interactive prompt written before each line is read.
pub const PROMPT: &[u8] = b"clank$ ";

const HELP: &[u8] = b"clank.sh builtins:\n  echo [args...]   write arguments to stdout\n  help             show this listing\n  context show     print the session transcript\n  context clear    discard the session transcript\n  exit             leave the shell\n";

/// What the loop should do after evaluating a line.
pub enum Flow {
    /// Keep looping.
    Continue,
    /// Leave the shell.
    Exit,
}

/// A shell-owned, in-memory record of the session: each command typed and the output it
/// produced. Owned by the shell (not the terminal), accumulated for the whole session.
#[derive(Default)]
pub struct Transcript {
    entries: Vec<Entry>,
}

struct Entry {
    command: String,
    output: Vec<u8>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a command line as typed (newline already stripped). Starts a new entry whose
    /// output is filled in by later [`record_output`](Self::record_output) calls.
    pub fn record_command(&mut self, command: &str) {
        self.entries.push(Entry {
            command: command.to_string(),
            output: Vec::new(),
        });
    }

    /// Append output bytes to the most recent command's entry.
    pub fn record_output(&mut self, output: &[u8]) {
        if let Some(last) = self.entries.last_mut() {
            last.output.extend_from_slice(output);
        }
    }

    /// Render the whole session as bytes: each command behind a prompt marker, followed by its
    /// captured output.
    pub fn render(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for e in &self.entries {
            out.extend_from_slice(PROMPT);
            out.extend_from_slice(e.command.as_bytes());
            out.push(b'\n');
            out.extend_from_slice(&e.output);
        }
        out
    }

    /// Discard the whole session (the AI starts fresh on the next `context show`).
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Handle the clank-specific `context` builtin against the transcript. Returns
/// `Some(output_bytes)` if the line was a `context` command, else `None` so the caller falls
/// through to normal command execution (the pure [`eval`] on wasm, or Brush on native).
///
/// `context show` output is intentionally NOT recorded back into the transcript, so the
/// session does not duplicate itself on inspection.
pub fn dispatch_context(transcript: &mut Transcript, line: &str) -> Option<Vec<u8>> {
    let mut words = line.split_whitespace();
    if words.next()? != "context" {
        return None;
    }
    match words.next() {
        Some("show") | None => Some(transcript.render()),
        Some("clear") => {
            transcript.clear();
            Some(Vec::new())
        }
        Some(other) => Some(format!("context: unknown subcommand: {other}\n").into_bytes()),
    }
}

/// Resolve and run a single command line, returning the bytes to write to stdout and the
/// control flow. Pure: no I/O, no async. This is the wasm path's toy builtin set; the native
/// path routes non-`context` lines through Brush instead.
pub fn eval(line: &[u8]) -> (Vec<u8>, Flow) {
    let text = String::from_utf8_lossy(line);
    let mut words = text.split_whitespace();

    let Some(cmd) = words.next() else {
        // Empty line: re-prompt only.
        return (Vec::new(), Flow::Continue);
    };

    match cmd {
        "exit" => (Vec::new(), Flow::Exit),
        "echo" => {
            let mut out = words.collect::<Vec<_>>().join(" ").into_bytes();
            out.push(b'\n');
            (out, Flow::Continue)
        }
        "help" => (HELP.to_vec(), Flow::Continue),
        other => (
            format!("clank: command not found: {other}\n").into_bytes(),
            Flow::Continue,
        ),
    }
}

/// Strip a single trailing `\n` and an optional preceding `\r` (CRLF-tolerant).
pub fn trim_eol(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && line[end - 1] == b'\r' {
            end -= 1;
        }
    }
    &line[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out_of(line: &[u8]) -> String {
        String::from_utf8(eval(line).0).unwrap()
    }

    #[test]
    fn echo_writes_joined_args_with_newline() {
        assert_eq!(out_of(b"echo hello world"), "hello world\n");
    }

    #[test]
    fn echo_with_no_args_writes_blank_line() {
        assert_eq!(out_of(b"echo"), "\n");
    }

    #[test]
    fn exit_signals_exit_with_no_output() {
        let (out, flow) = eval(b"exit");
        assert!(out.is_empty());
        assert!(matches!(flow, Flow::Exit));
    }

    #[test]
    fn help_lists_builtins() {
        assert!(out_of(b"help").contains("builtins"));
    }

    #[test]
    fn empty_line_produces_nothing_and_continues() {
        let (out, flow) = eval(b"   ");
        assert!(out.is_empty());
        assert!(matches!(flow, Flow::Continue));
    }

    #[test]
    fn unknown_command_reports_not_found() {
        assert_eq!(out_of(b"nosuchcmd"), "clank: command not found: nosuchcmd\n");
    }

    #[test]
    fn trim_eol_handles_lf_crlf_and_bare() {
        assert_eq!(trim_eol(b"a\n"), b"a");
        assert_eq!(trim_eol(b"a\r\n"), b"a");
        assert_eq!(trim_eol(b"a"), b"a");
        assert_eq!(trim_eol(b""), b"");
    }

    #[test]
    fn transcript_records_and_renders_commands_and_output() {
        let mut t = Transcript::new();
        t.record_command("echo hi");
        t.record_output(b"hi\n");
        let rendered = String::from_utf8(t.render()).unwrap();
        assert_eq!(rendered, "clank$ echo hi\nhi\n");
    }

    #[test]
    fn transcript_clear_empties_the_session() {
        let mut t = Transcript::new();
        t.record_command("echo hi");
        t.record_output(b"hi\n");
        t.clear();
        assert!(t.render().is_empty());
    }

    #[test]
    fn dispatch_context_show_returns_render_and_clear_empties() {
        let mut t = Transcript::new();
        t.record_command("pwd");
        t.record_output(b"/tmp\n");

        let shown = dispatch_context(&mut t, "context show").unwrap();
        assert_eq!(String::from_utf8(shown).unwrap(), "clank$ pwd\n/tmp\n");

        let cleared = dispatch_context(&mut t, "context clear").unwrap();
        assert!(cleared.is_empty());
        assert!(t.render().is_empty());
    }

    #[test]
    fn dispatch_context_ignores_non_context_lines() {
        let mut t = Transcript::new();
        assert!(dispatch_context(&mut t, "echo hi").is_none());
        assert!(dispatch_context(&mut t, "").is_none());
    }
}
