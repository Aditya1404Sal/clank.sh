//! Native target: the shell as an ordinary executable over blocking `std::io`. All command
//! execution and output capture live in the shared [`crate::session::Session`]; this driver is
//! just the prompt/read/write loop.
//!
//! Two loops, chosen by whether stdin is a TTY:
//! - **[`run_interactive`]** — a rich [`reedline`] line editor for humans: history + Ctrl-R search,
//!   arrow-key editing, fish-style ghost autosuggestions, live command highlighting, and a styled
//!   "starship-lite" prompt (`clank  <cwd>  <branch> ❯`). Output is neatly presented (a trailing
//!   newline is guaranteed so the next prompt never glues onto command output; stderr is dimmed).
//! - **[`run_plain`]** — the original blocking `read_line` loop, used verbatim when stdin is piped
//!   (`echo cmd | clank`, the e2e scripts, the tests). No terminal, no ANSI, no behavior change.

use crate::session::Session;
use crate::{trim_eol, Flow, PROMPT};
use std::io::{self, IsTerminal, Write};

/// Run the interactive read/eval/print loop until `exit` or end-of-input.
///
/// Dispatches to the rich [`reedline`] editor for an interactive terminal, or the plain blocking
/// loop when stdin is not a TTY (pipes, the test/e2e drivers).
///
/// # Errors
///
/// Returns an error if the [`Session`] fails to initialize, or if reading a line from stdin or
/// writing to stdout fails.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut session = Session::new().await?;
    inject_native_providers(&mut session);
    if io::stdin().is_terminal() {
        run_interactive(&mut session).await
    } else {
        run_plain(&mut session).await
    }
}

/// The plain blocking loop: prompt, `read_line`, eval, write. Used when stdin is not a terminal, so
/// piped input and the test/e2e drivers behave exactly as before (no ANSI, no line editor).
async fn run_plain(session: &mut Session) -> Result<(), Box<dyn std::error::Error>> {
    let mut line = String::new();

    loop {
        write_stdout(PROMPT)?;

        line.clear();
        if read_line_raw(&mut line)? == 0 {
            // EOF: leave the cursor on a fresh line after the dangling prompt.
            write_stdout(b"\n")?;
            break;
        }

        let mut line_str = String::from_utf8_lossy(trim_eol(line.as_bytes())).into_owned();

        // `ask repl` is a native-terminal feature: run the interactive REPL loop inline (the native
        // driver owns the terminal, so it can block on human input between turns — the durable agent
        // cannot, and returns an honest message from `eval_line`). See `Session::repl_*`.
        if let Some(args) = crate::ai::ask::classify_repl(&line_str) {
            run_repl(session, &args).await?;
            continue;
        }

        // PS2 continuation: while the input is syntactically incomplete (open heredoc, quote, or
        // substitution), keep reading lines — this driver owns the terminal, so a heredoc can be
        // typed the way every shell user expects. Ctrl-D mid-construct discards the construct, not
        // the shell. The cap is a backstop against a pathological feed; overflowing it falls
        // through to eval, where the Session's incomplete-input check answers exit 2 honestly.
        #[allow(clippy::items_after_statements)] // the cap lives beside its explanatory comment
        const MAX_CONTINUATION_LINES: usize = 512;
        let mut continuations = 0usize;
        let mut aborted = false;
        while session.line_is_incomplete(&line_str) && continuations < MAX_CONTINUATION_LINES {
            write_stdout(b"> ")?;
            line.clear();
            if read_line_raw(&mut line)? == 0 {
                write_stdout(b"\nclank: incomplete input discarded\n")?;
                aborted = true;
                break;
            }
            line_str.push('\n');
            line_str.push_str(&String::from_utf8_lossy(trim_eol(line.as_bytes())));
            continuations += 1;
        }
        if aborted {
            continue;
        }

        let result = session.eval_line(&line_str).await;
        write_stdout(&result.terminal_output())?;
        let flow = result.flow;

        // If the line surfaced a `prompt-user` question, collect the human's answer inline (the
        // native REPL owns the terminal) and deliver it. An answer outside `--choices` leaves the
        // prompt pending, so keep reading until it resolves. EOF is an abort.
        if let Some(prompt) = &result.pending_prompt {
            // Show the valid answers. The question alone leaves the human guessing — a `prompt-user`
            // with `--choices` printed nothing but the text, so a plausible-looking `y` against
            // `[yes/no]` was rejected with no visible reason. (The sudo/authz gate looks fine only
            // because it spells "(y)es, (n)o" inside its question string.)
            if let Some(choices) = &prompt.choices {
                if !choices.is_empty() {
                    write_stdout(format!("[{}]\n", choices.join("/")).as_bytes())?;
                }
            }
        }
        if result.pending_prompt.is_some() {
            while session.has_pending_prompt() {
                line.clear();
                let answer = if read_line_raw(&mut line)? == 0 {
                    session.answer_prompt(None).await // EOF → abort
                } else {
                    let answer_str = String::from_utf8_lossy(trim_eol(line.as_bytes())).into_owned();
                    session.answer_prompt(Some(answer_str)).await
                };
                write_stdout(&answer.terminal_output())?;
            }
        }

        if let Flow::Exit = flow {
            break;
        }
    }

    Ok(())
}

/// The interactive loop: a [`reedline`] line editor with history, ghost autosuggestions, live
/// command highlighting, and the styled starship-lite prompt. Only reached when stdin is a TTY.
///
/// The editor is idle (terminal in cooked mode) between `read_line` calls, so `ask repl` and
/// `prompt-user` answer collection reuse the plain blocking reads unchanged.
async fn run_interactive(session: &mut Session) -> Result<(), Box<dyn std::error::Error>> {
    use reedline::Signal;

    // Backstop against a pathological continuation feed (matches the plain loop).
    const MAX_CONTINUATION_LINES: usize = 512;

    let mut editor = build_editor();
    let mut last_ok = true;

    loop {
        let prompt = StarshipPrompt::new(last_ok);
        let buffer = match editor.read_line(&prompt) {
            Ok(Signal::Success(buffer)) => buffer,
            // Ctrl-C cancels the current line (like every shell) rather than killing the session.
            Ok(Signal::CtrlC) => continue,
            // Ctrl-D on an empty line leaves the shell.
            Ok(Signal::CtrlD) => break,
            // reedline's cursor-position query can time out right after a long command (notably
            // `ask`): the terminal answers `\x1b[6n` too late, and its late reply — which carries no
            // newline, so cooked mode holds it until the next Enter — would otherwise leak into the
            // next line as `^[[..R`. Drain that stray reply (in raw mode, so the newline-less bytes
            // are readable) and read THIS line with a plain prompt; the editor resumes on the next
            // line once the terminal settles. One line degrades, never the whole session, no leak.
            Err(_) => {
                // Let the late cursor-position reply land (it missed reedline's own timeout by a
                // hair), then drain it before we read — so it can't slip in between the drain and
                // the plain read. Only the rare fallback line pays this ~100ms.
                std::thread::sleep(std::time::Duration::from_millis(100));
                drain_terminal_input();
                write_stdout(&StarshipPrompt::new(last_ok).plain_bytes())?;
                let mut line = String::new();
                if read_line_raw(&mut line)? == 0 {
                    write_stdout(b"\n")?;
                    break;
                }
                line.trim_end_matches(['\n', '\r']).to_string()
            }
        };
        let mut line_str = buffer.trim_end_matches(['\n', '\r']).to_string();

        // `ask repl` — reuse the plain read-driven REPL (the terminal is cooked here; reedline only
        // holds raw mode during its own `read_line`).
        if let Some(args) = crate::ai::ask::classify_repl(&line_str) {
            run_repl(session, &args).await?;
            last_ok = true;
            continue;
        }

        // PS2 continuation for incomplete input (heredocs, open quotes/substitutions): read more
        // physical lines via reedline with a dim `· ` continuation prompt. Ctrl-C/Ctrl-D discards
        // the construct, not the shell.
        let mut continuations = 0usize;
        let mut aborted = false;
        while session.line_is_incomplete(&line_str) && continuations < MAX_CONTINUATION_LINES {
            match editor.read_line(&ContinuationPrompt) {
                Ok(Signal::Success(more)) => {
                    line_str.push('\n');
                    line_str.push_str(more.trim_end_matches(['\n', '\r']));
                    continuations += 1;
                }
                Ok(Signal::CtrlC | Signal::CtrlD) => {
                    write_stdout(b"\n")?;
                    aborted = true;
                    break;
                }
                Err(_) => {
                    aborted = true;
                    break;
                }
            }
        }
        if aborted {
            continue;
        }

        let result = session.eval_line(&line_str).await;
        render_output(&result.stdout, &result.stderr)?;
        last_ok = result.exit_code == 0;
        let flow = result.flow;

        // `prompt-user`: surface the valid answers, then collect the human's response with plain
        // blocking reads (reedline is idle, terminal cooked) until the prompt resolves.
        if let Some(prompt) = &result.pending_prompt {
            if let Some(choices) = &prompt.choices {
                if !choices.is_empty() {
                    write_stdout(format!("[{}]\n", choices.join("/")).as_bytes())?;
                }
            }
        }
        if result.pending_prompt.is_some() {
            let mut line = String::new();
            while session.has_pending_prompt() {
                line.clear();
                let answer = if read_line_raw(&mut line)? == 0 {
                    session.answer_prompt(None).await // EOF → abort
                } else {
                    let answer_str = String::from_utf8_lossy(trim_eol(line.as_bytes())).into_owned();
                    session.answer_prompt(Some(answer_str)).await
                };
                render_output(&answer.stdout, &answer.stderr)?;
                last_ok = answer.exit_code == 0;
            }
        }

        if let Flow::Exit = flow {
            break;
        }
    }

    Ok(())
}

/// Build the [`reedline`] editor: file-backed history (best-effort), a dim ghost-hint from history,
/// and the [`CommandHighlighter`] (the command word turns green when recognized).
fn build_editor() -> reedline::Reedline {
    use nu_ansi_term::{Color, Style};
    use reedline::{DefaultHinter, FileBackedHistory, Reedline};

    let mut editor = Reedline::create()
        .with_hinter(Box::new(
            DefaultHinter::default().with_style(Style::new().fg(Color::DarkGray)),
        ))
        .with_highlighter(Box::new(CommandHighlighter::new()));

    // Persist history across sessions when we can open the file; otherwise reedline keeps an
    // in-memory history (the default), so a read-only HOME degrades cleanly to no persistence.
    if let Some(hist) = history_path().and_then(|p| FileBackedHistory::with_file(2000, p).ok()) {
        editor = editor.with_history(Box::new(hist));
    }
    editor
}

/// Highlights the **command word** (first token) green when it's a recognized command, leaving
/// everything else at the default colour. Deliberately NOT reedline's `ExampleHighlighter`, which
/// substring-matches (so `false` lights up the `ls` inside it) and reddens anything not in its list
/// (so brush builtins like `echo` read as errors). The recognized set is clank's registry plus the
/// brush builtins/keywords that carry no clank manifest.
struct CommandHighlighter {
    known: std::collections::HashSet<String>,
}

impl CommandHighlighter {
    fn new() -> Self {
        let mut known: std::collections::HashSet<String> = crate::registry::build()
            .names()
            .map(std::string::ToString::to_string)
            .collect();
        // Brush builtins / shell keywords with no clank manifest — recognized, not "unknown".
        for word in [
            "echo", "true", "false", "test", "pwd", "let", "eval", "trap", "getopts", "shift",
            "return", "local", "declare", "readonly", "set", "shopt", "if", "then", "else", "elif",
            "fi", "for", "while", "until", "do", "done", "case", "esac", "in", "function", "select",
            "time", ":", ".",
        ] {
            known.insert(word.to_string());
        }
        Self { known }
    }
}

impl reedline::Highlighter for CommandHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> reedline::StyledText {
        use nu_ansi_term::{Color, Style};
        let mut styled = reedline::StyledText::new();
        let lead_len = line.len() - line.trim_start().len();
        let (lead, rest) = line.split_at(lead_len);
        let cmd_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (cmd, tail) = rest.split_at(cmd_len);
        if !lead.is_empty() {
            styled.push((Style::new(), lead.to_string()));
        }
        let cmd_style = if self.known.contains(cmd) {
            Style::new().fg(Color::Green)
        } else {
            Style::new()
        };
        styled.push((cmd_style, cmd.to_string()));
        styled.push((Style::new(), tail.to_string()));
        styled
    }
}

/// `$XDG_DATA_HOME/clank/history.txt` (or `~/.local/share/clank/…`), creating the directory. `None`
/// if neither var is set or the directory can't be created.
fn history_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")))?;
    let dir = base.join("clank");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("history.txt"))
}

/// Write `stdout` verbatim, then `stderr` (dimmed if it carries no ANSI of its own), and guarantee
/// the line ends in a newline — so the next prompt never glues onto output that lacked a trailing
/// `\n`. A dim `⏎` marks where that missing newline was inserted.
fn render_output(stdout: &[u8], stderr: &[u8]) -> io::Result<()> {
    use nu_ansi_term::{Color, Style};

    write_stdout(stdout)?;
    if !stderr.is_empty() {
        // Keep stderr on its own line: when stdout didn't end in a newline, separate the two so a
        // model answer (ask's stdout) never glues onto its dimmed `[tool]` trace (ask's stderr) —
        // matching the golem shell's and clank-repl.sh's renderers.
        if !stdout.is_empty() && !stdout.ends_with(b"\n") {
            write_stdout(b"\n")?;
        }
        if stderr.contains(&0x1b) {
            // Already styled by the tool — pass through so we don't clobber its colors.
            write_stdout(stderr)?;
        } else {
            let dimmed = Style::new()
                .dimmed()
                .paint(String::from_utf8_lossy(stderr))
                .to_string();
            write_stdout(dimmed.as_bytes())?;
        }
    }

    let printed = !stdout.is_empty() || !stderr.is_empty();
    let ends_with_newline = if stderr.is_empty() {
        stdout.ends_with(b"\n")
    } else {
        stderr.ends_with(b"\n")
    };
    if printed && !ends_with_newline {
        write_stdout(Color::DarkGray.paint("⏎").to_string().as_bytes())?;
        write_stdout(b"\n")?;
    }
    Ok(())
}

/// The starship-lite prompt: `clank` (cyan) · cwd (blue, `~`-abbreviated) · git branch (dim) · a
/// `❯` indicator that is green after a successful command and red after a failure.
struct StarshipPrompt {
    /// Pre-rendered, ANSI-styled left segment (`clank  <cwd>  <branch>`).
    left: String,
    /// Whether the previous command exited 0 (drives the indicator colour).
    ok: bool,
}

impl StarshipPrompt {
    fn new(ok: bool) -> Self {
        use nu_ansi_term::Color;
        let mut left = Color::Cyan.bold().paint("clank").to_string();
        left.push(' ');
        left.push_str(&Color::Blue.paint(cwd_display()).to_string());
        if let Some(branch) = git_branch() {
            left.push(' ');
            left.push_str(&Color::DarkGray.paint(format!("⎇ {branch}")).to_string());
        }
        Self { left, ok }
    }

    /// The prompt rendered as bytes for the plain (`write_stdout`) fallback read, matching the
    /// interactive look: `<left> ❯ ` with a green (ok) / red (failed) indicator.
    fn plain_bytes(&self) -> Vec<u8> {
        let color = if self.ok {
            nu_ansi_term::Color::Green
        } else {
            nu_ansi_term::Color::Red
        };
        format!("{} {} ", self.left, color.paint("❯")).into_bytes()
    }
}

impl reedline::Prompt for StarshipPrompt {
    fn render_prompt_left(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.left)
    }
    fn render_prompt_right(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _mode: reedline::PromptEditMode) -> std::borrow::Cow<'_, str> {
        let color = if self.ok {
            nu_ansi_term::Color::Green
        } else {
            nu_ansi_term::Color::Red
        };
        std::borrow::Cow::Owned(format!(" {} ", color.paint("❯")))
    }
    fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned(nu_ansi_term::Color::DarkGray.paint("· ").to_string())
    }
    fn render_prompt_history_search_indicator(
        &self,
        history_search: reedline::PromptHistorySearch,
    ) -> std::borrow::Cow<'_, str> {
        let failing = matches!(
            history_search.status,
            reedline::PromptHistorySearchStatus::Failing
        );
        let prefix = if failing { "failing " } else { "" };
        std::borrow::Cow::Owned(format!("({prefix}reverse-search: {}) ", history_search.term))
    }
}

/// The dim `· ` prompt shown for PS2 continuation lines (heredocs, open quotes).
struct ContinuationPrompt;

impl reedline::Prompt for ContinuationPrompt {
    fn render_prompt_left(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed("")
    }
    fn render_prompt_right(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _mode: reedline::PromptEditMode) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned(nu_ansi_term::Color::DarkGray.paint("· ").to_string())
    }
    fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed("· ")
    }
    fn render_prompt_history_search_indicator(
        &self,
        _history_search: reedline::PromptHistorySearch,
    ) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed("")
    }
}

/// The current working directory, abbreviating `$HOME` to `~`.
fn cwd_display() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rel) = cwd.strip_prefix(std::path::PathBuf::from(home)) {
            return if rel.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rel.display())
            };
        }
    }
    cwd.display().to_string()
}

/// The current git branch (or short detached-HEAD sha), walking up from the cwd for a `.git` dir or
/// worktree pointer file. `None` when not inside a repository — the prompt then omits the segment.
fn git_branch() -> Option<String> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return head_branch(&dot_git);
        }
        if dot_git.is_file() {
            // Worktree: `.git` is a file `gitdir: <path>`.
            let content = std::fs::read_to_string(&dot_git).ok()?;
            let gitdir = content.strip_prefix("gitdir:")?.trim();
            return head_branch(std::path::Path::new(gitdir));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Read `<gitdir>/HEAD`: a `ref: refs/heads/<branch>` symref yields the branch; a raw sha (detached
/// HEAD) yields its 7-char short form.
fn head_branch(gitdir: &std::path::Path) -> Option<String> {
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let head = head.trim();
    head.strip_prefix("ref: refs/heads/")
        .map(std::string::ToString::to_string)
        .or_else(|| Some(head.chars().take(7).collect()))
}

/// Run an interactive `ask repl` session: an AI conversation with its OWN isolated transcript.
/// Prompts `[<model>]> `; each line is either a meta-command (`:model`/`:new-session`/`:exit`) or a
/// prompt sent to the model. On exit (`:exit`, `:quit`, or Ctrl-D), the session content is printed
/// to stdout so it enters the parent transcript once as rendered output (README). The parent shell's
/// transcript is untouched during the REPL — only the isolated one grows.
async fn run_repl(
    session: &mut Session,
    args: &crate::ai::ask::ReplArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let model = match session.repl_start(args) {
        Ok(m) => m,
        Err(msg) => {
            write_stdout(msg.as_bytes())?;
            return Ok(());
        }
    };
    write_stdout(format!("ask repl — model {model}. Type :exit to leave.\n").as_bytes())?;

    let mut line = String::new();
    loop {
        // The prompt shows the CURRENT model (it can change via :model).
        let prompt_model = session.repl_model().unwrap_or_default();
        write_stdout(format!("[{prompt_model}]> ").as_bytes())?;

        line.clear();
        if read_line_raw(&mut line)? == 0 {
            write_stdout(b"\n")?; // Ctrl-D: leave the REPL cleanly.
            break;
        }
        let input = String::from_utf8_lossy(trim_eol(line.as_bytes())).into_owned();
        if input.trim().is_empty() {
            continue;
        }

        // Meta-command? (`:model`, `:new-session`, `:exit`, …)
        if let Some((output, should_exit)) = session.repl_meta(&input) {
            write_stdout(output.as_bytes())?;
            if should_exit {
                break;
            }
            continue;
        }

        // Otherwise it's a prompt: one model turn against the isolated transcript.
        let reply = session.repl_turn(&input).await;
        write_stdout(reply.as_bytes())?;
        if !reply.ends_with('\n') {
            write_stdout(b"\n")?;
        }
    }

    // On exit, emit the session content to stdout (enters the parent transcript once, as rendered
    // output) and drop the REPL state.
    let rendered = session.repl_end();
    write_stdout(&rendered)?;
    Ok(())
}

/// Install the native provider shims into the session — the off-Golem mirror of the durable
/// injection the agent does in `clank-agent`'s `ensure_session`. Each seam has a `reqwest`-backed
/// native impl; the setters wrap the network transports in the `Logging*` decorators, so `http.log`
/// covers native `ask`/MCP/grease traffic too.
///
/// - **MCP HTTP transport** (also backs `grease` registry/install/update fetches — they share the
///   `mcp_http` field): always installed.
/// - **`ask` LLM provider**: always installed; it reports "not configured" at call time if no API key
///   is present (env `ANTHROPIC_API_KEY` or `~/.config/ask/ask.toml`), rather than being absent.
/// - **Golem cluster + agent invoker**: installed only when an external cluster config is found
///   (otherwise native keeps the honest "needs a cluster" error, unchanged).
fn inject_native_providers(session: &mut Session) {
    session.set_mcp_http(Box::new(crate::mcp::http_native::ReqwestMcpHttp::new()));
    session.set_ask_provider(Box::new(crate::ai::anthropic_native::ReqwestAnthropicProvider::new()));

    // Golem cluster + agent invoker: only when an external cluster config is present (README §161-163).
    // Without it, native keeps the honest "needs a cluster" error — Tier C is inert unless configured.
    if let Some(cfg) = crate::golem::config_native::load() {
        session.set_golem_cluster(Box::new(
            crate::golem::rest_native::NativeHttpGolemCluster::new(cfg.clone()),
        ));
        session.set_agent_invoker(Box::new(
            crate::golem::rest_native::NativeHttpAgentInvoker::new(cfg),
        ));
    }
}

/// Discard any bytes the terminal has buffered on stdin — a stray cursor-position reply that arrived
/// after a query timed out, or keystrokes typed while a long command ran — so they can't pollute the
/// next read (or leak as `^[[..R`). Enters raw mode so bytes that carry no newline (a cursor-position
/// reply) are readable, drains fd 0 non-blockingly, then restores both. Best-effort — any terminal or
/// fcntl error just skips the drain. crossterm's own `event::read` is deliberately NOT used: it
/// filters cursor-position reports out of the normal event stream, so it would leave that reply in
/// the buffer — the very thing we need to clear.
#[allow(unsafe_code)] // libc fcntl/read FFI over fd 0; see the SAFETY note inside.
fn drain_terminal_input() {
    use crossterm::terminal;
    use std::os::unix::io::AsRawFd;
    if terminal::enable_raw_mode().is_err() {
        return;
    }
    let fd = io::stdin().as_raw_fd();
    // SAFETY: `fcntl`/`read` are FFI over the standard fd 0. `F_GETFL`/`F_SETFL`/`read` return -1 on
    // error (guarded), never exhibit UB; the buffer is a live stack array with its true length.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            let mut buf = [0u8; 256];
            while libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) > 0 {}
            libc::fcntl(fd, libc::F_SETFL, flags);
        }
    }
    let _ = terminal::disable_raw_mode();
}

/// Read one line (through `\n`) from fd 0 with NO read-ahead, appending it (lossily decoded) to
/// `buf`; returns the number of bytes read (0 at EOF). Unlike `io::stdin().read_line`, this reads
/// fd 0 directly and never pre-pulls later input into the process-global `Stdin` `BufReader` — the
/// buffer a uutils consumer (`wc`/`cat`/`sort`/…) would otherwise drain mid-command via its own
/// `io::stdin().lock()`, stealing the REPL's already-queued next line and stranding the shell at
/// EOF (`ls | wc -l` then losing the following command; see `tools::coreutils::stage_piped_stdin`'s
/// note "moving the REPL off the shared buffered stdin"). One byte per `read` is fine: REPL lines
/// are short, and the shell reads exactly one line per prompt.
#[allow(unsafe_code)]
fn read_line_raw(buf: &mut String) -> io::Result<usize> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        // SAFETY: `read` over the standard fd 0 into a 1-byte stack buffer; returns -1 on error and
        // 0 at EOF (both handled), never exhibits UB.
        let r = unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if r == 0 {
            break; // EOF
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    buf.push_str(&String::from_utf8_lossy(&bytes));
    Ok(bytes.len())
}

/// Write all `bytes` to stdout and flush. Takes a fresh stdout handle each call so no lock is
/// held across the `.await` on command execution.
fn write_stdout(bytes: &[u8]) -> io::Result<()> {
    let mut out = io::stdout();
    out.write_all(bytes)?;
    out.flush()
}
