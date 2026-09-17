//! Reusable bash execution with the owner's bound tools projected as commands.
//!
//! The component links the shell core and shared Golem adapter. Clank adds its plug-in around
//! that same core when embedding it. Each invocation creates a fresh shell; callers pass the
//! returned state into `run`, `answer-prompt`, or `abort-prompt` to continue.
//!
//! Version 2 state carries cwd, exit status, variables, functions, aliases, shell options, and
//! pending core prompts. Version 1 remains readable. Marked secret variables are omitted and
//! state is bounded. Background jobs and secret capability arguments are unsupported.
//! Provider stderr awaits an upstream protocol channel; shell and RPC diagnostics use stderr.

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

/// Emitted state version; the decoder also accepts legacy version 1.
const STATE_VERSION: &str = "2";

/// A question awaiting a caller response.
#[derive(Debug, Clone, IntoSchema, FromSchema)]
pub struct BashPrompt {
    pub question: String,
    pub choices: Option<Vec<String>>,
}

#[derive(Debug, Clone, IntoSchema, FromSchema)]
pub struct BashResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u8,
    /// The question of a `prompt-user` the script surfaced, if any.
    pub pending_prompt: Option<BashPrompt>,
    pub cwd: String,
    /// Opaque; pass it back as `--state` to continue this session. Empty when none could be built.
    pub state: String,
}

#[derive(Debug, Clone, ToolError)]
pub enum BashError {
    #[tool_error(kind = "runtime-error", exit_code = 1)]
    Internal { reason: String },
}

#[tool_definition(version = "0.2.0")]
pub trait Bash {
    /// Run a script. Pass the previous result's `state` as `--state` to continue that session.
    #[arg(state = "option", default = "")]
    #[arg(script = "positional")]
    async fn run(&self, state: String, script: String) -> Result<BashResult, BashError>;

    /// Answer the pending question in a previous result's state.
    #[arg(state = "option", required = true)]
    #[arg(response = "positional")]
    async fn answer_prompt(&self, state: String, response: String)
    -> Result<BashResult, BashError>;

    /// Cancel the pending question in a previous result's state.
    #[arg(state = "option", required = true)]
    async fn abort_prompt(&self, state: String) -> Result<BashResult, BashError>;
}

struct BashImpl;

#[tool_implementation]
impl Bash for BashImpl {
    async fn run(&self, state: String, script: String) -> Result<BashResult, BashError> {
        Ok(run_script(&state, &script).await)
    }
    async fn answer_prompt(
        &self,
        state: String,
        response: String,
    ) -> Result<BashResult, BashError> {
        Ok(answer(&state, Some(response)).await)
    }
    async fn abort_prompt(&self, state: String) -> Result<BashResult, BashError> {
        Ok(answer(&state, None).await)
    }
}

/// One call: restore `state` into a fresh shell, run `script`, capture the state back.
pub async fn run_script(state: &str, script: &str) -> BashResult {
    run_action(state, Some(script), None).await
}
/// Resume a prompt through a fresh shell instance.
pub async fn answer(state: &str, response: Option<String>) -> BashResult {
    run_action(state, None, response).await
}
async fn run_action(state: &str, script: Option<&str>, response: Option<String>) -> BashResult {
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
                if let Err(error) = session
                    .restore_shell_state(
                        &snapshot.shell_state,
                        &snapshot.cwd,
                        snapshot.last_exit_code,
                    )
                    .await
                {
                    stderr = format!("bash: cannot restore --state: {error}\n");
                }
                session.restore_confirmation_grant(snapshot.confirmation_granted);
                if let Some(pending) = snapshot.pending {
                    session.restore_continuation(pending);
                }
            }
            Err(e) => {
                stderr = format!("bash: ignoring --state ({e}); starting a fresh session\n");
            }
        }
    }

    let result = if let Some(script) = script {
        session.eval_line(script).await
    } else {
        session.answer_prompt(response).await
    };
    // Read the cwd AFTER the line runs, so a `cd` is reflected; the eval borrow has ended.
    let cwd = session.cwd().display().to_string();
    stderr.push_str(&lossy_utf8(result.stderr));
    let captured = session.capture_shell_state().await;
    let snapshot = Snapshot {
        cwd: cwd.clone(),
        last_exit_code: result.exit_code,
        shell_state: captured,
        pending: session.continuation(),
        confirmation_granted: session.confirmation_granted(),
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
        pending_prompt: result.pending_prompt.map(|p| BashPrompt {
            question: p.question,
            choices: p.choices,
        }),
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
    session.enable_stateless_mode();
    // `set_log_sink` takes `Arc`; the sink is `?Send`+`?Sync` and this tool is single-threaded.
    #[allow(clippy::arc_with_non_send_sync)]
    session.set_log_sink(std::sync::Arc::new(DurableLogSink::new()));
    bash_golem::install(&mut session)?;
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
    #[serde(default)]
    pending: Option<bash::session::ShellContinuation>,
    #[serde(default)]
    confirmation_granted: bool,
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
        if state.len() > MAX_STATE_BYTES {
            return Err("session state exceeds the size limit".into());
        }
        let (version, body) = state
            .split_once('.')
            .ok_or_else(|| "not a bash session state".to_string())?;
        if version != STATE_VERSION && version != "1" {
            return Err(format!(
                "session state version {version} is not supported (this bash reads version {STATE_VERSION})"
            ));
        }
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|e| format!("not a bash session state: {e}"))?;
        serde_json::from_slice(&json).map_err(|e| format!("not a bash session state: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        Snapshot {
            cwd: "/tmp/it's here".to_string(),
            last_exit_code: 3,
            shell_state: "declare -- x=\"1\"\nf () \n{ \n    echo fn\n}\n".to_string(),
            pending: None,
            confirmation_granted: false,
        }
    }

    #[test]
    fn state_round_trips() {
        let encoded = sample().encode(MAX_STATE_BYTES).unwrap();
        assert!(encoded.starts_with("2."));
        assert_eq!(Snapshot::decode(&encoded).unwrap(), sample());
    }

    #[test]
    fn unknown_version_and_garbage_are_refused() {
        let body = sample().encode(MAX_STATE_BYTES).unwrap();
        let v3 = body.replacen("2.", "3.", 1);
        assert!(Snapshot::decode(&v3).unwrap_err().contains("version 3"));
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
            let before = run_script("", "x=before-prompt").await;
            let prompted = run_script(&before.state, "prompt-user choose --choices yes,no").await;
            assert!(prompted.pending_prompt.is_some());
            assert!(
                Snapshot::decode(&prompted.state)
                    .unwrap()
                    .shell_state
                    .contains("before-prompt")
            );
            let invalid = answer(&prompted.state, Some("invalid".into())).await;
            assert!(invalid.pending_prompt.is_some());
            let answered = answer(&invalid.state, Some("yes".into())).await;
            assert!(answered.pending_prompt.is_none());
            assert_eq!(answered.stdout, "yes\n");
            assert_eq!(
                run_script(&answered.state, "echo $x").await.stdout,
                "before-prompt\n"
            );
            let prompted = run_script(&answered.state, "prompt-user choose").await;
            assert_eq!(answer(&prompted.state, None).await.exit_code, 130);
            let options = run_script("", "set -o pipefail; set -o nounset").await;
            assert_ne!(
                run_script(&options.state, "false | true").await.exit_code,
                0
            );
            let secret = run_script(
                "",
                "export --secret BASH_STATE_TEST_SECRET=hidden-state-value",
            )
            .await;
            assert!(
                !Snapshot::decode(&secret.state)
                    .unwrap()
                    .shell_state
                    .contains("hidden-state-value")
            );
            // SAFETY: this is the only session-driving test in this process; the other tests only encode/decode state.
            #[allow(unsafe_code)]
            unsafe {
                std::env::remove_var("BASH_STATE_TEST_SECRET");
            }
            assert_eq!(run_script("", "echo background &").await.exit_code, 2);
            assert_eq!(run_script("", "echo wait").await.exit_code, 0);
            assert_eq!(
                run_script("", "echo \"$(echo hidden &)\"").await.exit_code,
                2
            );
            for script in [
                "eval 'echo hidden &'",
                "builtin eval 'echo hidden &'",
                "command eval 'echo hidden &'",
                "action='echo hidden &'; eval \"$action\"",
                "alias hidden='echo hidden &'",
                "trap 'echo hidden &' EXIT",
                "f() { echo hidden & }; f",
                "if true; then coproc echo hidden; fi",
                "for x in a; do echo hidden & done",
                "echo \"${unset:-$(echo hidden &)}\"",
                "echo $(( $(echo hidden &) + 1 ))",
                "echo `echo hidden &`",
                "bg",
                "fg",
                "wait",
                "fc",
                "x='$(echo hidden &)'; echo ${x@P}",
                "source /dev/stdin",
                "shopt -s promptvars",
            ] {
                let refused = run_script("", script).await;
                assert_eq!(refused.exit_code, 2, "{script}: {}", refused.stderr);
                assert!(refused.stdout.is_empty(), "{script}: {}", refused.stdout);
            }
            assert_eq!(
                run_script("", "eval 'echo allowed'").await.stdout,
                "allowed\n"
            );
            assert_eq!(
                run_script("", "echo $((3 & 1)); echo '&'; echo coproc")
                    .await
                    .stdout,
                "1\n&\ncoproc\n"
            );
            let trace = run_script("", "PS4='$(echo hidden &)'; set -x; echo trace-kept").await;
            assert_eq!(trace.exit_code, 0);
            assert_eq!(trace.stdout, "trace-kept\n");
            let source_path =
                std::env::temp_dir().join(format!("bash-tool-source-{}.sh", std::process::id()));
            std::fs::write(&source_path, "echo hidden &").unwrap();
            let source_command = format!("source '{}'", source_path.display());
            assert_eq!(run_script("", &source_command).await.exit_code, 2);
            std::fs::write(
                &source_path,
                "x=source-kept; echo \"$1\"; return 7; echo unreachable",
            )
            .unwrap();
            let sourced = run_script("", &format!("{source_command} source-arg")).await;
            assert_eq!(sourced.exit_code, 7, "{}", sourced.stderr);
            assert_eq!(sourced.stdout, "source-arg\n");
            assert_eq!(
                run_script(&sourced.state, "echo $x").await.stdout,
                "source-kept\n"
            );
            std::fs::remove_file(source_path).unwrap();
            let mut legacy = sample();
            legacy.cwd = "/tmp".into();
            let legacy_state = legacy
                .encode(MAX_STATE_BYTES)
                .unwrap()
                .replacen("2.", "1.", 1);
            assert!(
                run_script(&legacy_state, "echo $x; f")
                    .await
                    .stdout
                    .contains("fn")
            );
            let mut forged = sample();
            forged.cwd = "/tmp".into();
            forged.shell_state = "echo injected".into();
            let refused =
                run_script(&forged.encode(MAX_STATE_BYTES).unwrap(), "echo intended").await;
            assert!(!refused.stdout.contains("injected"));
            assert!(refused.stderr.contains("state cannot execute"));
            forged.shell_state = "f() { echo hidden & };".into();
            let refused = run_script(&forged.encode(MAX_STATE_BYTES).unwrap(), "type f").await;
            assert!(refused.stderr.contains("background"));
            assert!(!refused.stdout.contains("function"));
        });
    }
}
