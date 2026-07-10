//! clank.sh shell core.
//!
//! A long-running, terminal-like read/eval/print loop that runs on two targets:
//!
//! - **wasm32** — a `wasi:cli/run` component (WASI 0.3, async p3 streams). See [`wasm`].
//! - **native** — an ordinary executable over blocking `std::io`. See [`native`].
//!
//! Two things are shared and target-agnostic:
//!
//! - [`Transcript`] — the shell-owned, in-memory record of the whole session (every command
//!   typed and the output it produced). This is the value `ask` will later read as context.
//! - [`dispatch_context`] — the clank-specific `context` builtin over that transcript.
//!
//! POC scope: the transcript is in-memory for the session (no compaction, no disk); Brush runs
//! on both targets through [`session::Session`].

pub mod askcmd;
pub mod authz;
pub mod binfs;
mod coreutils;
mod httpcmd;
mod mancmd;
pub mod manifest;
pub mod process;
pub mod procfs;
pub mod proctable;
pub mod promptuser;
mod ps;
pub mod registry;
pub mod session;
mod statcmd;
mod texttools;
pub mod typecmd;
mod which;

// The `wasi:cli/run` REPL driver. Gated behind the `repl-driver` feature so that dependents which
// only want the shared `Session` core (e.g. the Golem agent crate, which exports its own
// `golem:agent` world) can link this crate without re-emitting a clashing `wasi:cli/run` export.
#[cfg(all(target_arch = "wasm32", feature = "repl-driver"))]
mod wasm;

#[cfg(not(target_arch = "wasm32"))]
pub mod native;

/// The interactive prompt written before each line is read.
pub const PROMPT: &[u8] = b"clank$ ";

const HELP: &[u8] = b"clank.sh builtins:\n  echo [args...]     write arguments to stdout\n  help               show this listing\n  context show       print the session transcript\n  context clear      discard the session transcript\n  context budget [n] show or set the transcript token budget\n  exit               leave the shell\n";

/// What the loop should do after evaluating a line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flow {
    /// Keep looping.
    Continue,
    /// Leave the shell.
    Exit,
}

/// Default sliding-window budget, in estimated tokens. Sized so an ordinary session accumulates
/// freely but a runaway one is bounded before it would blow an LLM context window. Tunable via
/// [`Transcript::with_budget`] / [`Transcript::set_budget`]; the AI layer will drive this from
/// the model's real context size later.
pub const DEFAULT_TOKEN_BUDGET: usize = 24_000;

/// A shell-owned, in-memory record of the session: each command typed and the output it
/// produced. Owned by the shell (not the terminal), accumulated for the whole session.
///
/// The record is a **sliding window**: it is kept under [`token_budget`](Self::token_budget)
/// estimated tokens by dropping the oldest entries once the budget is exceeded. Dropped entries
/// are replaced by a single leading [`Entry::Elided`] marker so the boundary between discarded
/// and live history stays explicit (rather than history silently vanishing). Today the marker
/// just records how many entries were dropped; when the AI layer lands it becomes the slot for a
/// generated summary of that dropped span.
pub struct Transcript {
    entries: Vec<Entry>,
    token_budget: usize,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            token_budget: DEFAULT_TOKEN_BUDGET,
        }
    }
}

enum Entry {
    /// A command line and the output it produced.
    Command { command: String, output: Vec<u8> },
    /// A leading marker standing in for `count` older entries dropped to stay under budget.
    /// (Future: carries a generated summary of the dropped span.)
    Elided { count: usize },
}

/// Estimate the token cost of `byte_len` bytes of text. A deliberately crude heuristic
/// (~4 bytes/token) with zero dependencies — a placeholder for a real tokenizer, which the AI
/// layer can swap in without touching the windowing logic.
fn est_tokens(byte_len: usize) -> usize {
    byte_len.div_ceil(4)
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a transcript with an explicit token budget (mainly for tests and tuning).
    pub fn with_budget(token_budget: usize) -> Self {
        Self {
            entries: Vec::new(),
            token_budget,
        }
    }

    /// The current sliding-window budget, in estimated tokens.
    pub fn budget(&self) -> usize {
        self.token_budget
    }

    /// Set the sliding-window budget and immediately re-enforce it.
    pub fn set_budget(&mut self, token_budget: usize) {
        self.token_budget = token_budget;
        self.enforce_budget();
    }

    /// Record a command line as typed (newline already stripped). Starts a new entry whose
    /// output is filled in by later [`record_output`](Self::record_output) calls.
    pub fn record_command(&mut self, command: &str) {
        self.entries.push(Entry::Command {
            command: command.to_string(),
            output: Vec::new(),
        });
    }

    /// Append output bytes to the most recent command's entry, then enforce the window budget.
    /// Enforcement runs here (not in `record_command`) so an entry is costed with its output.
    pub fn record_output(&mut self, output: &[u8]) {
        if let Some(Entry::Command { output: buf, .. }) = self.entries.last_mut() {
            buf.extend_from_slice(output);
        }
        self.enforce_budget();
    }

    /// Estimated token cost of a single entry.
    fn entry_tokens(entry: &Entry) -> usize {
        match entry {
            Entry::Command { command, output } => est_tokens(command.len() + output.len()),
            // The marker's own text is tiny and bounded; count it as negligible so a run of
            // drops doesn't itself push the window over budget.
            Entry::Elided { .. } => 0,
        }
    }

    /// Total estimated token cost of the current window.
    fn total_tokens(&self) -> usize {
        self.entries.iter().map(Self::entry_tokens).sum()
    }

    /// Drop oldest `Command` entries until the window fits the budget, folding each drop into a
    /// single leading `Elided` marker. Never drops the marker itself, and never drops the most
    /// recent entry (a single oversized entry stays — degenerate but honest, and the loop can't
    /// spin forever).
    fn enforce_budget(&mut self) {
        while self.total_tokens() > self.token_budget {
            // Index of the oldest `Command` entry, and whether a leading marker already exists.
            let has_marker = matches!(self.entries.first(), Some(Entry::Elided { .. }));
            let oldest_cmd = if has_marker { 1 } else { 0 };

            // Stop if dropping would leave nothing, or would drop the current (last) entry.
            if oldest_cmd >= self.entries.len() || oldest_cmd == self.entries.len() - 1 {
                break;
            }

            self.entries.remove(oldest_cmd);
            if let Some(Entry::Elided { count }) = self.entries.first_mut() {
                *count += 1;
            } else {
                self.entries.insert(0, Entry::Elided { count: 1 });
            }
        }
    }

    /// Render the whole window as bytes: a leading `[N earlier entries dropped]` marker if any
    /// history was elided, then each command behind a prompt marker followed by its output.
    pub fn render(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for e in &self.entries {
            match e {
                Entry::Elided { count } => {
                    out.extend_from_slice(
                        format!("[{count} earlier entries dropped]\n").as_bytes(),
                    );
                }
                Entry::Command { command, output } => {
                    out.extend_from_slice(PROMPT);
                    out.extend_from_slice(command.as_bytes());
                    out.push(b'\n');
                    out.extend_from_slice(output);
                }
            }
        }
        out
    }

    /// Discard the whole session — entries and the elision marker (the AI starts fresh on the
    /// next `context show`).
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Handle the clank-specific `context` builtin against the transcript. Returns
/// `Some(output_bytes)` if the line was a `context` command, else `None` so the caller falls
/// through to normal command execution.
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
        // `context budget [<n>]` — with no argument, prints the current sliding-window token
        // budget; with a number, sets it (and re-enforces immediately). This is the runtime knob
        // for the window until the AI layer drives it from the model's real context size.
        Some("budget") => match words.next() {
            None => Some(format!("{}\n", transcript.budget()).into_bytes()),
            Some(arg) => match arg.parse::<usize>() {
                Ok(n) => {
                    transcript.set_budget(n);
                    Some(Vec::new())
                }
                Err(_) => Some(format!("context: budget: not a number: {arg}\n").into_bytes()),
            },
        },
        Some(other) => Some(format!("context: unknown subcommand: {other}\n").into_bytes()),
    }
}

/// Resolve and run a single command line with a tiny pure builtin set.
///
/// This is retained for core unit tests. Real native, wasm REPL, and Golem-agent execution goes
/// through [`session::Session`].
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
        assert_eq!(
            out_of(b"nosuchcmd"),
            "clank: command not found: nosuchcmd\n"
        );
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

    // --- Sliding window / token budget ---

    /// Record a `command` + `output` pair into the transcript.
    fn record(t: &mut Transcript, command: &str, output: &[u8]) {
        t.record_command(command);
        t.record_output(output);
    }

    #[test]
    fn est_tokens_rounds_up_by_four() {
        assert_eq!(est_tokens(0), 0);
        assert_eq!(est_tokens(1), 1);
        assert_eq!(est_tokens(4), 1);
        assert_eq!(est_tokens(5), 2);
        assert_eq!(est_tokens(8), 2);
    }

    #[test]
    fn under_budget_keeps_everything_with_no_marker() {
        // Generous budget: nothing is evicted, render matches the plain format exactly.
        let mut t = Transcript::with_budget(10_000);
        record(&mut t, "echo a", b"a\n");
        record(&mut t, "echo b", b"b\n");
        let rendered = String::from_utf8(t.render()).unwrap();
        assert_eq!(rendered, "clank$ echo a\na\nclank$ echo b\nb\n");
        assert!(!rendered.contains("dropped"));
    }

    #[test]
    fn over_budget_drops_oldest_and_marks_it() {
        // Tiny budget forces eviction. Each entry ~ est_tokens(len(cmd)+len(out)).
        let mut t = Transcript::with_budget(4);
        record(&mut t, "aaaa", b"aaaa"); // 8 bytes -> 2 tokens
        record(&mut t, "bbbb", b"bbbb"); // pushes total to 4 tokens (== budget, ok)
        record(&mut t, "cccc", b"cccc"); // now over budget -> evict oldest
        let rendered = String::from_utf8(t.render()).unwrap();
        // Newest survives, oldest is gone, a marker leads.
        assert!(rendered.starts_with("[1 earlier entries dropped]\n"));
        assert!(rendered.contains("cccc"));
        assert!(!rendered.contains("clank$ aaaa"));
    }

    #[test]
    fn consecutive_evictions_coalesce_into_one_marker() {
        let mut t = Transcript::with_budget(2); // ~8 bytes of headroom
        for i in 0..6 {
            record(&mut t, &format!("cmd{i}"), b"xxxx");
        }
        let rendered = String::from_utf8(t.render()).unwrap();
        // Exactly one marker line, and its count equals the number of dropped entries.
        assert_eq!(rendered.matches("earlier entries dropped").count(), 1);
        // The most recent command is always retained.
        assert!(rendered.contains("cmd5"));
        // The marker count + surviving commands should reconcile: 6 recorded, some dropped.
        let count: usize = rendered
            .split_once("earlier")
            .and_then(|(head, _)| head.trim_start_matches('[').trim().parse().ok())
            .unwrap();
        let surviving = rendered.matches("clank$ cmd").count();
        assert_eq!(count + surviving, 6);
    }

    #[test]
    fn single_oversized_entry_is_kept_not_looped() {
        // One entry alone exceeds the budget; it must stay (no infinite loop, no empty window).
        let mut t = Transcript::with_budget(1);
        record(&mut t, "big", &vec![b'x'; 1000]);
        let rendered = String::from_utf8(t.render()).unwrap();
        assert!(rendered.contains("clank$ big"));
        assert!(!rendered.contains("dropped"));
    }

    #[test]
    fn clear_wipes_entries_and_marker() {
        let mut t = Transcript::with_budget(2);
        for i in 0..5 {
            record(&mut t, &format!("cmd{i}"), b"xxxx");
        }
        assert!(String::from_utf8(t.render()).unwrap().contains("dropped"));
        t.clear();
        assert!(t.render().is_empty());
    }

    #[test]
    fn context_budget_reports_and_sets() {
        let mut t = Transcript::new();
        // No arg → prints the default budget.
        let shown = dispatch_context(&mut t, "context budget").unwrap();
        assert_eq!(
            String::from_utf8(shown).unwrap(),
            format!("{DEFAULT_TOKEN_BUDGET}\n")
        );
        // Set → empty output, and the new value sticks.
        assert!(dispatch_context(&mut t, "context budget 5")
            .unwrap()
            .is_empty());
        assert_eq!(t.budget(), 5);
        // Non-numeric → error message.
        let err = dispatch_context(&mut t, "context budget nope").unwrap();
        assert!(String::from_utf8(err).unwrap().contains("not a number"));
    }

    #[test]
    fn shrinking_budget_reenforces_immediately() {
        let mut t = Transcript::with_budget(10_000);
        record(&mut t, "aaaa", b"aaaa");
        record(&mut t, "bbbb", b"bbbb");
        record(&mut t, "cccc", b"cccc");
        assert!(!String::from_utf8(t.render()).unwrap().contains("dropped"));
        t.set_budget(2);
        let rendered = String::from_utf8(t.render()).unwrap();
        assert!(rendered.contains("dropped"));
        assert!(rendered.contains("cccc")); // newest retained
    }
}
