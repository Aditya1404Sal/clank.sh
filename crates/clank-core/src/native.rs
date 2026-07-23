//! Native target: the shell as an ordinary executable over blocking `std::io`. All command
//! execution and output capture live in the shared [`crate::session::Session`]; this driver is
//! just the prompt/read/write loop.

use crate::session::Session;
use crate::{trim_eol, Flow, PROMPT};
use std::io::{self, Write};

/// Run the interactive read/eval/print loop until `exit` or end-of-input.
///
/// # Errors
///
/// Returns an error if the [`Session`] fails to initialize, or if reading a line from stdin or
/// writing to stdout fails.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut session = Session::new().await?;
    inject_native_providers(&mut session);
    let mut line = String::new();

    loop {
        write_stdout(PROMPT)?;

        line.clear();
        if io::stdin().read_line(&mut line)? == 0 {
            // EOF: leave the cursor on a fresh line after the dangling prompt.
            write_stdout(b"\n")?;
            break;
        }

        let mut line_str = String::from_utf8_lossy(trim_eol(line.as_bytes())).into_owned();

        // `ask repl` is a native-terminal feature: run the interactive REPL loop inline (the native
        // driver owns the terminal, so it can block on human input between turns — the durable agent
        // cannot, and returns an honest message from `eval_line`). See `Session::repl_*`.
        if let Some(args) = crate::ai::ask::classify_repl(&line_str) {
            run_repl(&mut session, &args).await?;
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
            if io::stdin().read_line(&mut line)? == 0 {
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
                let answer = if io::stdin().read_line(&mut line)? == 0 {
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
        if io::stdin().read_line(&mut line)? == 0 {
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
    // The multi-provider dispatcher: routes `ask` by the model's `provider/` prefix (anthropic via
    // the Messages API; openai/grok/openrouter/ollama via the OpenAI Chat Completions API; bedrock is
    // an honest agent-only error). Mirrors clank-agent's durable golem-ai-llm dispatcher.
    session.set_ask_provider(Box::new(crate::ai::llm_native::NativeLlmProvider::new()));

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

/// Write all `bytes` to stdout and flush. Takes a fresh stdout handle each call so no lock is
/// held across the `.await` on command execution.
fn write_stdout(bytes: &[u8]) -> io::Result<()> {
    let mut out = io::stdout();
    out.write_all(bytes)?;
    out.flush()
}
