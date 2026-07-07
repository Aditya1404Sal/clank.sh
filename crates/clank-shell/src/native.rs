//! Native target: the shell as an ordinary executable over blocking `std::io`. All command
//! execution and output capture live in the shared [`crate::session::Session`]; this driver is
//! just the prompt/read/write loop.

use crate::session::Session;
use crate::{trim_eol, Flow, PROMPT};
use std::io::{self, Write};

/// Run the interactive read/eval/print loop until `exit` or end-of-input.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut session = Session::new().await?;
    let mut line = String::new();

    loop {
        write_stdout(PROMPT)?;

        line.clear();
        if io::stdin().read_line(&mut line)? == 0 {
            // EOF: leave the cursor on a fresh line after the dangling prompt.
            write_stdout(b"\n")?;
            break;
        }

        let line_str = String::from_utf8_lossy(trim_eol(line.as_bytes())).into_owned();
        let (output, flow) = session.run_line(&line_str).await;
        write_stdout(&output)?;
        if let Flow::Exit = flow {
            break;
        }
    }

    Ok(())
}

/// Write all `bytes` to stdout and flush. Takes a fresh stdout handle each call so no lock is
/// held across the `.await` on command execution.
fn write_stdout(bytes: &[u8]) -> io::Result<()> {
    let mut out = io::stdout();
    out.write_all(bytes)?;
    out.flush()
}
