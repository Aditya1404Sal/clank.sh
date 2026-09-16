//! SPIKE — `bash`, clank's shell core exported as a Golem agent tool.
//!
//! A walking skeleton of the 1.6 `bash` tool, built to prove the shape on a live cluster. Links
//! only the shell core (`bash`) — no ai/mcp/grease/golem plug-in code, and no `Clank` plug-in — so
//! the shipped `clank:bash` component stays genuinely lean.
//!
//! One command, `bash run [--state <S>] <script>`, whose result record carries the session state
//! back to the caller. The tool keeps nothing between calls: a fresh Store runs every invocation,
//! and a caller that wants continuity hands the last `state` back unchanged.
//!
//! The state is `"1." + base64url(json)` holding the working directory, the last exit status and
//! the shell's variables, functions and aliases as a re-sourceable script. Not carried yet: shell
//! options, the pending prompt (so there is no `answer-prompt` command), and secret filtering.

// The tool macros expand to dispatch items without doc comments, their per-trait dispatcher takes one
// argument per command parameter, and their generated wrappers carry no `# Errors` section; none of
// it is reachable from hand-written code. Mirrors `fixtures/echo-tool`.
#![allow(missing_docs, clippy::too_many_arguments, clippy::missing_errors_doc)]

use std::fmt::Write as _;

use base64::Engine as _;
use bash::session::Session;
use golem_rust::{FromSchema, IntoSchema, ToolError, tool_definition, tool_implementation};
use serde::{Deserialize, Serialize};

use durable_log_sink::DurableLogSink;

mod durable_log_sink;

/// Cap on the encoded state. It rides in every invocation's input and result, both recorded in the
/// owner's oplog, so an unbounded one would grow the oplog on every line.
pub const MAX_STATE_BYTES: usize = 256 * 1024;

/// The version prefix of the encoded state; any other prefix is refused.
const STATE_VERSION: &str = "1";

/// Printed by the shell's own builtins in re-sourceable syntax, and captured after every script.
const CAPTURE: &str = "declare -p; declare -f; alias -p";

/// The result of one `bash run`. Field order is the wire contract (the value model is positional).
#[derive(Debug, Clone, IntoSchema, FromSchema)]
pub struct BashResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u8,
    /// The question of a `prompt-user` the script surfaced, if any.
    pub pending_prompt: Option<String>,
    pub cwd: String,
    /// Opaque; pass it back as `--state` to continue this session. Empty when none could be built.
    pub state: String,
}

#[derive(Debug, Clone, ToolError)]
pub enum BashError {
    #[tool_error(kind = "runtime-error", exit_code = 1)]
    Internal { reason: String },
}

#[tool_definition(version = "0.1.0")]
pub trait Bash {
    /// Run a script. Pass the previous result's `state` as `--state` to continue that session.
    #[arg(state = "option", default = "")]
    #[arg(script = "positional")]
    async fn run(&self, state: String, script: String) -> Result<BashResult, BashError>;
}

struct BashImpl;

#[tool_implementation]
impl Bash for BashImpl {
    async fn run(&self, state: String, script: String) -> Result<BashResult, BashError> {
        Ok(run_script(&state, &script).await)
    }
}

/// One call: restore `state` into a fresh shell, run `script`, capture the state back.
pub async fn run_script(state: &str, script: &str) -> BashResult {
    let mut session = match new_session().await {
        Ok(session) => session,
        // Reported as a well-formed result rather than a panic: this is an agent's tool-call path,
        // where a panic traps the guest and wedges the durable instance for every later invocation.
        Err(e) => {
            return BashResult {
                stdout: String::new(),
                stderr: format!("bash: failed to start shell: {e}\n"),
                exit_code: 1,
                pending_prompt: None,
                cwd: String::new(),
                state: String::new(),
            };
        }
    };

    let mut stderr = String::new();
    if !state.is_empty() {
        match Snapshot::decode(state) {
            // The restore's own output (a readonly variable refusing reassignment, say) is not
            // the caller's business; only the script's is returned.
            Ok(snapshot) => {
                let _ = session.eval_line(&snapshot.restore_script()).await;
            }
            Err(e) => {
                stderr = format!("bash: ignoring --state ({e}); starting a fresh session\n");
            }
        }
    }

    let result = session.eval_line(script).await;
    // Read the cwd AFTER the line runs, so a `cd` is reflected; the eval borrow has ended.
    let cwd = session.cwd().display().to_string();
    stderr.push_str(&lossy_utf8(result.stderr));
    let captured = session.eval_line(CAPTURE).await;
    let snapshot = Snapshot {
        cwd: cwd.clone(),
        last_exit_code: result.exit_code,
        shell_state: lossy_utf8(captured.stdout),
    };
    let state = snapshot.encode(MAX_STATE_BYTES).unwrap_or_else(|e| {
        let _ = writeln!(
            stderr,
            "bash: {e}; no state returned, the next call starts fresh"
        );
        String::new()
    });

    BashResult {
        stdout: lossy_utf8(result.stdout),
        stderr,
        exit_code: result.exit_code,
        pending_prompt: result.pending_prompt.map(|p| p.question),
        cwd,
        state,
    }
}

/// A fresh shell session with the replay-safe `/var/log` sink installed.
///
/// `clank-embed`'s `EmbeddedShell::ensure` builds a `Session` the same way and installs the same
/// kind of sink (see [`durable_log_sink`]); this tool inlines the equivalent rather than depending
/// on that crate, so the shipped `clank:bash` component links only the shell core. There is no
/// wasm/native split here (unlike the `EmbeddedShell::with_default_golem_providers` this replaces):
/// `Session::new` itself already builds the right async runtime for each target, and this tool never
/// installed the plug-in providers that used to be the only reason for the split.
async fn new_session() -> Result<Session, String> {
    let mut session = Session::new().await.map_err(|e| e.to_string())?;
    // `set_log_sink` takes `Arc`; the sink is `?Send`+`?Sync` and this tool is single-threaded.
    #[allow(clippy::arc_with_non_send_sync)]
    session.set_log_sink(std::sync::Arc::new(DurableLogSink::new()));
    Ok(session)
}

/// `bytes` as a `String`, valid UTF-8 passed through unchanged and any invalid byte replaced rather
/// than the whole call failing — shell output is not guaranteed valid UTF-8, and a wire result field
/// has nowhere else to put raw bytes.
fn lossy_utf8(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// The session a caller carries between calls.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Snapshot {
    cwd: String,
    last_exit_code: u8,
    shell_state: String,
}

impl Snapshot {
    fn encode(&self, limit: usize) -> Result<String, String> {
        let json = serde_json::to_vec(self).map_err(|e| format!("cannot encode state: {e}"))?;
        let encoded = format!(
            "{STATE_VERSION}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
        );
        if encoded.len() > limit {
            return Err(format!(
                "session state is {} bytes, over the {limit}-byte limit",
                encoded.len()
            ));
        }
        Ok(encoded)
    }

    fn decode(state: &str) -> Result<Self, String> {
        let (version, body) = state
            .split_once('.')
            .ok_or_else(|| "not a bash session state".to_string())?;
        if version != STATE_VERSION {
            return Err(format!(
                "session state version {version} is not supported (this bash reads version {STATE_VERSION})"
            ));
        }
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|e| format!("not a bash session state: {e}"))?;
        serde_json::from_slice(&json).map_err(|e| format!("not a bash session state: {e}"))
    }

    /// Variables, functions and aliases first, then the directory, then `$?` last so the script
    /// sees the previous call's status.
    fn restore_script(&self) -> String {
        format!(
            "{}\ncd {} 2>/dev/null\n(exit {})",
            self.shell_state,
            single_quote(&self.cwd),
            self.last_exit_code
        )
    }
}

/// `s` as one single-quoted shell word.
fn single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        Snapshot {
            cwd: "/tmp/it's here".to_string(),
            last_exit_code: 3,
            shell_state: "declare -- x=\"1\"\nf () \n{ \n    echo fn\n}\n".to_string(),
        }
    }

    #[test]
    fn state_round_trips() {
        let encoded = sample().encode(MAX_STATE_BYTES).unwrap();
        assert!(encoded.starts_with("1."));
        assert_eq!(Snapshot::decode(&encoded).unwrap(), sample());
    }

    #[test]
    fn unknown_version_and_garbage_are_refused() {
        let body = sample().encode(MAX_STATE_BYTES).unwrap();
        let v2 = body.replacen("1.", "2.", 1);
        assert!(Snapshot::decode(&v2).unwrap_err().contains("version 2"));
        assert!(Snapshot::decode("nonsense").is_err());
        assert!(Snapshot::decode("1.!!!").is_err());
    }

    #[test]
    fn oversize_state_is_refused() {
        let err = sample().encode(16).unwrap_err();
        assert!(err.contains("16-byte limit"), "{err}");
    }

    /// The whole point of the state: a second call, in a brand-new shell, sees what the first left.
    #[test]
    fn a_session_survives_through_its_state() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let first = run_script(
                "",
                "cd /tmp; x=kept; f() { echo fn-kept; }; alias ll='echo alias-kept'; (exit 4)",
            )
            .await;
            assert_eq!(first.exit_code, 4, "stderr: {}", first.stderr);
            assert!(
                !first.state.is_empty(),
                "no state; stderr: {}",
                first.stderr
            );

            let second = run_script(&first.state, "echo \"$?\"; pwd; echo $x; f; alias ll").await;
            assert_eq!(second.exit_code, 0, "stderr: {}", second.stderr);
            let lines: Vec<&str> = second.stdout.lines().collect();
            assert_eq!(lines.first(), Some(&"4"), "stdout: {}", second.stdout);
            assert!(second.stdout.contains("tmp"), "stdout: {}", second.stdout);
            assert!(
                second.stdout.contains("kept\n"),
                "stdout: {}",
                second.stdout
            );
            assert!(
                second.stdout.contains("fn-kept"),
                "stdout: {}",
                second.stdout
            );
            assert!(
                second.stdout.contains("alias-kept"),
                "stdout: {}",
                second.stdout
            );

            let fresh = run_script("", "echo \"[$x]\"").await;
            assert_eq!(fresh.stdout, "[]\n", "no state must mean a fresh shell");

            let bad = run_script("1.garbage", "echo ran").await;
            assert_eq!(bad.stdout, "ran\n");
            assert!(bad.stderr.contains("ignoring --state"), "{}", bad.stderr);
        });
    }

    #[test]
    fn restore_script_quotes_the_directory_and_sets_status_last() {
        let script = sample().restore_script();
        assert!(script.contains(r"cd '/tmp/it'\''s here'"), "{script}");
        assert!(script.ends_with("(exit 3)"), "{script}");
    }
}
