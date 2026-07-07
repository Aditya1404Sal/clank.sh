//! clank.sh shell core.
//!
//! A long-running, terminal-like read/eval/print loop that runs on two targets:
//!
//! - **wasm32** — as a `wasi:cli/run` component (WASI 0.3, async p3 streams). See
//!   [`wasm`].
//! - **native** — as an ordinary executable over blocking `std::io`. See [`native`].
//!
//! Only the I/O mechanism differs between targets. Command evaluation ([`eval`]) is a
//! pure, target-agnostic function shared by both — the seam where real command resolution
//! (the process table, `$PATH`, Brush) will be introduced later.
//!
//! Designs: `dev-docs/designs/proposed/shell-entrypoint-and-io-realized.md` (wasm path)
//! and `dev-docs/designs/proposed/target-abstraction-native-and-wasm.md` (target split).

#[cfg(target_arch = "wasm32")]
mod wasm;

#[cfg(not(target_arch = "wasm32"))]
pub mod native;

/// The interactive prompt written before each line is read.
pub const PROMPT: &[u8] = b"clank$ ";

const HELP: &[u8] = b"clank.sh builtins:\n  echo [args...]   write arguments to stdout\n  help             show this listing\n  exit             leave the shell\n";

/// What the loop should do after evaluating a line.
pub enum Flow {
    /// Keep looping.
    Continue,
    /// Leave the shell.
    Exit,
}

/// Resolve and run a single command line, returning the bytes to write to stdout and the
/// control flow. Pure: no I/O, no async — both the wasm and native drivers call this and
/// handle writing the returned bytes themselves. This is the dispatch seam for future
/// command resolution; today it handles a fixed builtin set.
pub fn eval(line: &[u8]) -> (Vec<u8>, Flow) {
    let text = String::from_utf8_lossy(line);
    let mut words = text.split_whitespace();

    let Some(cmd) = words.next() else {
        // Empty line: re-prompt only.
        return (Vec::new(), Flow::Continue);
    };

    match cmd {
        "exit" => (Vec::new(), Flow::Exit),
        "echo" => {
            let mut out = words.collect::<Vec<_>>().join(" ").into_bytes();
            out.push(b'\n');
            (out, Flow::Continue)
        }
        "help" => (HELP.to_vec(), Flow::Continue),
        other => (
            format!("clank: command not found: {other}\n").into_bytes(),
            Flow::Continue,
        ),
    }
}

/// Strip a single trailing `\n` and an optional preceding `\r` (CRLF-tolerant).
pub fn trim_eol(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && line[end - 1] == b'\r' {
            end -= 1;
        }
    }
    &line[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out_of(line: &[u8]) -> String {
        String::from_utf8(eval(line).0).unwrap()
    }

    #[test]
    fn echo_writes_joined_args_with_newline() {
        assert_eq!(out_of(b"echo hello world"), "hello world\n");
    }

    #[test]
    fn echo_with_no_args_writes_blank_line() {
        assert_eq!(out_of(b"echo"), "\n");
    }

    #[test]
    fn exit_signals_exit_with_no_output() {
        let (out, flow) = eval(b"exit");
        assert!(out.is_empty());
        assert!(matches!(flow, Flow::Exit));
    }

    #[test]
    fn help_lists_builtins() {
        assert!(out_of(b"help").contains("builtins"));
    }

    #[test]
    fn empty_line_produces_nothing_and_continues() {
        let (out, flow) = eval(b"   ");
        assert!(out.is_empty());
        assert!(matches!(flow, Flow::Continue));
    }

    #[test]
    fn unknown_command_reports_not_found() {
        assert_eq!(
            out_of(b"nosuchcmd"),
            "clank: command not found: nosuchcmd\n"
        );
    }

    #[test]
    fn trim_eol_handles_lf_crlf_and_bare() {
        assert_eq!(trim_eol(b"a\n"), b"a");
        assert_eq!(trim_eol(b"a\r\n"), b"a");
        assert_eq!(trim_eol(b"a"), b"a");
        assert_eq!(trim_eol(b""), b"");
    }
}
