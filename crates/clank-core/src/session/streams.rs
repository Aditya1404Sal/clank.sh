//! Brush stdio wired to in-memory buffers.
//!
//! `Session` captures a command's stdout and stderr rather than letting them reach the real process
//! descriptors — on the durable agent there is no terminal to reach, and natively the captured bytes
//! are what feed the transcript. [`BufSink`] and [`BufSource`] are the `brush_core::openfiles::Stream`
//! adapters that make an ordinary `Vec<u8>` usable as one end of a Brush `OpenFile`, and [`finish`]
//! is the step that drains them into a [`LineResult`].

use super::{ExecutionControlFlow, Flow, LineResult};

/// Map a Brush result to line output, appending any shell error message to stderr.
pub(super) fn finish(
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
/// fd-returning trait methods are `#[cfg(unix)]`, so on wasm only `Read`/`Write`/`clone_box` are needed.
#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
pub(super) struct BufSink(pub(super) std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(target_arch = "wasm32")]
impl std::io::Read for BufSink {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(0)
    }
}

#[cfg(target_arch = "wasm32")]
impl std::io::Write for BufSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(data);
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

/// An in-memory source implementing `brush_core::openfiles::Stream` for wasm stdin injection —
/// `BufSink`'s read-side sibling. Hands a Session-layer pipeline head's bytes (curl/wget output)
/// to a Brush-run downstream as fd 0. Writes are no-ops, mirroring `BufSink`'s inert read side.
#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
pub(super) struct BufSource(pub(super) std::io::Cursor<Vec<u8>>);

#[cfg(target_arch = "wasm32")]
impl std::io::Read for BufSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut self.0, buf)
    }
}

#[cfg(target_arch = "wasm32")]
impl std::io::Write for BufSource {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
impl brush_core::openfiles::Stream for BufSource {
    fn clone_box(&self) -> Box<dyn brush_core::openfiles::Stream> {
        Box::new(self.clone())
    }
}
