//! Shared shell session: owns the Brush interpreter and the [`Transcript`], and runs one line
//! at a time. Command dispatch is unified across targets; only the async runtime and the output
//! capture differ:
//!
//! - **native** — Brush runs on the ambient multi-thread tokio runtime (from `main`), output is
//!   captured per command into an anonymous temp file (`OpenFile::File`, which also feeds real
//!   external programs).
//! - **wasm** — Brush is driven on an owned **current-thread** tokio runtime (wasip2 has no
//!   threads), output is captured into an in-memory buffer via `OpenFile::Stream`. External
//!   process spawning is unavailable in the sandbox and errors cleanly.
//!
//! Brush hard-depends on tokio internally (`tokio::spawn` for pipelines, `spawn_blocking` for
//! owned-shell builtins), so a tokio runtime is required to run it at all — `wit_bindgen::spawn`
//! cannot substitute for that. On wasm, pipelines/subshells that reach `spawn_blocking` are a
//! known limitation (no threads); simple builtins and shell language work.

use crate::{dispatch_context, promptuser, Flow, Transcript};
use brush_builtins::{BuiltinSet, ShellBuilderExt};
use brush_core::openfiles::{OpenFile, OpenFiles};
use brush_core::{ExecutionControlFlow, Shell, SourceInfo};

use std::sync::{Arc, Mutex};

use crate::process::ProcessKind;
use crate::promptuser::{AnswerInput, PendingPrompt, Resolution};
use crate::proctable::ProcessTable;
use crate::registry::CommandRegistry;

type BoxError = Box<dyn std::error::Error>;

/// The result of evaluating one shell line.
pub struct LineResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: u8,
    pub flow: Flow,
    /// Set when this line surfaced a `prompt-user` question the shell is now awaiting a response
    /// to. The caller must collect a human answer and deliver it via [`Session::answer_prompt`]
    /// (the shell does not block). `None` for every ordinary line.
    pub pending_prompt: Option<PendingPrompt>,
}

impl LineResult {
    /// Bytes as a terminal would display them through the legacy `run_line` API.
    pub fn terminal_output(&self) -> Vec<u8> {
        let mut output = self.stdout.clone();
        output.extend_from_slice(&self.stderr);
        output
    }

    fn continue_with_stdout(stdout: Vec<u8>) -> Self {
        Self {
            stdout,
            stderr: Vec::new(),
            exit_code: 0,
            flow: Flow::Continue,
            pending_prompt: None,
        }
    }

    fn stderr(message: impl Into<Vec<u8>>) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: message.into(),
            exit_code: 1,
            flow: Flow::Continue,
            pending_prompt: None,
        }
    }
}

/// A live shell session: the Brush interpreter plus the session transcript and the command
/// registry.
pub struct Session {
    shell: Shell,
    transcript: Transcript,
    /// The clank-owned inventory of command manifests (sits beside `transcript` as a shell-owned
    /// state object). Drives command metadata surfaces; not yet consulted on the execution path.
    registry: CommandRegistry,
    /// The process table: one row per executed line. Shared behind `Arc<Mutex>` so `run_line` can
    /// install it into the process-global slot the `ps` builtin reads (Brush builtins can't reach
    /// `Session` directly).
    proc_table: Arc<Mutex<ProcessTable>>,
    /// A `prompt-user` question the shell has surfaced and is awaiting a response to, plus the
    /// process-table PID of the paused (`P`) row. Durable `Session` state (persisted on the Golem
    /// oplog), so a pending prompt survives across invocations — the caller answers it via
    /// [`Session::answer_prompt`]. `None` when no prompt is outstanding.
    pending: Option<(PendingPrompt, Option<u32>)>,
    source: SourceInfo,
    #[cfg(target_arch = "wasm32")]
    rt: tokio::runtime::Runtime,
}

impl Session {
    /// Build a non-interactive shell with the full bash-compatible builtin set.
    pub async fn new() -> Result<Self, BoxError> {
        #[cfg(target_arch = "wasm32")]
        {
            // wasip2 has no threads: a current-thread runtime drives Brush's async.
            let rt = tokio::runtime::Builder::new_current_thread().build()?;
            let shell = rt.block_on(build_shell())?;
            Ok(Self {
                shell,
                transcript: Transcript::new(),
                registry: crate::registry::build(),
                proc_table: Arc::new(Mutex::new(ProcessTable::new())),
                pending: None,
                source: SourceInfo::default(),
                rt,
            })
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let shell = build_shell().await?;
            Ok(Self {
                shell,
                transcript: Transcript::new(),
                registry: crate::registry::build(),
                proc_table: Arc::new(Mutex::new(ProcessTable::new())),
                pending: None,
                source: SourceInfo::default(),
            })
        }
    }

    /// The command registry — clank's inventory of command manifests.
    pub fn registry(&self) -> &CommandRegistry {
        &self.registry
    }

    /// Evaluate one input line: record it, serve the clank-specific `context` builtin, otherwise
    /// execute it through Brush.
    pub async fn eval_line(&mut self, line: &str) -> LineResult {
        // A prompt is already outstanding: the caller must answer it (via `answer_prompt`), not run
        // a new command. The shell never blocks, so it's the caller's job to notice `pending_prompt`
        // and respond. Reject the command with a clear message rather than silently interleaving.
        if self.pending.is_some() {
            return LineResult::stderr(
                "clank: a prompt-user question is awaiting a response; answer it first\n",
            );
        }

        self.transcript.record_command(line);

        // Install this session's process table as the active one for the duration of the line, so
        // the `ps` builtin (a Brush builtin, which can't reach `Session` directly) can read it.
        // The guard clears the slot on drop.
        let _install = crate::proctable::install(self.proc_table.clone());

        // Record this line as a process (one PID per executed line). Blank lines get no row, matching
        // the "empty line re-prompts" behavior. `context` lines DO get a row — they're real typed
        // work, and `ps` omitting them would mislead. The row is born `R` and marked `Z` only after
        // execution returns, so a `ps` in this same line sees its own row as `R`, like real Unix.
        let pid = {
            let argv: Vec<String> = line.split_whitespace().map(String::from).collect();
            if argv.is_empty() {
                None
            } else {
                let kind = classify(line);
                Some(self.proc_table.lock().unwrap().spawn(kind, argv))
            }
        };

        // `context show` output is intentionally not recorded back into the transcript.
        if let Some(bytes) = dispatch_context(&mut self.transcript, line) {
            if let Some(pid) = pid {
                self.proc_table.lock().unwrap().complete(pid);
            }
            return LineResult::continue_with_stdout(bytes);
        }

        // `prompt-user` is intercepted before Brush dispatch (like `context` above). It does NOT
        // block: it records a durable pending prompt, leaves the row in `P`, and returns
        // immediately — surfacing the question to the caller, who answers via `answer_prompt`. See
        // the `promptuser` module docs.
        if promptuser::is_prompt_user(line) {
            return self.surface_prompt(line, pid);
        }

        let result = self.execute(line).await;
        if let Some(pid) = pid {
            self.proc_table.lock().unwrap().complete(pid);
        }
        self.transcript.record_output(&result.terminal_output());
        result
    }

    /// Run one input line for terminal-style callers. This keeps the original API used by the REPLs.
    pub async fn run_line(&mut self, line: &str) -> (Vec<u8>, Flow) {
        let result = self.eval_line(line).await;
        (result.terminal_output(), result.flow)
    }

    /// Whether a `prompt-user` question is currently awaiting a response.
    pub fn has_pending_prompt(&self) -> bool {
        self.pending.is_some()
    }

    /// Handle a `prompt-user` line: parse it, record the pending prompt (durable state), leave the
    /// process row paused (`P`), and return immediately with `pending_prompt` set. The shell does
    /// not block — the caller collects a human answer and delivers it via [`answer_prompt`].
    fn surface_prompt(&mut self, line: &str, pid: Option<u32>) -> LineResult {
        let args = match promptuser::parse(line) {
            Ok(args) => args,
            Err(e) => {
                if let Some(pid) = pid {
                    self.proc_table.lock().unwrap().complete(pid);
                }
                let result = LineResult::stderr(format!("{e}\n"));
                self.transcript.record_output(&result.terminal_output());
                return result;
            }
        };
        // Piping into `prompt-user` (`X | prompt-user ...`) is a later increment; for now stdin is
        // never wired, so no markdown is prepended.
        let pending = args.into_pending(None);

        // Leave the row paused (`P`) until the answer arrives; record the question as this line's
        // output (unless `--secret`, though the question itself isn't the secret — the response is).
        if let Some(pid) = pid {
            self.proc_table.lock().unwrap().pause(pid);
        }
        let mut stdout = pending.question.clone().into_bytes();
        stdout.push(b'\n');
        self.transcript.record_output(&stdout);

        self.pending = Some((pending.clone(), pid));
        LineResult {
            stdout,
            stderr: Vec::new(),
            exit_code: 0,
            flow: Flow::Continue,
            pending_prompt: Some(pending),
        }
    }

    /// Deliver a response to the outstanding `prompt-user` question. `response` is `Some(text)` for
    /// an answer or `None` for an abort (README: exit `130`). Resumes and reaps the paused row.
    ///
    /// - A valid answer → the response on stdout, exit `0` (recorded in the transcript unless the
    ///   prompt was `--secret`).
    /// - Abort → no stdout, exit `130`.
    /// - A response outside the prompt's `--choices` → an error on stderr, exit `1`, and the prompt
    ///   **stays pending** so the caller can re-ask.
    /// - No prompt outstanding → an error, exit `1`.
    pub fn answer_prompt(&mut self, response: Option<String>) -> LineResult {
        let Some((pending, pid)) = self.pending.clone() else {
            return LineResult::stderr("clank: no prompt-user question is awaiting a response\n");
        };

        let answer = match response {
            Some(text) => AnswerInput::Response(text),
            None => AnswerInput::Abort,
        };

        match promptuser::resolve(&pending, answer) {
            Resolution::InvalidChoice { message } => {
                // Prompt stays pending — re-ask. Don't touch the row (still `P`).
                LineResult::stderr(message)
            }
            resolution => {
                // Resolved (answered or aborted): resume and reap the paused row, clear pending.
                if let Some(pid) = pid {
                    let mut table = self.proc_table.lock().unwrap();
                    table.resume(pid);
                    table.complete(pid);
                }
                self.pending = None;

                let (stdout, exit_code, secret) = match resolution {
                    Resolution::Answered { stdout, secret } => (stdout, 0, secret),
                    Resolution::Aborted => (Vec::new(), 130, false),
                    Resolution::InvalidChoice { .. } => unreachable!("handled above"),
                };
                // `--secret` responses are never entered into the transcript (README).
                if !secret {
                    self.transcript.record_output(&stdout);
                }
                LineResult {
                    stdout,
                    stderr: Vec::new(),
                    exit_code,
                    flow: Flow::Continue,
                    pending_prompt: None,
                }
            }
        }
    }

    /// Native execution: capture Brush's stdout and stderr into anonymous temp files.
    #[cfg(not(target_arch = "wasm32"))]
    async fn execute(&mut self, line: &str) -> LineResult {
        use std::io::{Read, Seek, SeekFrom};

        let stdout_capture = match tempfile::tempfile() {
            Ok(f) => f,
            Err(e) => return LineResult::stderr(format!("clank: {e}\n")),
        };
        let stderr_capture = match tempfile::tempfile() {
            Ok(f) => f,
            Err(e) => return LineResult::stderr(format!("clank: {e}\n")),
        };
        let (out_fd, err_fd) = match (stdout_capture.try_clone(), stderr_capture.try_clone()) {
            (Ok(o), Ok(e)) => (o, e),
            _ => return LineResult::stderr(b"clank: failed to set up output capture\n".to_vec()),
        };

        let mut params = self.shell.default_exec_params();
        params.set_fd(OpenFiles::STDOUT_FD, OpenFile::File(out_fd.into()));
        params.set_fd(OpenFiles::STDERR_FD, OpenFile::File(err_fd.into()));

        let result = self
            .shell
            .run_string(line.to_string(), &self.source, &params)
            .await;
        drop(params);

        let mut stdout = Vec::new();
        let mut stdout_reader = stdout_capture;
        let _ = stdout_reader
            .seek(SeekFrom::Start(0))
            .and_then(|_| stdout_reader.read_to_end(&mut stdout));

        let mut stderr = Vec::new();
        let mut stderr_reader = stderr_capture;
        let _ = stderr_reader
            .seek(SeekFrom::Start(0))
            .and_then(|_| stderr_reader.read_to_end(&mut stderr));

        finish(result, stdout, stderr)
    }

    /// Wasm execution: capture Brush's stdout and stderr into in-memory buffers and drive the
    /// async on the owned current-thread runtime.
    #[cfg(target_arch = "wasm32")]
    async fn execute(&mut self, line: &str) -> LineResult {
        let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let mut params = self.shell.default_exec_params();
        params.set_fd(
            OpenFiles::STDOUT_FD,
            OpenFile::Stream(Box::new(BufSink(stdout_buf.clone()))),
        );
        params.set_fd(
            OpenFiles::STDERR_FD,
            OpenFile::Stream(Box::new(BufSink(stderr_buf.clone()))),
        );

        let fut = self
            .shell
            .run_string(line.to_string(), &self.source, &params);
        let result = self.rt.block_on(fut);
        drop(params);

        let stdout = std::mem::take(&mut *stdout_buf.lock().unwrap());
        let stderr = std::mem::take(&mut *stderr_buf.lock().unwrap());
        finish(result, stdout, stderr)
    }
}

/// Classify a command line into a process kind for the process table. Everything is a `Builtin`
/// this increment; this is where Script/Prompt/AgentInvocation classification lands once those
/// command kinds exist (they'll be resolved from `$PATH` / the registry).
fn classify(_line: &str) -> ProcessKind {
    ProcessKind::Builtin
}

async fn build_shell() -> Result<Shell, brush_core::Error> {
    // NB: clank's builtins are registered here AND their manifests in `registry::build()`; the two
    // must stay in lockstep (the registry drift-guard test enforces it). Adding a builtin via
    // `Shell::register_builtin` directly would bypass the manifest — don't.
    Shell::builder()
        .default_builtins(BuiltinSet::BashMode)
        .builtins(crate::coreutils::builtins())
        .builtins(crate::texttools::builtins())
        .builtins(crate::ps::builtins())
        .build()
        .await
}

/// Map a Brush result to line output, appending any shell error message to stderr.
fn finish(
    result: Result<brush_core::ExecutionResult, brush_core::Error>,
    stdout: Vec<u8>,
    mut stderr: Vec<u8>,
) -> LineResult {
    match result {
        Ok(r) => LineResult {
            stdout,
            stderr,
            exit_code: r.exit_code.into(),
            flow: if matches!(r.next_control_flow, ExecutionControlFlow::ExitShell) {
                Flow::Exit
            } else {
                Flow::Continue
            },
            pending_prompt: None,
        },
        Err(e) => {
            let exit_code: u8 = brush_core::ExecutionExitCode::from(&e).into();
            stderr.extend_from_slice(format!("clank: {e}\n").as_bytes());
            LineResult {
                stdout,
                stderr,
                exit_code,
                flow: Flow::Continue,
                pending_prompt: None,
            }
        }
    }
}

/// An in-memory sink implementing `brush_core::openfiles::Stream` for wasm output capture. The
/// fd-returning trait methods are `#[cfg(unix)]`, so on wasm only Read/Write/clone_box are needed.
#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
struct BufSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(target_arch = "wasm32")]
impl std::io::Read for BufSink {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(0)
    }
}

#[cfg(target_arch = "wasm32")]
impl std::io::Write for BufSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
impl brush_core::openfiles::Stream for BufSink {
    fn clone_box(&self) -> Box<dyn brush_core::openfiles::Stream> {
        Box::new(self.clone())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// Drive a closure on a fresh current-thread runtime (mirrors how `Session` is used natively).
    fn on_rt<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// Structured evaluation exposes stdout, stderr, and exit code for agent callers.
    #[test]
    fn eval_line_reports_streams_and_exit_status() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();

            let result = session.eval_line("echo hi").await;
            assert_eq!(String::from_utf8(result.stdout).unwrap(), "hi\n");
            assert!(result.stderr.is_empty());
            assert_eq!(result.exit_code, 0);
            assert_eq!(result.flow, Flow::Continue);

            let result = session.eval_line("false").await;
            assert!(result.stdout.is_empty());
            assert!(result.stderr.is_empty());
            assert_eq!(result.exit_code, 1);
            assert_eq!(result.flow, Flow::Continue);
        });
    }

    /// End-to-end through the public API: a completed command shows `Z` in `ps`, and `ps` sees its
    /// own row as `R` (spawned before execution, completed only after) — like real Unix.
    #[test]
    fn ps_reflects_completed_and_running_rows() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let (_out, _flow) = session.run_line("echo hi").await;
            let (ps_out, _flow) = session.run_line("ps").await;
            let ps_out = String::from_utf8(ps_out).unwrap();

            // The prior `echo hi` line completed → its row is Z.
            let echo_row = ps_out
                .lines()
                .find(|l| l.contains("echo hi"))
                .expect("ps should list the completed `echo hi` line");
            assert!(
                echo_row.contains('Z'),
                "completed line should be Z, got: {echo_row}"
            );

            // The `ps` invocation itself is still running while it renders → its row is R.
            let ps_row = ps_out
                .lines()
                .find(|l| l.trim_end().ends_with("ps"))
                .expect("ps should list itself");
            assert!(
                ps_row.contains('R'),
                "ps's own row should be R, got: {ps_row}"
            );

            // The synthetic root is present.
            assert!(ps_out.contains("clank"));
        });
    }

    /// `prompt-user` is intercepted before Brush dispatch: an invocation Brush would reject
    /// differently (unknown command) instead surfaces `promptuser::parse`'s own error, proving
    /// the interception — not Brush — handled the line. Also proves the line still shows `Z` in
    /// `ps` (the process-table row completes normally for intercepted lines, same as `context`).
    #[test]
    fn prompt_user_is_intercepted_before_brush_dispatch() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let (out, flow) = session.run_line("prompt-user --confirm").await;
            let out = String::from_utf8(out).unwrap();
            assert!(
                out.contains("missing question"),
                "expected promptuser's own parse error, got: {out}"
            );
            assert_eq!(flow, Flow::Continue);

            let (ps_out, _) = session.run_line("ps").await;
            let ps_out = String::from_utf8(ps_out).unwrap();
            let row = ps_out
                .lines()
                .find(|l| l.contains("prompt-user"))
                .expect("ps should list the prompt-user line");
            assert!(row.contains('Z'), "completed line should be Z, got: {row}");
        });
    }

    /// A non-`--secret` `prompt-user` error is recorded in the transcript like any other command
    /// (only `--secret` *responses* are redacted, per the README — this line never reached the
    /// point of collecting a response).
    #[test]
    fn prompt_user_error_is_recorded_in_transcript() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.run_line("prompt-user --bogus").await;
            let (transcript, _) = session.run_line("context show").await;
            let transcript = String::from_utf8(transcript).unwrap();
            assert!(transcript.contains("prompt-user --bogus"));
            assert!(transcript.contains("unknown flag"));
        });
    }

    /// The registry carries a manifest for `prompt-user` even though it's never registered as a
    /// Brush `SimpleCommand` (it's intercepted before dispatch) — `type`/tool-surface consumers
    /// should still see it.
    #[test]
    fn prompt_user_has_a_registry_manifest() {
        on_rt(async {
            let session = Session::new().await.unwrap();
            let manifest = session
                .registry()
                .get("prompt-user")
                .expect("prompt-user should have a manifest");
            assert_eq!(
                manifest.execution_scope,
                crate::manifest::ExecutionScope::ShellInternal
            );
        });
    }

    /// The full two-step path: `prompt-user` surfaces the question (returns immediately with
    /// `pending_prompt` set, does NOT hang), then `answer_prompt` delivers the response to stdout
    /// with exit 0, recorded in the transcript (not `--secret`), and clears the pending state.
    #[test]
    fn prompt_user_surfaces_then_answer_resolves() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();

            // Step 1: surface. The question comes back and the prompt is pending — no hang.
            let surfaced = session.eval_line(r#"prompt-user "Which environment?""#).await;
            assert_eq!(surfaced.exit_code, 0);
            let pending = surfaced.pending_prompt.expect("should surface a pending prompt");
            assert_eq!(pending.question, "Which environment?");
            assert!(session.has_pending_prompt());

            // Step 2: answer. The response flows to stdout, pending clears.
            let answered = session.answer_prompt(Some("production".to_string()));
            assert_eq!(String::from_utf8(answered.stdout).unwrap(), "production\n");
            assert_eq!(answered.exit_code, 0);
            assert!(answered.pending_prompt.is_none());
            assert!(!session.has_pending_prompt());

            let (transcript, _) = session.run_line("context show").await;
            let transcript = String::from_utf8(transcript).unwrap();
            assert!(transcript.contains("production"), "got: {transcript}");
        });
    }

    /// An aborted answer (`None`) exits 130 with no stdout (README) and clears the pending prompt.
    #[test]
    fn prompt_user_abort_exits_130() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.eval_line(r#"prompt-user "q?""#).await;
            let result = session.answer_prompt(None);
            assert!(result.stdout.is_empty());
            assert_eq!(result.exit_code, 130);
            assert!(!session.has_pending_prompt());
        });
    }

    /// An answer outside the prompt's `--choices` errors and leaves the prompt pending to re-ask.
    #[test]
    fn prompt_user_invalid_choice_keeps_prompt_pending() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session
                .eval_line(r#"prompt-user "Approve?" --confirm"#)
                .await;
            let bad = session.answer_prompt(Some("maybe".to_string()));
            assert_eq!(bad.exit_code, 1);
            assert!(session.has_pending_prompt(), "prompt should stay pending");

            // A valid choice then resolves it.
            let ok = session.answer_prompt(Some("yes".to_string()));
            assert_eq!(String::from_utf8(ok.stdout).unwrap(), "yes\n");
            assert!(!session.has_pending_prompt());
        });
    }

    /// A `--secret` response is never entered into the transcript (README), though the command
    /// line itself is still recorded.
    #[test]
    fn prompt_user_secret_response_is_redacted_from_transcript() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session
                .eval_line(r#"prompt-user "Enter the API key:" --secret"#)
                .await;
            let result = session.answer_prompt(Some("s3cr3t-key".to_string()));
            // The caller still gets the response on stdout...
            assert_eq!(String::from_utf8(result.stdout).unwrap(), "s3cr3t-key\n");

            // ...but it must not appear in the transcript.
            let (transcript, _) = session.run_line("context show").await;
            let transcript = String::from_utf8(transcript).unwrap();
            assert!(
                !transcript.contains("s3cr3t-key"),
                "secret response leaked into transcript: {transcript}"
            );
            // The command line itself is still recorded.
            assert!(transcript.contains("prompt-user"));
        });
    }

    /// While a prompt is pending, an ordinary command is rejected — the caller must answer first.
    #[test]
    fn command_while_prompt_pending_is_rejected() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.eval_line(r#"prompt-user "q?""#).await;
            let blocked = session.eval_line("echo hi").await;
            assert_ne!(blocked.exit_code, 0);
            assert!(
                String::from_utf8(blocked.terminal_output())
                    .unwrap()
                    .contains("awaiting a response"),
                "expected a 'answer the prompt first' error"
            );
            // The prompt is still pending and still answerable.
            assert!(session.has_pending_prompt());
        });
    }

    /// `answer_prompt` with no prompt outstanding is a clean error, not a panic.
    #[test]
    fn answer_prompt_with_no_pending_is_an_error() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let result = session.answer_prompt(Some("x".to_string()));
            assert_ne!(result.exit_code, 0);
        });
    }

    #[test]
    fn cat_reads_virtual_proc_status() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let (out, _) = session.run_line("cat /proc/1/status").await;
            let out = String::from_utf8(out).unwrap();
            assert!(out.contains("Pid:"), "got: {out}");
            assert!(out.contains("State:"));
            assert!(out.contains("clank"));
        });
    }

    /// `grep` output is captured via `context.stdout()` (the `run_tool` path) — this is
    /// parallel-safe (no process-global fd swap) and verifies the wasm output-capture fix on the
    /// native side too.
    #[test]
    fn grep_captures_output() {
        on_rt(async {
            let dir = std::env::temp_dir();
            let path = dir.join(format!("clank_grep_test_{}", std::process::id()));
            std::fs::write(&path, b"alpha\nbeta\ngamma\n").unwrap();
            let mut session = Session::new().await.unwrap();
            let (out, _) = session
                .run_line(&format!("grep beta {}", path.display()))
                .await;
            let _ = std::fs::remove_file(&path);
            let out = String::from_utf8(out).unwrap();
            assert!(
                out.contains("beta"),
                "grep output should contain the match, got: {out}"
            );
            assert!(
                !out.contains("alpha"),
                "grep should not emit non-matching lines: {out}"
            );
        });
    }

    /// `grep` over a virtual `/proc` file works and its output is captured.
    #[test]
    fn grep_matches_virtual_proc_file() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let (out, _) = session.run_line("grep State /proc/1/status").await;
            let out = String::from_utf8(out).unwrap();
            assert!(out.contains("State"), "got: {out}");
        });
    }

    /// A real-file `cat` still works after `cat` became a `/proc`-aware shim (delegation intact).
    #[test]
    fn cat_still_reads_real_files() {
        on_rt(async {
            let dir = std::env::temp_dir();
            let path = dir.join(format!("clank_cat_test_{}", std::process::id()));
            std::fs::write(&path, b"real-file-contents\n").unwrap();
            let mut session = Session::new().await.unwrap();
            let (out, _) = session.run_line(&format!("cat {}", path.display())).await;
            let _ = std::fs::remove_file(&path);
            let out = String::from_utf8(out).unwrap();
            assert!(out.contains("real-file-contents"), "got: {out}");
        });
    }

    /// PIDs persist and keep climbing across `run_line` calls (the durable-agent property, tested
    /// locally): the second command gets a higher PID than the first.
    #[test]
    fn pids_are_monotonic_across_lines() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.run_line("echo one").await;
            session.run_line("echo two").await;
            let (ps_out, _) = session.run_line("ps").await;
            let ps_out = String::from_utf8(ps_out).unwrap();

            let pid_of = |needle: &str| -> u32 {
                ps_out
                    .lines()
                    .find(|l| l.contains(needle))
                    .and_then(|l| l.split_whitespace().next())
                    .and_then(|p| p.parse().ok())
                    .unwrap_or_else(|| panic!("no pid for {needle} in:\n{ps_out}"))
            };
            assert!(pid_of("echo two") > pid_of("echo one"));
        });
    }
}
