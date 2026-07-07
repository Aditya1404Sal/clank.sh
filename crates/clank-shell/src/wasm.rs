//! Wasm target: the shell as a `wasi:cli/run` component (WASI 0.3, async).
//!
//! stdout is driven by a concurrent writer future so the loop can flush the prompt and
//! command output while it blocks on the next stdin read. Command evaluation is delegated
//! to the pure, shared [`crate::eval`].

use crate::{eval, trim_eol, Flow, PROMPT};
use wit_bindgen::{StreamReader, StreamResult, StreamWriter};

wasip3::cli::command::export!(Component);

struct Component;

/// Bytes requested per stdin read. Host stdin arrives in host-sized chunks; this is only
/// the buffer we offer each read, not a line-length limit — partial lines are reassembled
/// across reads.
const READ_CHUNK: usize = 4096;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        // stdout is produced by writing byte chunks into `out_tx`; the receiving half is
        // drained to real stdout by `write_via_stream`, run concurrently so writes flush
        // while the loop awaits stdin.
        let (mut out_tx, out_rx) = wasip3::wit_stream::new();

        // stdin as an async byte stream. The companion future signals the final read
        // result; we don't need it for the loop and let it drop.
        let (mut stdin, _stdin_result) = wasip3::cli::stdin::read_via_stream();

        futures::join!(
            async {
                // Resolves once `out_tx` is dropped and the stream is drained.
                let _ = wasip3::cli::stdout::write_via_stream(out_rx).await;
            },
            async {
                repl(&mut out_tx, &mut stdin).await;
                // Dropping the writer lets `write_via_stream` finish.
                drop(out_tx);
            }
        );

        Ok(())
    }
}

/// The read/eval/print loop. Reads stdin chunks, reassembles newline-delimited lines,
/// dispatches each through [`crate::eval`], and writes a prompt before every read. Returns
/// on `exit` or end-of-input.
async fn repl(out: &mut StreamWriter<u8>, stdin: &mut StreamReader<u8>) {
    let mut pending: Vec<u8> = Vec::new();

    write_bytes(out, PROMPT).await;

    loop {
        let (status, chunk) = stdin.read(Vec::with_capacity(READ_CHUNK)).await;

        if !chunk.is_empty() {
            pending.extend_from_slice(&chunk);

            // Dispatch every complete line currently buffered.
            while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = pending.drain(..=nl).collect();
                let (bytes, flow) = eval(trim_eol(&raw));
                write_bytes(out, &bytes).await;
                if let Flow::Exit = flow {
                    return;
                }
                write_bytes(out, PROMPT).await;
            }
        }

        // The writer end of stdin dropping (or a cancelled read) is EOF.
        if matches!(status, StreamResult::Dropped | StreamResult::Cancelled) {
            break;
        }
    }

    // A final line without a trailing newline (e.g. EOF mid-line) still runs.
    if !pending.is_empty() {
        let (bytes, flow) = eval(trim_eol(&pending));
        write_bytes(out, &bytes).await;
        if let Flow::Exit = flow {
            return;
        }
    }

    // Leave the cursor on a fresh line after the dangling prompt.
    write_bytes(out, b"\n").await;
}

/// Write all `bytes` to the stdout stream. The writer accepts the whole buffer; any
/// returned remainder (unexpected here) is dropped.
async fn write_bytes(out: &mut StreamWriter<u8>, bytes: &[u8]) {
    let _ = out.write_all(bytes.to_vec()).await;
}
