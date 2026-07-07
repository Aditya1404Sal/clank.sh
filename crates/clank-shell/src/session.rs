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

type BoxError = Box<dyn std::error::Error>;

/// A live shell session: the Brush interpreter plus the session transcript.
pub struct Session {
    shell: Shell,
    transcript: Transcript,
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
                source: SourceInfo::default(),
            })
        }
    }

    /// Run one input line: record it, serve the clank-specific `context` builtin, otherwise
    /// execute it through Brush. Returns the bytes to write to the terminal and the control flow.
    pub async fn run_line(&mut self, line: &str) -> (Vec<u8>, Flow) {
        self.transcript.record_command(line);

        // `context show` output is intentionally not recorded back into the transcript.
        if let Some(bytes) = dispatch_context(&mut self.transcript, line) {
            return (bytes, Flow::Continue);
        }

        let (output, exit) = self.execute(line).await;
        self.transcript.record_output(&output);
        (output, if exit { Flow::Exit } else { Flow::Continue })
    }

    /// Native execution: capture Brush's stdout+stderr into an anonymous temp file.
    #[cfg(not(target_arch = "wasm32"))]
    async fn execute(&mut self, line: &str) -> (Vec<u8>, bool) {
        use std::io::{Read, Seek, SeekFrom};

        let capture = match tempfile::tempfile() {
            Ok(f) => f,
            Err(e) => return (format!("clank: {e}\n").into_bytes(), false),
        };
        let (out_fd, err_fd) = match (capture.try_clone(), capture.try_clone()) {
            (Ok(o), Ok(e)) => (o, e),
            _ => return (b"clank: failed to set up output capture\n".to_vec(), false),
        };

        let mut params = self.shell.default_exec_params();
        params.set_fd(OpenFiles::STDOUT_FD, OpenFile::File(out_fd));
        params.set_fd(OpenFiles::STDERR_FD, OpenFile::File(err_fd));

        let result = self
            .shell
            .run_string(line.to_string(), &self.source, &params)
            .await;
        drop(params);

        let mut output = Vec::new();
        let mut reader = capture;
        let _ = reader
            .seek(SeekFrom::Start(0))
            .and_then(|_| reader.read_to_end(&mut output));

        finish(result, output)
    }

    /// Wasm execution: capture Brush's stdout+stderr into an in-memory buffer and drive the
    /// async on the owned current-thread runtime.
    #[cfg(target_arch = "wasm32")]
    async fn execute(&mut self, line: &str) -> (Vec<u8>, bool) {
        use std::sync::{Arc, Mutex};

        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let mut params = self.shell.default_exec_params();
        params.set_fd(
            OpenFiles::STDOUT_FD,
            OpenFile::Stream(Box::new(BufSink(buf.clone()))),
        );
        params.set_fd(
            OpenFiles::STDERR_FD,
            OpenFile::Stream(Box::new(BufSink(buf.clone()))),
        );

        let fut = self
            .shell
            .run_string(line.to_string(), &self.source, &params);
        let result = self.rt.block_on(fut);
        drop(params);

        let output = std::mem::take(&mut *buf.lock().unwrap());
        finish(result, output)
    }
}

async fn build_shell() -> Result<Shell, brush_core::Error> {
    Shell::builder()
        .default_builtins(BuiltinSet::BashMode)
        .builtins(crate::coreutils::builtins())
        .builtins(crate::texttools::builtins())
        .build()
        .await
}

/// Map a Brush result to (output, should-exit), appending any error message to the output.
fn finish(
    result: Result<brush_core::ExecutionResult, brush_core::Error>,
    mut output: Vec<u8>,
) -> (Vec<u8>, bool) {
    match result {
        Ok(r) => (
            output,
            matches!(r.next_control_flow, ExecutionControlFlow::ExitShell),
        ),
        Err(e) => {
            output.extend_from_slice(format!("clank: {e}\n").as_bytes());
            (output, false)
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
