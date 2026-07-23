//! Wasm target: the shell as a `wasi:cli/run` component (WASI 0.3, async).
//!
//! stdout is driven by a concurrent writer future (via `futures::join!`) so the loop can flush
//! the prompt and command output while it blocks on the next stdin read. All command execution
//! lives in the shared [`crate::session::Session`], which drives Brush on an owned current-thread
//! tokio runtime (wasip2 has no threads). Builtins, shell language, and pipelines/`$(...)` all work
//! (the Brush fork's Wall-C inline-sequential path); spawning real external processes is genuinely
//! unavailable in the sandbox.
//!
//! (We keep `futures::join!` rather than `wit_bindgen::spawn` for the writer: `spawn` is
//! fire-and-forget with no join handle, and `join!` guarantees the stream is fully drained
//! before `run` returns.)

use crate::session::Session;
use crate::{trim_eol, Flow, PROMPT};
use wit_bindgen::{StreamReader, StreamResult, StreamWriter};

wasip3::cli::command::export!(Component);

struct Component;

/// Bytes requested per stdin read. Host stdin arrives in host-sized chunks; this is only the
/// buffer we offer each read, not a line-length limit — partial lines are reassembled.
const READ_CHUNK: usize = 4096;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        let (mut out_tx, out_rx) = wasip3::wit_stream::new();
        let (mut stdin, _stdin_result) = wasip3::cli::stdin::read_via_stream();

        futures::join!(
            async {
                let _ = wasip3::cli::stdout::write_via_stream(out_rx).await;
            },
            async {
                match Session::new().await {
                    Ok(mut session) => repl(&mut out_tx, &mut stdin, &mut session).await,
                    Err(e) => {
                        let msg = format!("clank: failed to start shell: {e}\n");
                        write_bytes(&mut out_tx, msg.as_bytes()).await;
                    }
                }
                drop(out_tx);
            }
        );

        Ok(())
    }
}

/// The read/eval/print loop. Reassembles newline-delimited lines from stdin chunks and runs each
/// through the session, writing a prompt before every read. Returns on `exit` or end-of-input.
async fn repl(out: &mut StreamWriter<u8>, stdin: &mut StreamReader<u8>, session: &mut Session) {
    let mut pending: Vec<u8> = Vec::new();

    write_bytes(out, PROMPT).await;

    loop {
        let (status, chunk) = stdin.read(Vec::with_capacity(READ_CHUNK)).await;

        if !chunk.is_empty() {
            pending.extend_from_slice(&chunk);

            while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = pending.drain(..=nl).collect();
                if let Flow::Exit = handle_line(out, session, &raw).await {
                    return;
                }
                write_bytes(out, PROMPT).await;
            }
        }

        if matches!(status, StreamResult::Dropped | StreamResult::Cancelled) {
            break;
        }
    }

    // A final line without a trailing newline (e.g. EOF mid-line) still runs.
    if !pending.is_empty() {
        if let Flow::Exit = handle_line(out, session, &pending).await {
            return;
        }
    }

    // Leave the cursor on a fresh line after the dangling prompt.
    write_bytes(out, b"\n").await;
}

/// Run one line through the session and write its output.
async fn handle_line(out: &mut StreamWriter<u8>, session: &mut Session, raw: &[u8]) -> Flow {
    let line = String::from_utf8_lossy(trim_eol(raw)).into_owned();
    let (output, flow) = session.run_line(&line).await;
    write_bytes(out, &output).await;
    flow
}

/// Write all `bytes` to the stdout stream. The writer accepts the whole buffer; any returned
/// remainder (unexpected here) is dropped.
async fn write_bytes(out: &mut StreamWriter<u8>, bytes: &[u8]) {
    let _ = out.write_all(bytes.to_vec()).await;
}
