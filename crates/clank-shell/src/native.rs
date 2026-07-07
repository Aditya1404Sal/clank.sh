//! Native target: the shell as an ordinary executable over blocking `std::io`.
//!
//! The prompt is flushed before each blocking read, so no concurrency is needed (unlike
//! the wasm stream path). Command evaluation is delegated to the pure, shared
//! [`crate::eval`].

use crate::{eval, trim_eol, Flow, PROMPT};
use std::io::{self, BufRead, Write};

/// Run the interactive read/eval/print loop until `exit` or end-of-input.
pub fn run() -> io::Result<()> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut out = io::stdout().lock();
    let mut line = String::new();

    loop {
        out.write_all(PROMPT)?;
        out.flush()?;

        line.clear();
        if input.read_line(&mut line)? == 0 {
            // EOF: leave the cursor on a fresh line after the dangling prompt.
            out.write_all(b"\n")?;
            out.flush()?;
            break;
        }

        let (bytes, flow) = eval(trim_eol(line.as_bytes()));
        out.write_all(&bytes)?;
        out.flush()?;
        if let Flow::Exit = flow {
            break;
        }
    }

    Ok(())
}
