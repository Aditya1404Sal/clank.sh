//! Native REPL stdin regression: a uutils consumer must not swallow the command that follows it.
//!
//! uu tools (`wc`/`cat`/`sort`/…) read their input through the process-global `std::io::stdin()`.
//! The REPL used to read the shell's own lines through that *same* buffered singleton, whose first
//! `read_line` eagerly pulls the entire remaining feed into a shared userspace buffer. A uu consumer
//! then drained that buffer mid-command, stealing the REPL's already-queued next line (and
//! over-counting). `native.rs::read_line_raw` fixes it by reading the shell's input unbuffered.
//!
//! This lives here, spawning the real `clank` binary with piped stdin, because the conformance suite
//! drives `Session::eval_line` in-process and never exercises the `run_plain`/`io::stdin` path where
//! the bug lived — which is exactly how it slipped past 34/0 native conformance.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::{Command, Stdio};

/// Feed `input` to the real `clank` binary's REPL over piped stdin; return its captured stdout.
fn repl(input: &str) -> String {
    // MCP config defaults to `/etc/mcp` (unwritable); point it at temp dirs so `Session::new` starts.
    let tmp = std::env::temp_dir();
    let etc = tmp.join("clank-cli-it-mcp");
    let bin = tmp.join("clank-cli-it-mcp-bin");
    let _ = std::fs::create_dir_all(&etc);
    let _ = std::fs::create_dir_all(&bin);

    let mut child = Command::new(env!("CARGO_BIN_EXE_clank"))
        .env("CLANK_MCP_ETC", &etc)
        .env("CLANK_MCP_BIN", &bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the clank binary");

    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(input.as_bytes())
        .expect("write to child stdin");

    let output = child.wait_with_output().expect("wait for clank");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn uu_consumer_pipeline_does_not_swallow_the_next_command() {
    // Before the fix, `wc` drained the REPL's read-ahead buffer, ate the queued `echo SURVIVED`,
    // and the shell hit EOF — SURVIVED never printed.
    let out = repl("ls | wc -l\necho SURVIVED\nexit\n");
    assert!(
        out.contains("SURVIVED"),
        "the command after a uu-consumer pipeline was swallowed; stdout: {out:?}"
    );
}

#[test]
fn nested_substitution_uu_consumer_does_not_swallow_the_next_command() {
    // The tool loop inside `ask` runs uu consumers exactly like this nested substitution.
    let out = repl("echo n=$(ls | wc -l)\necho SURVIVED\nexit\n");
    assert!(
        out.contains("SURVIVED"),
        "the command after a nested $(uu pipeline) was swallowed; stdout: {out:?}"
    );
}

#[test]
fn uu_consumer_does_not_overcount_by_stealing_queued_lines() {
    // Exactly three lines into `wc -l` must report 3 — not 3 plus the trailing REPL lines it used
    // to steal from the shared buffer.
    let out = repl("printf 'a\\nb\\nc\\n' | wc -l\necho SURVIVED\nexit\n");
    assert!(
        out.lines().any(|l| l.trim_start_matches("clank$ ").trim() == "3"),
        "uu consumer over-counted by stealing queued REPL lines; stdout: {out:?}"
    );
    assert!(out.contains("SURVIVED"), "next command also lost; stdout: {out:?}");
}

#[test]
fn uu_consumer_with_file_redirect_reads_the_file_and_keeps_the_next_command() {
    // `wc -l < file` must read the FILE (a redirect opens it on a fresh fd, so the stager stages it)
    // and must not fall through to real fd 0 — which read the shell's own stdin and swallowed the
    // next command. Verifies both: the count is the file's, and the following command survives.
    let tmp = std::env::temp_dir().join("clank-cli-it-redirect.txt");
    std::fs::write(&tmp, "x\ny\nz\n").expect("write temp file");
    let out = repl(&format!("wc -l < {}\necho SURVIVED\nexit\n", tmp.display()));
    assert!(
        out.contains("SURVIVED"),
        "the command after a `< file` uu redirect was swallowed; stdout: {out:?}"
    );
    assert!(
        out.lines().any(|l| l.trim_start_matches("clank$ ").trim() == "3"),
        "`wc -l < file` did not read the 3-line file (fell through to stdin?); stdout: {out:?}"
    );
    let _ = std::fs::remove_file(&tmp);
}
