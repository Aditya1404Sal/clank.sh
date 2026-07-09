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

use crate::{dispatch_context, Flow, Transcript};
use brush_builtins::{BuiltinSet, ShellBuilderExt};
use brush_core::openfiles::{OpenFile, OpenFiles};
use brush_core::{ExecutionControlFlow, Shell, SourceInfo};

use std::sync::{Arc, Mutex};

use crate::process::ProcessKind;
use crate::proctable::ProcessTable;
use crate::registry::CommandRegistry;

type BoxError = Box<dyn std::error::Error>;

/// The result of evaluating one shell line.
pub struct LineResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: u8,
    pub flow: Flow,
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
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn stderr(message: impl Into<Vec<u8>>) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: message.into(),
            exit_code: 1,
            flow: Flow::Continue,
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
        },
        Err(e) => {
            let exit_code: u8 = brush_core::ExecutionExitCode::from(&e).into();
            stderr.extend_from_slice(format!("clank: {e}\n").as_bytes());
            LineResult {
                stdout,
                stderr,
                exit_code,
                flow: Flow::Continue,
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

    /// `cat /proc/<pid>/status` reads the virtual process file through the real command path.
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
