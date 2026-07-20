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
//! cannot substitute for that. Pipelines and `$(...)` DO work on wasm: the Brush fork replaces the
//! OS-pipe + spawn model with an in-memory `OpenFile::Stream` pipe run inline-sequentially ("Wall C").

use crate::builtins::{promptuser, typecmd};
use crate::{dispatch_context, Flow, Transcript};
use brush_builtins::{BuiltinSet, ShellBuilderExt};
use brush_core::openfiles::{OpenFile, OpenFiles};
use brush_core::{ExecutionControlFlow, Shell, SourceInfo};

use std::sync::{Arc, Mutex};

use crate::authz::{self, AuthzState, Decision};
use crate::runtime::process::ProcessKind;
use crate::builtins::promptuser::{AnswerInput, PendingPrompt, Resolution};
use crate::runtime::proctable::ProcessTable;
use crate::registry::CommandRegistry;

type BoxError = Box<dyn std::error::Error>;

mod prompt;
mod agent;
mod mcp;
mod ask;
mod grease;

/// Why the shell is paused awaiting a response — set alongside the [`PendingPrompt`].
enum PendingKind {
    /// A `prompt-user` invocation: the answer is returned to the caller verbatim.
    UserPrompt,
    /// An authorization confirmation gating a command: on approval the stashed `command` runs; on
    /// denial the caller gets exit `5`. `all` (when offered) also sets the session `allow_all` grant.
    /// `ask_stdin` carries a pre-captured pipeline stdin payload for a deferred `cat x | ask` tail, so
    /// the piped context survives the pause/resume (restored into `next_ask_stdin` before re-running).
    AuthConfirm {
        command: String,
        sudo_grant: bool,
        ask_stdin: Option<String>,
    },
    /// An `ask` agentic loop paused mid-flight: the model requested a tool call that needs the human
    /// (an authorization confirmation, or the `prompt_user` tool). The whole conversation-so-far is
    /// carried in `state`; `answer_prompt` resolves the paused tool call and resumes the loop. This is
    /// in-memory but replay-safe — Golem rebuilds it by deterministic replay, and each model `turn`
    /// replays from the oplog.
    AgentLoop {
        state: Box<AskLoopState>,
        /// The pause under resolution: what the human is being asked and how to resolve it.
        pause: AskPause,
    },
}

/// A tool call in an `ask` loop that paused for the human, plus the sibling calls in the same turn
/// still to run and the results already computed — everything needed to resume the loop after the
/// human answers.
struct AskPause {
    /// The tool call awaiting the human's answer.
    call: crate::ai::ask::AskToolCall,
    /// The kind of pause, which decides how the human's answer resolves the call.
    kind: AskPauseKind,
    /// Sibling calls from the same assistant turn, not yet executed.
    remaining: Vec<crate::ai::ask::AskToolCall>,
    /// Results already computed for earlier calls in this turn.
    completed: Vec<crate::ai::ask::AskToolResult>,
}

/// The outcome of attempting one tool call in the loop: either a finished result to feed back, or a
/// pause requiring the human before the call can be resolved.
enum ToolStep {
    Done(crate::ai::ask::AskToolResult),
    Pause(AskPauseKind),
}

/// How a paused `ask` tool call is resolved by the human's answer.
enum AskPauseKind {
    /// An authorization confirmation for a `shell` command line. On approval the line runs; on denial
    /// a refusal tool result is fed back. `sudo_grant` marks a sudo-only gate (no "all" offered).
    Confirm { command: String, sudo_grant: bool },
    /// The `prompt_user` tool: the human's answer text becomes the tool result verbatim.
    PromptUser,
}

/// An active `ask repl` session: its isolated transcript and the model it targets. Held on the
/// `Session` only while the native driver is inside the REPL loop. `:model` mutates `model`;
/// `:new-session` clears `transcript`.
struct ReplState {
    transcript: Transcript,
    model: String,
}

/// The carried state of an in-flight `ask` agentic loop, enough to resume it after a pause. Owned data
/// only (replay-safe).
struct AskLoopState {
    /// The system prompt (rebuilt once at loop start; stable across turns).
    system: String,
    /// The tool definitions offered each turn (stable across turns).
    tools: Vec<crate::ai::ask::AskTool>,
    /// The conversation so far: `User`, then alternating `Assistant`/`ToolResults`.
    history: Vec<crate::ai::ask::AskTurn>,
    /// The model id.
    model: String,
    /// Accumulated tool trace (→ the final result's stderr).
    trace: Vec<u8>,
    /// Blanket confirm-tier authorization for the rest of this loop (upgraded to true on "all").
    blanket_authorized: bool,
    /// `--json`: the final answer must validate as JSON (exit 0) or the loop exits 6 with the raw
    /// text on stderr (README's `--json` contract).
    json: bool,
}

/// The shell's paused state: the surfaced prompt, the process-table row it belongs to, and why.
struct Pending {
    prompt: PendingPrompt,
    pid: Option<u32>,
    kind: PendingKind,
}

/// A per-line snapshot of the installed capability surface, keyed by the MCP + grease state versions.
/// Rebuilt only when that key changes; otherwise the same `Arc`s are re-installed each line (a cheap
/// clone) instead of re-rendering the manifests / resource index / system prompt every command.
struct CapabilityCache {
    key: (u64, u64),
    dynreg: std::sync::Arc<std::sync::Mutex<Vec<crate::manifest::Manifest>>>,
    mcpfs: std::sync::Arc<Vec<crate::runtime::mcpfs::ResourceEntry>>,
    sysprompt: std::sync::Arc<String>,
}

/// One live background job: the Brush job manager's id and the clank process-table PID that
/// represents it (row state `S` until reaped or killed).
struct BgJob {
    job_id: usize,
    pid: u32,
}

/// A triggered/scheduled agent invocation awaiting a possible `kill`-cancel: its proc-table PID and
/// the opaque cancel token the invoker understands (README:850). `None` token = not cancelable.
struct PendingInvocation {
    pid: u32,
    cancel_token: Option<String>,
}

/// The result of evaluating one shell line.
pub struct LineResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: u8,
    pub flow: Flow,
    /// Set when this line surfaced a `prompt-user` question the shell is now awaiting a response
    /// to. The caller must collect a human answer and deliver it via [`Session::answer_prompt`]
    /// (the shell does not block). `None` for every ordinary line.
    pub pending_prompt: Option<PendingPrompt>,
}

impl LineResult {
    /// Bytes as a terminal would display them through the legacy `run_line` API.
    pub fn terminal_output(&self) -> Vec<u8> {
        let mut output = self.stdout.clone();
        output.extend_from_slice(&self.stderr);
        output
    }

    fn continue_with_stdout(stdout: Vec<u8>) -> Self {
        Self {
            stdout,
            stderr: Vec::new(),
            exit_code: 0,
            flow: Flow::Continue,
            pending_prompt: None,
        }
    }

    fn stderr(message: impl Into<Vec<u8>>) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: message.into(),
            exit_code: 1,
            flow: Flow::Continue,
            pending_prompt: None,
        }
    }

    /// An authorization failure: exit `5` (README) with a stderr message.
    fn denied() -> Self {
        Self {
            stdout: Vec::new(),
            stderr: b"clank: authorization denied\n".to_vec(),
            exit_code: 5,
            flow: Flow::Continue,
            pending_prompt: None,
        }
    }

    /// Build a result from an HTTP command's outcome (`wcurl`/`waget` return the same shape).
    fn from_outcome(stdout: Vec<u8>, stderr: Vec<u8>, exit_code: u8) -> Self {
        Self {
            stdout,
            stderr,
            exit_code,
            flow: Flow::Continue,
            pending_prompt: None,
        }
    }
}

/// A live shell session: the Brush interpreter plus the session transcript and the command
/// registry.
pub struct Session {
    shell: Shell,
    /// The session transcript. Shared behind `Arc<Mutex>` (like `proc_table`) so each executed
    /// line can install it into the thread-local slot the Brush-registered `context` builtin
    /// reads — that's how `$(context show)` and `context show | head` reach it.
    transcript: Arc<Mutex<Transcript>>,
    /// The clank-owned inventory of command manifests (sits beside `transcript` as a shell-owned
    /// state object). Drives command metadata surfaces; not yet consulted on the execution path.
    registry: CommandRegistry,
    /// The process table: one row per executed line. Shared behind `Arc<Mutex>` so `run_line` can
    /// install it into the process-global slot the `ps` builtin reads (Brush builtins can't reach
    /// `Session` directly).
    proc_table: Arc<Mutex<ProcessTable>>,
    /// A question the shell has surfaced and is awaiting a response to (a `prompt-user` invocation
    /// or an authorization confirmation), plus its process row and kind. Durable `Session` state
    /// (persisted on the Golem oplog), so it survives across invocations — the caller answers via
    /// [`Session::answer_prompt`]. `None` when nothing is outstanding.
    pending: Option<Pending>,
    /// Session-scoped authorization state (the "all" grant). See [`crate::authz`].
    authz: AuthzState,
    /// Live background jobs: the Brush job id ↔ clank PID mapping `kill <pid>` resolves through.
    /// Deterministic under Golem replay — derived purely from the replayed line history, like the
    /// process table (the JoinHandles themselves are rebuilt by re-execution).
    bg_jobs: Vec<BgJob>,
    /// The injected LLM provider for `ask`. Installed by the agent build (a durable Anthropic
    /// provider); `None` on native and until injected, in which case `ask` degrades to a clean
    /// "not configured" error. See [`crate::ai::ask`].
    ask_provider: Option<Box<dyn crate::ai::ask::AskProvider>>,
    /// The injected Golem-agent invoker (durable `WasmRpc` on the agent; a fake in tests). `None` on
    /// native / until injected, in which case an agent invocation degrades to a clean "needs a cluster"
    /// error. See [`crate::golem::agent`].
    agent_invoker: Option<Box<dyn crate::golem::agent::AgentInvoker>>,
    /// The injected Golem cluster interface backing the `golem` command + agent oplog/status (durable
    /// `golem:api` bindings on the agent). `None` on native / until injected → the honest no-cluster
    /// error. See [`crate::golem::cluster`].
    golem_cluster: Option<Box<dyn crate::golem::cluster::GolemCluster>>,
    /// Triggered/scheduled agent invocations awaiting a possible `kill`-cancel: PID → cancel token.
    pending_invocations: Vec<PendingInvocation>,
    /// Out-of-band stdin for the next `ask` dispatch: the captured stdout of an upstream pipeline
    /// stage (`cat x | ask "…"`). Set by the pipe pre-extraction in `eval_line` (or restored on a
    /// deferred-confirm resume) and `take()`n by `run_ask`. `None` for an ordinary `ask` line.
    next_ask_stdin: Option<String>,
    /// An active `ask repl` session's isolated transcript + model. `Some` only while the native
    /// driver is inside a REPL; `run_repl_turn` renders/records against THIS transcript, not the main
    /// one, giving the REPL its own context window. Never set on the durable agent (REPL is a
    /// native-terminal feature there).
    repl: Option<ReplState>,
    /// Installed MCP servers + open sessions. Reconstructed deterministically under Golem replay.
    mcp: crate::mcp::state::McpState,
    /// The injected MCP HTTP transport. Installed by the agent build (a durable `wstd` client);
    /// `None` on native, in which case MCP degrades to a clean "not configured" error. Also used by
    /// `grease` for registry fetches (it's a generic durable "fetch bytes over HTTPS" seam).
    mcp_http: Option<Box<dyn crate::mcp::client::McpHttp>>,
    /// Installed grease packages (prompts). Reconstructed from the durable agent FS on boot.
    grease: crate::grease::state::GreaseState,
    /// The log sink installed per-line (the `/var/log` observability layer). Defaults to the direct
    /// append sink (correct on native); the agent injects a whole-file-rewrite sink whose writes are
    /// idempotent under oplog replay, avoiding line duplication (see `logging` + `log_sink`).
    log_sink: std::sync::Arc<dyn crate::logging::LogSink>,
    /// Cached capability views (dynamic manifests / MCP resource index / system prompt) installed per
    /// line. These are functions of the MCP + grease state (registry is static), so they're rebuilt only
    /// when `(mcp.version(), grease.version())` changes — most lines reuse the cache instead of
    /// re-rendering the whole system prompt + manifests every command.
    cap_cache: Option<CapabilityCache>,
    source: SourceInfo,
    /// Variables marked sensitive via `export --secret NAME=VALUE` (README "Sensitive environment
    /// variables"): `name → value`. The value is available to agents via the environment (set in
    /// Brush's variable table and `std::env`, so `$NAME` expands and subprocesses inherit it) but is
    /// redacted from `env`, `ps`, `/proc`, the logs, and the transcript. Installed per-line into the
    /// [`crate::runtime::secretenv`] thread-local so the synchronous render paths can honor that.
    /// Deterministic under Golem replay — rebuilt purely from the replayed line history, like
    /// `bg_jobs` and the process table. `BTreeMap` for a stable install order.
    secret_env: std::collections::BTreeMap<String, String>,
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
            let mut session = Self {
                shell,
                transcript: Arc::new(Mutex::new(Transcript::new())),
                registry: crate::registry::build(),
                proc_table: Arc::new(Mutex::new(ProcessTable::new())),
                pending: None,
                authz: AuthzState::default(),
                bg_jobs: Vec::new(),
                ask_provider: None,
                agent_invoker: None,
                golem_cluster: None,
                pending_invocations: Vec::new(),
                next_ask_stdin: None,
                repl: None,
                mcp: crate::mcp::state::McpState::default(),
                mcp_http: None,
                grease: crate::grease::state::GreaseState::load(),
                log_sink: std::sync::Arc::new(crate::logging::DefaultLogSink),
                cap_cache: None,
                source: SourceInfo::default(),
                secret_env: std::collections::BTreeMap::new(),
                rt,
            };
            session.reconstruct_mcp_from_grease();
            Ok(session)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let shell = build_shell().await?;
            let mut session = Self {
                shell,
                transcript: Arc::new(Mutex::new(Transcript::new())),
                registry: crate::registry::build(),
                proc_table: Arc::new(Mutex::new(ProcessTable::new())),
                pending: None,
                authz: AuthzState::default(),
                bg_jobs: Vec::new(),
                ask_provider: None,
                agent_invoker: None,
                golem_cluster: None,
                pending_invocations: Vec::new(),
                next_ask_stdin: None,
                repl: None,
                mcp: crate::mcp::state::McpState::default(),
                mcp_http: None,
                grease: crate::grease::state::GreaseState::load(),
                log_sink: std::sync::Arc::new(crate::logging::DefaultLogSink),
                cap_cache: None,
                source: SourceInfo::default(),
                secret_env: std::collections::BTreeMap::new(),
            };
            session.reconstruct_mcp_from_grease();
            Ok(session)
        }
    }

    /// Re-register grease-installed MCP servers into `McpState` from their cached grease payloads.
    /// `McpState` is empty on boot (it's replay-rebuilt, not FS-backed), but a grease-installed MCP
    /// package durably cached its tool listing — so we rebuild the server + tool surface here without a
    /// live `tools/list` (the actual `tools/call` still goes to the server at invocation time).
    fn reconstruct_mcp_from_grease(&mut self) {
        for m in self.grease.mcp_packages() {
            if !m.artifacts.tools {
                continue;
            }
            let config = crate::mcp::config::McpServerConfig {
                url: m.url.clone(),
                enabled: true,
                auth_env: m.auth_env.clone(),
                auth_header: None,
            };
            let tools: Vec<crate::mcp::state::McpTool> = m
                .tools
                .iter()
                .map(|t| crate::mcp::state::McpTool {
                    name: t.name.clone(),
                    description: if t.description.is_empty() {
                        None
                    } else {
                        Some(t.description.clone())
                    },
                    input_schema: serde_json::from_str(&t.input_schema)
                        .unwrap_or(serde_json::json!({})),
                })
                .collect();
            self.mcp.set_installed(&m.name, config, tools);
        }
    }

    /// The command registry — clank's inventory of command manifests.
    pub fn registry(&self) -> &CommandRegistry {
        &self.registry
    }

    /// Install the LLM provider that backs `ask`. The agent build injects a durable Anthropic
    /// provider here after constructing the session; without one, `ask` reports "not configured".
    pub fn set_ask_provider(&mut self, provider: Box<dyn crate::ai::ask::AskProvider>) {
        // Wrap so each LLM turn is logged to http.log (the outbound Anthropic call).
        self.ask_provider = Some(Box::new(crate::ai::ask::LoggingAskProvider::new(provider)));
    }

    /// Install the Golem-agent invoker (a durable `WasmRpc` binding on the agent). Without one, an
    /// installed agent command reports "needs a cluster" (README:895). Injected after construction.
    pub fn set_agent_invoker(&mut self, invoker: Box<dyn crate::golem::agent::AgentInvoker>) {
        self.agent_invoker = Some(invoker);
    }

    /// Install the Golem cluster interface backing the `golem` command + agent oplog/status (durable
    /// `golem:api` bindings on the agent). Without one, `golem` reports "needs a cluster".
    pub fn set_golem_cluster(&mut self, cluster: Box<dyn crate::golem::cluster::GolemCluster>) {
        self.golem_cluster = Some(cluster);
    }

    /// Install the MCP HTTP transport (a durable `wstd` client on the agent). Without one, MCP
    /// commands report "not configured" (exit 4). Injected after construction like the ask provider.
    pub fn set_mcp_http(&mut self, http: Box<dyn crate::mcp::client::McpHttp>) {
        // Wrap the transport so every MCP + grease-registry request is logged to http.log (redacted).
        self.mcp_http = Some(Box::new(crate::mcp::client::LoggingMcpHttp::new(http)));
    }

    /// Install the `/var/log` log sink. The agent injects a whole-file-rewrite sink (idempotent under
    /// oplog replay, so no duplicated lines); native keeps the default direct-append sink.
    pub fn set_log_sink(&mut self, sink: std::sync::Arc<dyn crate::logging::LogSink>) {
        self.log_sink = sink;
    }

    /// Evaluate one input line: record it, serve the clank-specific `context` builtin, otherwise
    /// execute it through Brush.
    /// Evaluate one command line, logging its lifecycle to `shell.log`: a `start` event as the line
    /// begins and an `end` (with exit code) when it finishes — or a `pause` when it stops for a
    /// `prompt-user`/authorization question (the eventual `end` is logged when `answer_prompt` resolves
    /// it). The actual command dispatch lives in [`eval_line_inner`](Self::eval_line_inner).
    pub async fn eval_line(&mut self, line: &str) -> LineResult {
        // Install this session's log sink for the whole line so every logging call site (shell/http/mcp/
        // ops, deep in run_command / McpClient::call / coreutils) routes through it.
        let _log = crate::logging::install(self.log_sink.clone());
        if !line.trim().is_empty() {
            crate::logging::Record::new("start").field("line", line).emit(crate::logging::LogFile::Shell);
        }
        let result = self.eval_line_inner(line).await;
        self.log_line_outcome(line, &result);
        result
    }

    /// Emit the shell.log terminal event for a finished (or paused) line.
    fn log_line_outcome(&self, line: &str, result: &LineResult) {
        if line.trim().is_empty() {
            return;
        }
        let event = if result.pending_prompt.is_some() { "pause" } else { "end" };
        let mut rec = crate::logging::Record::new(event).field("line", line);
        if result.pending_prompt.is_none() {
            rec = rec.field("exit", result.exit_code.to_string());
        }
        rec.emit(crate::logging::LogFile::Shell);
    }

    async fn eval_line_inner(&mut self, line: &str) -> LineResult {
        // A prompt is already outstanding: the caller must answer it (via `answer_prompt`), not run
        // a new command. The shell never blocks, so it's the caller's job to notice `pending_prompt`
        // and respond. Reject the command with a clear message rather than silently interleaving.
        // ONE command is allowed through: `kill <pid-of-the-paused-row>` aborts the pending prompt
        // (the P-state kill) — the same contract as an explicit abort (exit 130 / 5).
        if self.pending.is_some() {
            let pending_pid = self.pending.as_ref().and_then(|p| p.pid);
            let kills_pending = matches!(
                (crate::builtins::kill::classify(line), pending_pid),
                (Some(Ok(args)), Some(pp)) if args
                    .targets
                    .iter()
                    .any(|t| matches!(t, crate::builtins::kill::Target::Pid(p) if *p == pp))
            );
            if kills_pending {
                self.transcript.lock().unwrap().record_command(line);
                return self.answer_prompt(None).await;
            }
            // Re-surface the still-outstanding prompt (NOT a bare stderr, which carries
            // pending_prompt=None): a caller that saw an empty pending_prompt here would believe the
            // prompt resolved and hand control back, wedging the session on a question it no longer
            // knows about — exactly the connect-shell deadlock a Ctrl-C'd `ask` left behind. Mirrors
            // the InvalidChoice re-ask in answer_prompt_inner (session/prompt.rs). Same message/exit;
            // only pending_prompt flips None→Some so any client can route the next input to
            // answer_prompt. `self.pending` is Some here (guarded above; kills_pending already fired).
            let prompt = self.pending.as_ref().map(|p| p.prompt.clone());
            return LineResult {
                stdout: Vec::new(),
                stderr: b"clank: a prompt-user question is awaiting a response; answer it first\n"
                    .to_vec(),
                exit_code: 1,
                flow: Flow::Continue,
                pending_prompt: prompt,
            };
        }

        // Install the secret-env set FIRST — before the line is recorded — so the synchronous render
        // paths (`env`, `ps`, `/proc`, the transcript recorder, log text) filter/mask `export
        // --secret` variables for this whole line. This must precede `record_command` below: a later
        // line that references a secret *by value* on its command line (e.g. `env | grep sk-abc`) is
        // masked in the transcript only if the secret set is already active when the line is recorded.
        // The other per-line installs (proctable/transcript/dynreg/…) happen further down; the secret
        // set is the one the recorder itself consults, so it leads.
        let _install_secretenv = crate::runtime::secretenv::install(std::sync::Arc::new(
            self.secret_env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ));

        // Record the typed line into the transcript. `record_command` masks any *already-known*
        // secret value, but the defining `export --secret KEY=val` line carries a not-yet-known
        // secret (this line is what marks it), so redact that value explicitly here first. See
        // `run_secret_export` and [`crate::runtime::secretenv`].
        match crate::builtins::secretenv::parse(line) {
            Some(secret) if !secret.value.is_empty() => {
                let redacted =
                    line.replace(&secret.value, crate::runtime::secretenv::REDACTED);
                self.transcript.lock().unwrap().record_command(&redacted);
            }
            _ => {
                self.transcript.lock().unwrap().record_command(line);
            }
        }

        // Reap finished background jobs (a tick-free poll of Brush's job manager): their rows flip
        // `S → Z`. Silent — bash-style "[1]+ Done" notifications are a later increment; `jobs`/`ps`
        // reflect the state.
        self.reap_bg_jobs();

        // Install this session's process table as the active one for the duration of the line, so
        // the `ps` builtin (a Brush builtin, which can't reach `Session` directly) can read it.
        // The guard clears the slot on drop. The transcript slot is the same pattern, read by the
        // Brush-registered `context` builtin in nested contexts ($(context show), context | head).
        let _install = crate::runtime::proctable::install(self.proc_table.clone());
        let _install_transcript = crate::install_transcript(self.transcript.clone());
        // Build (or reuse a cached) view of the installed capabilities, keyed by the MCP + grease state
        // versions — so the dynamic manifests (`man`/`type` resolution), the MCP resource index (`ls
        // /mnt/mcp/...`), and the live system prompt (`cat /proc/clank/system-prompt`) are re-rendered
        // only when a package/server was installed or removed, not on every command line.
        let cap_key = (self.mcp.version(), self.grease.version());
        if self.cap_cache.as_ref().map(|c| c.key) != Some(cap_key) {
            let mut manifests = self.mcp.all_manifests();
            manifests.extend(self.grease.all_manifests());
            self.cap_cache = Some(CapabilityCache {
                key: cap_key,
                dynreg: std::sync::Arc::new(std::sync::Mutex::new(manifests)),
                mcpfs: std::sync::Arc::new(self.grease.mcp_resource_index()),
                sysprompt: std::sync::Arc::new(crate::ai::ask::build_system_prompt_with_capabilities(
                    &self.registry,
                    &self.mcp,
                    &self.grease,
                )),
            });
        }
        // Clone the cached `Arc`s into the per-line thread-local slots (cheap ref-count bumps); the
        // guards clear the slots on drop. The manifests/index/prompt are read-only surfaces, so sharing
        // one `Arc` across lines is safe.
        let (dynreg, mcpfs, sysprompt) = {
            let cap = self.cap_cache.as_ref().expect("cap_cache just populated");
            (cap.dynreg.clone(), cap.mcpfs.clone(), cap.sysprompt.clone())
        };
        let _install_dynreg = crate::runtime::dynreg::install(dynreg);
        let _install_mcpfs = crate::runtime::mcpfs::install(mcpfs);
        let _install_sysprompt = crate::runtime::sysprompt::install(sysprompt);

        // Record this line as a process (one PID per executed line). Blank lines get no row, matching
        // the "empty line re-prompts" behavior. `context` lines DO get a row — they're real typed
        // work, and `ps` omitting them would mislead. The row is born `R` and marked `Z` only after
        // execution returns, so a `ps` in this same line sees its own row as `R`, like real Unix.
        let pid = {
            let argv: Vec<String> = line.split_whitespace().map(String::from).collect();
            if argv.is_empty() {
                None
            } else {
                let kind = classify(line);
                Some(self.proc_table.lock().unwrap().spawn(kind, argv))
            }
        };

        // Syntactically incomplete input (an unterminated heredoc/quote/substitution) must not run:
        // clank's Brush shell is non-interactive, so Brush marks the parse error FATAL and maps it
        // to `ExitShell` — which `finish` faithfully turns into `Flow::Exit`, ending the whole
        // session because someone typed `cat <<EOF`. Catch it here with Brush's own
        // incomplete-input classification and answer honestly instead. Placed after the pid spawn
        // (the attempt is real typed work; `ps` should show it) and before every intercept, so no
        // classifier ever sees a half-construct. The native REPL upgrades this to PS2 continuation
        // before eval; on the agent one eval is one invocation, so the whole construct must arrive
        // in a single line (documented).
        if self.line_is_incomplete(line) {
            let result = LineResult::from_outcome(
                Vec::new(),
                b"clank: incomplete input (a heredoc, quote, or substitution is missing its \
                  terminator); provide the full construct in one eval\n"
                    .to_vec(),
                2,
            );
            return self.finish_intercepted(pid, result);
        }

        // `<cmd> --help` for a clank-intercepted command: print its manifest help text (exit 0).
        // These commands never reach Brush's dispatch, so they'd otherwise ignore `--help`. Handled
        // FIRST among all interceptions (before `context`/`prompt-user`/curl and the authz gate) so
        // each intercepted command's own handling doesn't swallow `--help`: `context --help` would
        // otherwise be an "unknown subcommand", `prompt-user --help` would be parsed as a prompt, and
        // `curl --help` would surface an outbound-HTTP confirmation instead of just printing help.
        // Brush's own builtins (cat/grep) answer `--help` through their `get_content`; not here.
        if let Some(help) = typecmd::help_for(line, &self.registry) {
            let result = LineResult::from_outcome(help.into_bytes(), Vec::new(), 0);
            return self.finish_intercepted(pid, result);
        }

        // `context summarize` is the ONE context subcommand that needs the model (an outbound LLM
        // call), so it can't be served by the sync `dispatch_context`/`apply_context` engine — it
        // routes to the async Session layer like `ask`. Detected here (a top-level `context
        // summarize` with no shell operators, and any leading `sudo`), it goes through the authz gate
        // as Confirm (outbound HTTP; `sudo context summarize` pre-authorizes), then
        // `run_context_summarize`. A nested `$(context summarize)`/pipe stays with Brush and hits the
        // honest error in `apply_context`. Its output is inspection-only — NOT recorded back.
        if is_context_summarize(line) {
            let elevated = authz::leading_command(line).1;
            match authz::decide(
                crate::manifest::AuthorizationPolicy::Confirm,
                elevated,
                self.authz.allow_all,
            ) {
                Decision::Allow => {
                    // Inspection output — reap the row but do NOT record it back (like `context show`).
                    let result = self.run_context_summarize().await;
                    if let Some(pid) = pid {
                        self.proc_table.lock().unwrap().complete(pid);
                    }
                    return result;
                }
                Decision::Deny => {
                    return self.finish_intercepted(pid, LineResult::denied());
                }
                Decision::Confirm { sudo_grant } => {
                    return self.surface_auth_confirm(
                        Some("context"),
                        "context summarize".to_string(),
                        pid,
                        sudo_grant,
                        None,
                    );
                }
            }
        }

        // `context show` output is intentionally not recorded back into the transcript.
        if let Some(bytes) = dispatch_context(&mut self.transcript.lock().unwrap(), line) {
            if let Some(pid) = pid {
                self.proc_table.lock().unwrap().complete(pid);
            }
            return LineResult::continue_with_stdout(bytes);
        }

        // `prompt-user` is intercepted before Brush dispatch (like `context` above). It does NOT
        // block: it records a durable pending prompt, leaves the row in `P`, and returns
        // immediately — surfacing the question to the caller, who answers via `answer_prompt`. See
        // the `promptuser` module docs.
        if promptuser::is_prompt_user(line) {
            return self.surface_prompt(line, pid);
        }

        // `export --secret NAME=VALUE` — mark a variable sensitive (README "Sensitive environment
        // variables"). Intercepted before Brush so clank owns the secret table + std::env write and
        // the value never enters any rendered surface. Only a plain top-level line is handled; a
        // `--secret` export carrying operators (`&&`, `|`, `;`, `$()`) is REFUSED rather than
        // silently falling through to Brush — Brush's export would set the value with no redaction,
        // which is exactly the surface the flag exists to prevent (observed live: the demo probe
        // used `export --secret K=v && echo set` and concluded the feature was inert).
        if crate::builtins::secretenv::is_secret_export(line) {
            if is_plain_line(line) {
                return self.run_secret_export(line, pid);
            }
            let result = LineResult::from_outcome(
                Vec::new(),
                b"export --secret: must be a standalone command (no pipes, `&&`, `;`, or \
                  substitutions) so the value never reaches an unredacted surface\n"
                    .to_vec(),
                2,
            );
            return self.finish_intercepted(pid, result);
        }

        // `type` for clank's intercepted commands (`prompt-user`/`curl`/`wget`/`context`): Brush's
        // own `type` can't see them (they aren't Brush builtins), so clank answers for lines that
        // query ONLY intercepted names, matching Brush's wording. Any other `type` line (a
        // Brush-known name, a mix, an unrecognized flag) returns `None` here and falls through to
        // Brush's `type` unchanged. Read-only meta command — intercepted before the authz gate.
        if let Some((stdout, exit_code)) = typecmd::dispatch(line, &self.registry) {
            let result = LineResult::from_outcome(stdout.into_bytes(), Vec::new(), exit_code);
            return self.finish_intercepted(pid, result);
        }

        // `<server> --help` / `<server> <tool> --help` for an installed MCP server: print generated
        // help (before the authz gate — help never confirms).
        if let Some(help) = self.mcp_help_for(line) {
            let result = LineResult::from_outcome(help.into_bytes(), Vec::new(), 0);
            return self.finish_intercepted(pid, result);
        }

        // `<name> --help` for an installed grease command package (prompt or script): print its
        // generated help (before the gate).
        if let Some(help) = self.pkg_help_for(line) {
            let result = LineResult::from_outcome(help.into_bytes(), Vec::new(), 0);
            return self.finish_intercepted(pid, result);
        }

        // `ask repl` reaching `eval_line` is the durable-agent path (the native driver intercepts it
        // before `eval_line` and runs the interactive loop). An interactive REPL needs a terminal and
        // a blocking read-loop, which the durable agent can't own (Golem serializes invocations — it
        // can't park mid-loop waiting for a human). Return an honest pointer to the working forms.
        if crate::ai::ask::classify_repl(line).is_some() {
            let msg = b"ask repl: interactive REPL is a native-terminal feature; on the durable \
                        agent, drive a conversation with repeated `ask` calls (each is one turn)\n";
            return self.finish_intercepted(pid, LineResult::from_outcome(Vec::new(), msg.to_vec(), 2));
        }

        // stdin-as-context: `cat x | ask "…"`. The LLM call can't run inside Brush's pipeline (the
        // reactor isn't live there — the "Wall C" wall), so the Session pre-extracts it: run the
        // upstream, capture its stdout, and dispatch the `ask` tail at the session layer with those
        // bytes as stdin. `ask` must be the FINAL stage; anywhere else it stays the honest stub error.
        if let Some(pipe) = crate::ai::ask::split_ask_tail(line) {
            return self.run_ask_pipe(pipe, pid).await;
        }

        // Authorization gate: enforce the leading command's `authorization-policy` (README). A
        // `confirm`/`sudo-only` command that isn't pre-authorized surfaces a confirmation pause
        // (reusing the `prompt-user` mechanism) and defers the command until approved. In every
        // path the command actually run is the line with any leading `sudo` token stripped — `sudo`
        // is a clank authorization marker, not a real executable to dispatch to Brush.
        //
        // Resolution consults the static registry AND the dynamic MCP manifests (an installed server
        // name resolves to its Confirm-policy manifest — MCP tool calls are outbound HTTP).
        let (policy, elevated, command) = self.resolve_authz(line);
        let effective = strip_sudo_prefix(line);
        let decision = authz::decide(policy, elevated, self.authz.allow_all);
        // ops.log: a `sudo-only` command is the destructive tier (rm / overwrite). Log the attempt with
        // its authorization outcome — recorded even when denied, so a blocked destructive op still shows.
        if policy == crate::manifest::AuthorizationPolicy::SudoOnly {
            let outcome = match decision {
                Decision::Allow => "authorized",
                Decision::Deny => "denied",
                Decision::Confirm { .. } => "confirm-required",
            };
            crate::logging::Record::new("destructive")
                .field("cmd", command.as_deref().unwrap_or(""))
                .field("line", &effective)
                .field("outcome", outcome)
                .emit(crate::logging::LogFile::Ops);
        }
        match decision {
            Decision::Allow => {}
            Decision::Deny => {
                return self.finish_intercepted(pid, LineResult::denied());
            }
            Decision::Confirm { sudo_grant } => {
                return self
                    .surface_auth_confirm(command.as_deref(), effective, pid, sudo_grant, None);
            }
        }

        // `blanket_authorized` = whether an `ask` dispatched from this line runs its tool calls with
        // blanket confirm-tier authorization. Only a literal `sudo ask` (elevated here) or a
        // session-wide "all" grant qualifies — approving a bare `ask` later does NOT (see
        // `resolve_auth_confirm`, which passes `false`).
        let blanket = elevated || self.authz.allow_all;
        let result = self.run_command(&effective, pid, blanket).await;
        self.transcript.lock().unwrap().record_output(&result.terminal_output());
        // If recording just evicted old entries to stay under budget, upgrade the leading count marker
        // into a model-generated summary block (no-op when nothing was dropped or no provider exists).
        self.compact_dropped_span().await;
        result
    }

    /// Run an authorized command line and reap its process row (`R → Z`). The shared execution choke
    /// point, reached both by `eval_line` (a directly-allowed command) and by `answer_prompt` (an
    /// approved gated command). Does not record the transcript — the caller decides.
    ///
    /// `curl`/`wget` are dispatched here to their async HTTP crates, NOT through `execute`. This is
    /// load-bearing: `execute` runs Brush on clank's nested `rt.block_on`, where the wstd WASI-HTTP
    /// reactor is not the running executor (the "Wall C" shape). Awaiting `wcurl::run`/`waget::run`
    /// directly here keeps the HTTP one level under the Golem SDK's `wstd::block_on`, where the
    /// reactor is live. Both call paths funnel through here, so the direct-allow and
    /// post-approval-deferred routes both reach the HTTP correctly. See `httpcmd`.
    async fn run_command(
        &mut self,
        line: &str,
        pid: Option<u32>,
        blanket_authorized: bool,
    ) -> LineResult {
        let result = if is_context_summarize(line) {
            // Reached here only on a deferred-confirm re-run (top-level `context summarize` is
            // intercepted in `eval_line`). Route to the async summarizer; the caller
            // (`resolve_auth_confirm`) skips recording its inspection output.
            self.run_context_summarize().await
        } else if let Some(parsed) = crate::builtins::kill::classify(line) {
            // `kill` is Session-owned (it mutates the job table + proc table + pending state) and
            // MUST be tick-free: driving the runtime here could first-poll another parked
            // background job and wedge the invocation on its synchronous body.
            match parsed {
                Ok(args) => self.run_kill(&args),
                Err(e) => {
                    LineResult::from_outcome(Vec::new(), format!("kill: {e}\n").into_bytes(), 2)
                }
            }
        } else if let Some(args) = crate::ai::ask::classify(line) {
            // `ask` dispatches to the injected LLM provider — same "await at the Session layer, never
            // through `execute`'s nested runtime" rule as curl/wget. The provider's async `complete`
            // is awaited here, one level under the Golem SDK's `wstd::block_on` where the durable
            // context and the WASI-HTTP reactor are live. See `askcmd`.
            self.run_ask(args, blanket_authorized).await
        } else if let Some(parsed) = crate::mcp::cmd::classify(line) {
            // `mcp` management runs at the Session layer — its add/reload/session subcommands do
            // HTTP, which must await under the live reactor (same rule as curl/ask).
            match parsed {
                Ok(cmd) => self.run_mcp(cmd).await,
                Err(e) => LineResult::from_outcome(Vec::new(), format!("{e}\n").into_bytes(), 2),
            }
        } else if let Some(parsed) = crate::grease::cmd::classify(line) {
            // `grease` package management runs at the Session layer — install/search/update do HTTP.
            match parsed {
                Ok(cmd) => self.run_grease(cmd).await,
                Err(e) => LineResult::from_outcome(Vec::new(), format!("{e}\n").into_bytes(), 2),
            }
        } else if let Some(parsed) = crate::golem::cluster::classify(line) {
            // `golem` cluster command — runtime API calls await under the reactor (like mcp/ask).
            match parsed {
                Ok(cmd) => self.run_golem(cmd).await,
                Err(e) => LineResult::from_outcome(Vec::new(), format!("{e}\n").into_bytes(), 2),
            }
        } else if self.is_mcp_tool_line(line) {
            // `<server> <tool> …` for an installed MCP server: an outbound HTTP tool call (its authz
            // Confirm was already resolved via the dynamic manifest at the gate).
            self.run_mcp_tool(line).await
        } else if self.is_prompt_line(line) {
            // A grease-installed prompt: fill its body from args and run it through the model (its
            // Confirm was resolved via the dynamic manifest at the gate; sudo pre-authorizes).
            self.run_prompt(line, blanket_authorized).await
        } else if self.is_script_line(line) {
            // A grease-installed script: fill its body from args and run the shell source locally
            // (its Confirm was resolved via the dynamic manifest at the gate; sudo pre-authorizes).
            self.run_script(line, blanket_authorized).await
        } else if self.is_agent_line(line) {
            // A grease-installed Golem agent: parse the ctor/method/args and invoke it via wRPC in the
            // cluster (Confirm resolved at the gate; sudo pre-authorizes). Await mode only in v1.
            self.run_agent(line).await
        } else if self.is_mcp_template_line(line) {
            // A grease-installed MCP resource-template executable: substitute the args into the URI
            // template and read the constructed resource live (top-level only, Wall-C).
            self.run_mcp_template(line).await
        } else if let Some((server, uri)) = self.dynamic_mcp_read_target(line) {
            // A top-level `cat /mnt/mcp/<server>/<dynamic>`: fetch the resource live via
            // `resources/read` (the read can't run in Brush's synchronous `cat` — the Wall-C wall — so
            // it's served here at the Session layer for top-level lines only; inside $()/pipes it falls
            // through to Brush and hits the honest "no such file").
            self.run_mcp_resource_read(&server, &uri).await
        } else {
            match crate::builtins::http::classify(line) {
                Some((crate::builtins::http::HttpCommand::Curl, args)) => {
                    let o = wcurl::run(&args).await;
                    log_http_tool("curl", &args, o.exit_code);
                    LineResult::from_outcome(o.stdout, o.stderr, o.exit_code)
                }
                Some((crate::builtins::http::HttpCommand::Wget, args)) => {
                    let o = waget::run(&args).await;
                    log_http_tool("wget", &args, o.exit_code);
                    LineResult::from_outcome(o.stdout, o.stderr, o.exit_code)
                }
                None => {
                    let mut result = self.execute(line).await;
                    self.adopt_new_jobs(pid, &mut result);
                    result
                }
            }
        };
        if let Some(pid) = pid {
            self.proc_table.lock().unwrap().complete(pid);
        }
        result
    }

    /// Reap completed background jobs: poll Brush's job manager (never ticks the runtime — a
    /// parked-but-unstarted job stays unstarted) and flip finished jobs' proc rows `S → Z`.
    fn reap_bg_jobs(&mut self) {
        let Ok(results) = self.shell.jobs_mut().poll() else {
            return;
        };
        for (job, _result) in results {
            if let Some(idx) = self.bg_jobs.iter().position(|b| b.job_id == job.id) {
                let bg = self.bg_jobs.remove(idx);
                self.proc_table.lock().unwrap().complete(bg.pid);
            }
        }
    }

    /// After a line executes, register any background jobs it left in Brush's job manager: a proc
    /// row born `S` (parented to the spawning line's row), a job↔pid mapping for `kill`, and the
    /// bash-style `[id] pid` start line appended to stdout (Brush's own print is interactive-only).
    /// Then reap once — a `wait` in the same line may have completed jobs synchronously.
    fn adopt_new_jobs(&mut self, line_pid: Option<u32>, result: &mut LineResult) {
        let ppid = line_pid.unwrap_or(crate::runtime::proctable::SHELL_ROOT_PID);
        let new_jobs: Vec<(usize, String)> = self
            .shell
            .jobs()
            .jobs
            .iter()
            .filter(|j| !self.bg_jobs.iter().any(|b| b.job_id == j.id))
            .map(|j| (j.id, j.command_line.clone()))
            .collect();
        for (job_id, command_line) in new_jobs {
            let argv: Vec<String> = command_line.split_whitespace().map(String::from).collect();
            let bg_pid = self
                .proc_table
                .lock()
                .unwrap()
                .spawn_bg(crate::runtime::process::ProcessKind::Builtin, argv, ppid);
            self.bg_jobs.push(BgJob {
                job_id,
                pid: bg_pid,
            });
            result
                .stdout
                .extend_from_slice(format!("[{job_id}] {bg_pid}\n").as_bytes());
        }
        self.reap_bg_jobs();
    }

    /// Cancel background jobs — the synthetic `kill`. Resolves each target to a live job (by
    /// jobspec via Brush, or by clank PID via the `bg_jobs` mapping), removes it from the manager,
    /// aborts its future (dropped at its next await point — or never polled at all), and flips its
    /// row to `Z`. **Tick-free by design** (see `run_command`). Exit 0 when every target was
    /// killed, 1 on any miss (README exit-code table).
    fn run_kill(&mut self, args: &crate::builtins::kill::KillArgs) -> LineResult {
        use crate::builtins::kill::Target;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut any_missed = false;

        for target in &args.targets {
            let job_id = match target {
                Target::Job(spec) => {
                    match self.shell.jobs_mut().resolve_job_spec(spec).map(|j| j.id) {
                        Some(id) => id,
                        None => {
                            stderr.extend_from_slice(
                                format!("kill: {spec}: no such job\n").as_bytes(),
                            );
                            any_missed = true;
                            continue;
                        }
                    }
                }
                Target::Pid(pid) => {
                    if *pid == crate::runtime::proctable::SHELL_ROOT_PID {
                        stderr.extend_from_slice(
                            format!("kill: ({pid}) - Operation not permitted\n").as_bytes(),
                        );
                        any_missed = true;
                        continue;
                    }
                    // A pending (triggered/scheduled) agent invocation: cancel via its idempotency key
                    // (README:850). `run_kill` is tick-free (must not drive the runtime), so we can't
                    // await the remote cancel here — we drop the local tracking + row and report the
                    // best-effort cancel. (The scheduled-invocation token doesn't survive across the
                    // durable agent's serialized invocations, so a remote cancel-after-return isn't
                    // guaranteed — documented.)
                    if let Some(idx) = self.pending_invocations.iter().position(|p| p.pid == *pid) {
                        let inv = self.pending_invocations.remove(idx);
                        self.proc_table.lock().unwrap().complete(*pid);
                        let msg = if inv.cancel_token.is_some() {
                            format!("[{pid}] cancelled (queued/scheduled invocation)\n")
                        } else {
                            format!(
                                "[{pid}] already dispatched (fire-and-forget) — cannot cancel; \
                                 local tracking cleared\n"
                            )
                        };
                        stdout.extend_from_slice(msg.as_bytes());
                        continue;
                    }
                    match self.bg_jobs.iter().find(|b| b.pid == *pid) {
                        Some(bg) => bg.job_id,
                        None => {
                            stderr.extend_from_slice(
                                format!("kill: ({pid}) - No such process\n").as_bytes(),
                            );
                            any_missed = true;
                            continue;
                        }
                    }
                }
            };

            let jobs = &mut self.shell.jobs_mut().jobs;
            let Some(idx) = jobs.iter().position(|j| j.id == job_id) else {
                stderr.extend_from_slice(format!("kill: %{job_id}: no such job\n").as_bytes());
                any_missed = true;
                continue;
            };
            let mut job = jobs.remove(idx);
            job.abort();
            let mapping_idx = self.bg_jobs.iter().position(|b| b.job_id == job_id);
            let killed_pid = mapping_idx.map(|i| self.bg_jobs.remove(i).pid);
            if let Some(killed_pid) = killed_pid {
                self.proc_table.lock().unwrap().complete(killed_pid);
                stdout.extend_from_slice(
                    format!("[{job_id}] {killed_pid} Killed\t{}\n", job.command_line).as_bytes(),
                );
            } else {
                stdout.extend_from_slice(
                    format!("[{job_id}] Killed\t{}\n", job.command_line).as_bytes(),
                );
            }
        }

        LineResult::from_outcome(stdout, stderr, u8::from(any_missed))
    }



    /// Whether `line`'s leading word is an installed grease prompt. Drives the `run_prompt` dispatch.
    /// Only top-level lines (no operators) count — a prompt makes an LLM call and can't run in Brush's
    /// nested runtime (the Wall-C wall), so a nested use falls through to the stub honest error.
    fn is_prompt_line(&self, line: &str) -> bool {
        let Some(word) = prompt_leading_word(line) else {
            return false;
        };
        self.grease.is_prompt(&word)
    }

    /// Whether `line`'s leading word is an installed grease script. Drives the `run_script` dispatch.
    /// Like prompts, only top-level lines count (a script runs through the session's `execute`, which
    /// isn't reachable from Brush's nested runtime).
    fn is_script_line(&self, line: &str) -> bool {
        let Some(word) = prompt_leading_word(line) else {
            return false;
        };
        self.grease.is_script(&word)
    }

    /// Run an installed grease prompt: parse `--arg value` flags against the package's declared
    /// arguments, fill the body's `{{arg}}` placeholders, and dispatch the filled prompt through
    /// `run_ask`. `--model` on the prompt line overrides the package model. Missing required args are
    /// an exit-2 usage error (no model call).
    async fn run_prompt(&mut self, line: &str, blanket_authorized: bool) -> LineResult {
        let (name, provided, model_override) = match parse_pkg_invocation(line) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let Some(package) = self.grease.prompt(&name).cloned() else {
            return LineResult::denied(); // shouldn't happen (is_prompt_line gated it)
        };

        // Fill the template; a missing required arg is a clean usage error.
        let filled = match package.fill(&provided) {
            Ok(f) => f,
            Err(e) => {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("{name}: {e}\n").into_bytes(),
                    2,
                )
            }
        };

        let model = model_override.or_else(|| package.model.clone());
        let args = crate::ai::ask::AskArgs {
            prompt: filled,
            model,
            fresh: false,
            json: false,
            stdin: None,
        };
        self.run_ask(args, blanket_authorized).await
    }

    /// Run an installed grease script: parse `--arg value` flags against the package's declared
    /// arguments, fill the body's `{{arg}}` placeholders, and dispatch the filled **shell source**
    /// through the session's `execute` (Brush `run_string`) — the local-shell path, no LLM call.
    /// Missing required args are an exit-2 usage error. `blanket_authorized` is threaded for parity
    /// with prompts but the authz gate already resolved before dispatch; the script itself runs its
    /// filled body as ordinary shell.
    async fn run_script(&mut self, line: &str, _blanket_authorized: bool) -> LineResult {
        let (name, provided, _model) = match parse_pkg_invocation(line) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let Some(package) = self.grease.script(&name).cloned() else {
            return LineResult::denied(); // shouldn't happen (is_script_line gated it)
        };
        let filled = match package.fill(&provided) {
            Ok(f) => f,
            Err(e) => {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("{name}: {e}\n").into_bytes(),
                    2,
                )
            }
        };
        // Run the filled shell source through the local-shell path. `execute` adopts no jobs here
        // (a script is a synthetic top-level invocation, not a backgrounded pipeline stage).
        self.execute(&filled).await
    }


    /// Generated help for an installed command-package line ending in `--help` (prompt, script, or
    /// agent). `None` if the line isn't an installed command package or doesn't request help.
    ///
    /// A leading `sudo` is skipped: it only pre-authorizes, so it must not change what `--help`
    /// prints. Without this, `sudo <pkg> --help` looked up the package named "sudo", found nothing,
    /// and fell through to the package's own parser — for an agent that means
    /// "unknown flag --help before the method" (exit 2) instead of the agent's surface. The path is
    /// not hypothetical: `ask`'s per-command authorization re-runs an approved command WITH the sudo
    /// grant, so a model asking for `<pkg> --help` always took the broken one.
    /// (`builtins::typecmd::help_for` does the same for the statically-intercepted commands.)
    fn pkg_help_for(&self, line: &str) -> Option<String> {
        let words = crate::ai::ask::dequote_words(line)?;
        let name = match words.split_first() {
            Some((first, rest)) if first == "sudo" => rest.first()?,
            _ => words.first()?,
        };
        if words.iter().any(|w| w == "--help") {
            return self.grease.pkg_help(name);
        }
        None
    }

    /// Execute a `<server> <tool> …` MCP tool call: build the arguments from the tool's inputSchema
    /// (or `--args '<json>'`), issue `tools/call` (reusing an open session or initializing one), and
    /// render the result (text content joined, or raw JSON with `--json`).
    async fn run_mcp_tool(&mut self, line: &str) -> LineResult {
        let inv = match crate::mcp::cmd::parse_tool_invocation(line) {
            Some(Ok(inv)) => inv,
            Some(Err(e)) => {
                return LineResult::from_outcome(Vec::new(), format!("{}: {e}\n", "mcp").into_bytes(), 2)
            }
            None => return LineResult::denied(),
        };

        let Some(tool_name) = inv.tool.clone() else {
            // Bare `<server>` with no tool: show help (help path already handled this in eval_line, but
            // a direct run_command re-entry lands here).
            return LineResult::continue_with_stdout(
                self.mcp.server_help(&inv.server).unwrap_or_default().into_bytes(),
            );
        };

        // Resolve the tool + its schema.
        let Some(tool) = self.mcp.tool(&inv.server, &tool_name).cloned() else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("{}: no tool '{tool_name}' on server '{}'\n", inv.server, inv.server).into_bytes(),
                2,
            );
        };

        // Build the arguments object: `--args` escape hatch wins; else map --flags via the schema.
        let arguments = match &inv.raw_args {
            Some(raw) => match serde_json::from_str::<serde_json::Value>(raw) {
                Ok(v) => v,
                Err(e) => {
                    return LineResult::from_outcome(
                        Vec::new(),
                        format!("{}: --args is not valid JSON: {e}\n", inv.server).into_bytes(),
                        2,
                    )
                }
            },
            None => match build_mcp_arguments(&tool.input_schema, &inv.flags) {
                Ok(v) => v,
                Err(e) => {
                    return LineResult::from_outcome(
                        Vec::new(),
                        format!("{} {tool_name}: {e}\n", inv.server).into_bytes(),
                        2,
                    )
                }
            },
        };

        let Some(config) = self.mcp.get(&inv.server).map(|s| s.config.clone()) else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("{}: server not installed\n", inv.server).into_bytes(),
                1,
            );
        };
        // Reuse an explicit --session-id, else an open session for the server, else stateless.
        let session_id = inv
            .session_id
            .clone()
            .or_else(|| self.mcp.session_for(&inv.server).and_then(|s| s.server_session_id.clone()));

        let Some(http) = self.mcp_http.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"mcp: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };
        let auth = config.resolve_auth();
        let mut client = crate::mcp::client::McpClient::new(http, &config.url, auth);
        match client.call_tool(&tool_name, arguments, session_id.as_deref()).await {
            Ok(result) => {
                let out = if inv.json {
                    result.raw.to_string()
                } else {
                    result.text
                };
                let mut out = out.into_bytes();
                if !out.is_empty() && !out.ends_with(b"\n") {
                    out.push(b'\n');
                }
                LineResult::continue_with_stdout(out)
            }
            Err(e) => LineResult::from_outcome(
                Vec::new(),
                format!("{} {tool_name}: {}\n", inv.server, e.message).into_bytes(),
                e.exit_code,
            ),
        }
    }

    /// Complete an intercepted line's row and record its output (for intercepted paths that don't go
    /// through `run_command`, e.g. an authorization denial).
    fn finish_intercepted(&mut self, pid: Option<u32>, result: LineResult) -> LineResult {
        if let Some(pid) = pid {
            self.proc_table.lock().unwrap().complete(pid);
        }
        self.transcript.lock().unwrap().record_output(&result.terminal_output());
        result
    }

    /// Handle an intercepted `export --secret NAME=VALUE` (README "Sensitive environment variables").
    /// The variable is made available to agents via the environment — set (exported) in Brush's
    /// variable table so `$NAME` expands, and in `std::env` so real subprocesses inherit it — while
    /// its value is recorded in [`secret_env`](Self::secret_env) so every render path redacts it. The
    /// defining line was already recorded to the transcript with its value redacted (see the caller).
    ///
    /// Replay-safe on the durable agent: both `set_global` and `std::env::set_var` are whole-value
    /// writes (idempotent — re-running the line under oplog replay reproduces the same state), unlike
    /// an append (see the `golem-fs-append-replay-unsafe` note). Produces no stdout; the row is reaped.
    fn run_secret_export(&mut self, line: &str, pid: Option<u32>) -> LineResult {
        let Some(secret) = crate::builtins::secretenv::parse(line) else {
            // The caller already checked `is_secret_export`; this is unreachable in practice.
            return self.finish_intercepted(pid, LineResult::from_outcome(Vec::new(), Vec::new(), 0));
        };

        // Mark it exported in Brush's variable table so `$NAME` expands in scripts and it's part of
        // the exported set. `set_global` replaces any prior value (whole-value → replay-safe).
        let mut var = brush_core::variables::ShellVariable::new(secret.value.clone());
        var.export();
        if let Err(e) = self.shell.env_mut().set_global(&secret.name, var) {
            let msg = format!("export: {e}\n");
            return self.finish_intercepted(pid, LineResult::from_outcome(Vec::new(), msg.into_bytes(), 1));
        }

        // Make it visible to real subprocesses via the process environment (Full env parity). This is
        // the source `env` / `/proc/environ` read; the secret is filtered back out of *those displays*
        // by `secretenv::filter_environ`, but a spawned child still inherits it. Whole-value set →
        // idempotent under replay.
        std::env::set_var(&secret.name, &secret.value);

        // Record it in the session's secret table so the per-line `secretenv` install redacts it from
        // every rendered surface on subsequent lines.
        self.secret_env.insert(secret.name.clone(), secret.value);

        self.finish_intercepted(pid, LineResult::from_outcome(Vec::new(), Vec::new(), 0))
    }

    /// Run one input line for terminal-style callers. This keeps the original API used by the REPLs.
    pub async fn run_line(&mut self, line: &str) -> (Vec<u8>, Flow) {
        let result = self.eval_line(line).await;
        (result.terminal_output(), result.flow)
    }

    /// Whether a `prompt-user` question is currently awaiting a response.
    pub fn has_pending_prompt(&self) -> bool {
        self.pending.is_some()
    }


    /// Whether `line` is syntactically *incomplete* — an unterminated heredoc, quote, or
    /// substitution that needs more input to become a program — as opposed to complete-but-wrong.
    /// This is exactly brush-interactive's `is_valid_input` classification (its reedline validator
    /// and basic backend use the same two arms); Brush's `parse_string` is `#[cached]`, so the
    /// eventual `run_string` of the same text is a cache hit and this pre-parse is ~free.
    ///
    /// The native REPL uses it to drive PS2 continuation; `eval_line_inner` uses it to answer
    /// incomplete input honestly instead of letting the fatal-parse path end the session.
    pub fn line_is_incomplete(&self, line: &str) -> bool {
        match self.shell.parse_string(line) {
            Err(brush_parser::ParseError::Tokenizing { ref inner, .. }) if inner.is_incomplete() => {
                true
            }
            Err(brush_parser::ParseError::ParsingAtEndOfInput) => true,
            _ => false,
        }
    }

    /// Native execution: capture Brush's stdout and stderr into anonymous temp files.
    #[cfg(not(target_arch = "wasm32"))]
    async fn execute(&mut self, line: &str) -> LineResult {
        use std::io::{Read, Seek, SeekFrom};

        let stdout_capture = match tempfile::tempfile() {
            Ok(f) => f,
            Err(e) => return LineResult::stderr(format!("clank: {e}\n")),
        };
        let stderr_capture = match tempfile::tempfile() {
            Ok(f) => f,
            Err(e) => return LineResult::stderr(format!("clank: {e}\n")),
        };
        let (out_fd, err_fd) = match (stdout_capture.try_clone(), stderr_capture.try_clone()) {
            (Ok(o), Ok(e)) => (o, e),
            _ => return LineResult::stderr(b"clank: failed to set up output capture\n".to_vec()),
        };

        let mut params = self.shell.default_exec_params();
        params.set_fd(OpenFiles::STDOUT_FD, OpenFile::File(out_fd.into()));
        params.set_fd(OpenFiles::STDERR_FD, OpenFile::File(err_fd.into()));

        let result = self
            .shell
            .run_string(line.to_string(), &self.source, &params)
            .await;
        drop(params);

        let mut stdout = Vec::new();
        let mut stdout_reader = stdout_capture;
        let _ = stdout_reader
            .seek(SeekFrom::Start(0))
            .and_then(|_| stdout_reader.read_to_end(&mut stdout));

        let mut stderr = Vec::new();
        let mut stderr_reader = stderr_capture;
        let _ = stderr_reader
            .seek(SeekFrom::Start(0))
            .and_then(|_| stderr_reader.read_to_end(&mut stderr));

        finish(result, stdout, stderr)
    }

    /// Wasm execution: capture Brush's stdout and stderr into in-memory buffers and drive the
    /// async on the owned current-thread runtime.
    #[cfg(target_arch = "wasm32")]
    async fn execute(&mut self, line: &str) -> LineResult {
        let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let mut params = self.shell.default_exec_params();
        params.set_fd(
            OpenFiles::STDOUT_FD,
            OpenFile::Stream(Box::new(BufSink(stdout_buf.clone()))),
        );
        params.set_fd(
            OpenFiles::STDERR_FD,
            OpenFile::Stream(Box::new(BufSink(stderr_buf.clone()))),
        );

        let fut = self
            .shell
            .run_string(line.to_string(), &self.source, &params);
        let result = self.rt.block_on(fut);
        drop(params);

        let stdout = std::mem::take(&mut *stdout_buf.lock().unwrap());
        let stderr = std::mem::take(&mut *stderr_buf.lock().unwrap());
        finish(result, stdout, stderr)
    }
}

/// Classify a command line into a process kind for the process table. Everything is a `Builtin`
/// this increment; this is where Script/Prompt/AgentInvocation classification lands once those
/// command kinds exist (they'll be resolved from `$PATH` / the registry).
fn classify(_line: &str) -> ProcessKind {
    ProcessKind::Builtin
}

/// Strip a leading `sudo ` token from a line (the command to actually run once `sudo`-elevated
/// authorization is satisfied). Whitespace-based — sufficient for the leading-word scope of this
/// increment. If the line isn't `sudo`-prefixed, it's returned unchanged.
fn strip_sudo_prefix(line: &str) -> String {
    let trimmed = line.trim_start();
    match trimmed.strip_prefix("sudo") {
        // Only a `sudo` token followed by whitespace (not `sudoedit`, etc.).
        Some(rest) if rest.starts_with(char::is_whitespace) => rest.trim_start().to_string(),
        _ => line.to_string(),
    }
}

/// The resolved integrity status of a fetched package, threaded from `grease_install` into the
/// finish/persist path. Bundles the content-hash + signature + transparency-log results so the marker
/// construction has one source of truth.
struct InstallIntegrity {
    /// The computed sha256 of the payload body.
    sha256: String,
    /// Whether the sha256 matched the registry's advertised hash.
    verified: bool,
    /// Whether the ed25519 signature verified against the registry's trusted key.
    signature_verified: bool,
    /// The signer identity (when signature-verified).
    signer: Option<String>,
    /// Whether the RFC-6962 inclusion proof verified against the advertised root.
    log_verified: bool,
    /// The transparency-log leaf index (when log-verified).
    log_index: Option<u64>,
}

impl InstallIntegrity {
    /// Build the on-disk install marker for a given kind + registry.
    fn to_marker(&self, kind: crate::grease::pkg::PackageKind, registry: &str) -> crate::grease::state::InstallMarker {
        crate::grease::state::InstallMarker {
            kind,
            registry: registry.to_string(),
            sha256: self.sha256.clone(),
            verified: self.verified,
            signature_verified: self.signature_verified,
            signer: self.signer.clone(),
            log_verified: self.log_verified,
            log_index: self.log_index,
        }
    }

    /// The `sha256 … — verified, signed[, in log]` summary for the install output.
    fn summary(&self) -> String {
        let status = if self.verified { "verified" } else { "unverified" };
        let mut s = format!("sha256 {} — {status}", &self.sha256[..self.sha256.len().min(12)]);
        if self.signature_verified {
            s.push_str(", signed");
        }
        if self.log_verified {
            s.push_str(", in log");
        }
        s
    }
}

/// A package's advertised transparency-log inclusion proof (RFC-6962), from the index `log` object.
struct LogProof {
    leaf_index: u64,
    tree_size: u64,
    /// The tree's Merkle root (base64, 32 bytes).
    root: String,
    /// The audit path — sibling hashes bottom-up (base64, 32 bytes each).
    proof: Vec<String>,
}

/// A package's advertised integrity metadata from a registry's `index.json` entry.
#[derive(Default)]
struct IndexEntry {
    /// The advertised sha256 of the payload (content-addressing).
    sha256: Option<String>,
    /// The advertised base64 detached ed25519 signature over the payload body.
    sig: Option<String>,
    /// The advertised signer identity (surfaced in `info`/`list`).
    signer: Option<String>,
    /// The advertised RFC-6962 inclusion proof, if the registry runs a transparency log.
    log: Option<LogProof>,
}

/// Log a curl/wget invocation to http.log: the tool, its target URL (the first non-flag argument), and
/// the exit code. curl/wget bypass the `McpHttp` seam (their own `wstd`/`reqwest` fetch), so they're
/// logged here at the dispatch site rather than by the `LoggingMcpHttp` decorator.
fn log_http_tool(tool: &str, args: &[String], exit_code: u8) {
    let url = args.iter().find(|a| !a.starts_with('-')).map(String::as_str).unwrap_or("");
    crate::logging::Record::new("http")
        .field("tool", tool)
        .field("url", crate::logging::redact_url(url))
        .field("exit", exit_code.to_string())
        .emit(crate::logging::LogFile::Http);
}

/// Whether a fetched package body is a Markdown prompt with a leading `---` frontmatter fence (as
/// opposed to the JSON payload shape). Used to route `.md`-authored prompts through the frontmatter
/// converter after integrity verification. Checks the raw byte prefix directly (the fence is ASCII), so
/// a multibyte character right after the fence can't cause a misclassification.
fn is_markdown_frontmatter(body: &[u8]) -> bool {
    body.starts_with(b"---\n") || body.starts_with(b"---\r\n")
}

/// Best-effort lookup of a package's index entry (`sha256` + `sig` + `signer`). GETs
/// `<base>/index.json` and returns the fields of the entry whose `name` matches. Empty (`None`s) if the
/// index is unreachable, unparseable, or has no entry for `name` — the caller then falls back to
/// record-only integrity (and unsigned).
async fn fetch_index_entry(
    http: &dyn crate::mcp::client::McpHttp,
    base: &str,
    name: &str,
) -> IndexEntry {
    let url = format!("{}/index.json", base.trim_end_matches('/'));
    let Ok(resp) = http.request("GET", &url, &[], None).await else {
        return IndexEntry::default();
    };
    if resp.status != 200 {
        return IndexEntry::default();
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&resp.body) else {
        return IndexEntry::default();
    };
    let entry = v
        .get("packages")
        .and_then(|p| p.as_array())
        .and_then(|arr| arr.iter().find(|p| p.get("name").and_then(|n| n.as_str()) == Some(name)));
    let Some(entry) = entry else {
        return IndexEntry::default();
    };
    let s = |k: &str| entry.get(k).and_then(|x| x.as_str()).map(String::from);
    // The optional RFC-6962 transparency-log inclusion proof.
    let log = entry.get("log").and_then(|l| {
        let leaf_index = l.get("leaf-index").or_else(|| l.get("leaf_index"))?.as_u64()?;
        let tree_size = l.get("tree-size").or_else(|| l.get("tree_size"))?.as_u64()?;
        let root = l.get("root")?.as_str()?.to_string();
        let proof = l
            .get("proof")?
            .as_array()?
            .iter()
            .filter_map(|h| h.as_str().map(String::from))
            .collect();
        Some(LogProof { leaf_index, tree_size, root, proof })
    });
    IndexEntry { sha256: s("sha256"), sig: s("sig"), signer: s("signer"), log }
}

/// Verify a package's RFC-6962 inclusion proof: the log leaf is the payload's hex sha256 string (the
/// content-address), so the proof witnesses that this exact content was logged. Decodes the base64
/// root + proof nodes and delegates to [`crate::grease::pkg::verify_inclusion_proof`].
fn verify_log_inclusion(payload_sha256_hex: &str, log: &LogProof) -> Result<(), String> {
    use base64::Engine;
    let root = base64::engine::general_purpose::STANDARD
        .decode(log.root.trim())
        .map_err(|e| format!("invalid log root (base64): {e}"))?;
    let proof: Result<Vec<Vec<u8>>, String> = log
        .proof
        .iter()
        .map(|h| {
            base64::engine::general_purpose::STANDARD
                .decode(h.trim())
                .map_err(|e| format!("invalid proof node (base64): {e}"))
        })
        .collect();
    let proof = proof?;
    crate::grease::pkg::verify_inclusion_proof(
        payload_sha256_hex.as_bytes(),
        log.leaf_index,
        log.tree_size,
        &root,
        &proof,
    )
}

/// Materialize an MCP server's resources under `/mnt/mcp/<server>/` and return the cache entries that
/// drive the virtual-fs listing. Fetches `resources/list`; each resource whose `resources/read`
/// succeeds at install is written as a real STATIC file (composes in pipes); a resource that can't be
/// read now is recorded as DYNAMIC (served live on a top-level `cat` interception). Path-confined.
/// Free fn (no `self`) so it can run while `client` borrows `self.mcp_http`.
async fn materialize_mcp_resources(
    server: &str,
    client: &mut crate::mcp::client::McpClient<'_>,
    session: Option<&str>,
) -> Vec<crate::grease::pkg::McpResourceCache> {
    let Ok(resources) = client.list_resources(session).await else {
        return Vec::new();
    };
    let root = crate::grease::config::mcp_mount_dir().join(server);
    let mut cache = Vec::new();
    for res in &resources {
        let rel = mcp_resource_rel_path(&res.uri);
        let mut is_static = false;
        if let Some(dest) = crate::grease::config::mcp_safe_join(&root, &rel) {
            if let Ok(contents) = client.read_resource(&res.uri, session).await {
                if let Some(parent) = dest.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                is_static = std::fs::write(&dest, contents.as_bytes()).is_ok();
            }
        }
        cache.push(crate::grease::pkg::McpResourceCache {
            uri: res.uri.clone(),
            rel_path: rel,
            description: res.description.clone().unwrap_or_default(),
            mime_type: res.mime_type.clone(),
            is_static,
            last_modified: res.last_modified.clone(),
            audience: res.audience.clone(),
            priority: res.priority,
            size: res.size,
        });
    }
    cache
}

/// Fill an RFC-6570-lite URI template's `{param}` placeholders from CLI `args`. `--name value` fills
/// the placeholder named `name`; bare positional args fill the remaining placeholders left-to-right.
/// Values are inserted verbatim (MCP servers accept literal path segments). An unfilled placeholder is
/// an error. Walks the template once, resolving each placeholder as it's encountered.
fn fill_uri_template(template: &str, args: &[String]) -> Result<String, String> {
    // Parse args: `--name value` pairs + positionals.
    let mut named: Vec<(String, String)> = Vec::new();
    let mut positionals: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(key) = a.strip_prefix("--") {
            let val = it.next().ok_or_else(|| format!("--{key} needs a value"))?;
            named.push((key.to_string(), val.clone()));
        } else {
            positionals.push(a.clone());
        }
    }
    let mut pos_iter = positionals.into_iter();

    // Walk the template, replacing each `{…}` with its resolved value in order.
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close_rel) = rest[open..].find('}') else {
            // Unbalanced brace — emit verbatim and stop.
            out.push_str(&rest[open..]);
            return Ok(out);
        };
        let raw = &rest[open + 1..open + close_rel];
        let name = raw
            .trim_start_matches(['+', '#', '.', '/', ';', '?', '&'])
            .trim_end_matches('*');
        let value = named
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| pos_iter.next())
            .ok_or_else(|| format!("missing value for template parameter '{name}'"))?;
        out.push_str(&value);
        rest = &rest[open + close_rel + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Convert an MCP resource URI to a relative path under `/mnt/mcp/<server>/`. Strips a `<scheme>://`
/// (or `<scheme>:`) prefix and any leading slashes, leaving the path-like remainder (e.g.
/// `file:///repo/README.md` → `repo/README.md`, `github://repo/src/main.rs` → `repo/src/main.rs`).
/// Query/fragment are dropped. The caller path-confines the result with `mcp_safe_join`.
fn mcp_resource_rel_path(uri: &str) -> String {
    // Drop the scheme.
    let after_scheme = match uri.split_once("://") {
        Some((_scheme, rest)) => rest,
        None => match uri.split_once(':') {
            Some((_scheme, rest)) => rest,
            None => uri,
        },
    };
    // Drop query/fragment.
    let path = after_scheme.split(['?', '#']).next().unwrap_or(after_scheme);
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        "resource".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The leading command word of a top-level (operator-free) `line`, with any `sudo` prefix stripped —
/// for matching against installed grease prompts. `None` for a nested line (operators present).
fn prompt_leading_word(line: &str) -> Option<String> {
    let words = crate::ai::ask::dequote_words(line)?;
    let first = words.first()?;
    if first == "sudo" {
        words.get(1).cloned()
    } else {
        Some(first.clone())
    }
}

/// Parse an installed-package invocation line (`<name> --key value … [--model id]`) into its
/// `(name, provided-args, model-override)`. Shared by `run_prompt` and `run_script`. The line is NOT
/// `sudo`-prefixed here (the caller reaches this after the authz gate strips sudo). Returns a
/// pre-built exit-2 `LineResult` on a parse error or a `--key` missing its value.
#[allow(clippy::type_complexity)]
fn parse_pkg_invocation(
    line: &str,
) -> Result<(String, Vec<(String, String)>, Option<String>), LineResult> {
    let words = crate::ai::ask::dequote_words(line)
        .ok_or_else(|| LineResult::from_outcome(Vec::new(), b"grease: parse error\n".to_vec(), 2))?;
    let name = words[0].clone();
    let mut provided: Vec<(String, String)> = Vec::new();
    let mut model_override: Option<String> = None;
    let mut iter = words[1..].iter();
    while let Some(w) = iter.next() {
        if let Some(key) = w.strip_prefix("--") {
            let Some(val) = iter.next() else {
                return Err(LineResult::from_outcome(
                    Vec::new(),
                    format!("{name}: --{key} needs a value\n").into_bytes(),
                    2,
                ));
            };
            if key == "model" {
                model_override = Some(val.clone());
            } else {
                provided.push((key.to_string(), val.clone()));
            }
        }
        // Bare positional words are ignored in v1 (args are named).
    }
    Ok((name, provided, model_override))
}

/// The parsed shape of an agent-executable line (everything after the command name).
struct ParsedAgentLine {
    constructor: Vec<(String, String)>,
    method: String,
    args: Vec<(String, String)>,
    mode: crate::golem::agent::InvokeMode,
    phantom: Option<String>,
    /// `--revision <n>` if given (honest-stubbed — no wasm-rpc slot).
    revision: Option<String>,
}

/// Parse an agent line: `[--<ctor> val] [<wrapper-flags>] <method|subcommand> [--] [--<arg> val]`.
/// Wrapper flags (`--trigger`/`--schedule <iso>`/`--phantom <uuid>`/`--revision <n>`) are recognized
/// before the method (README:823); a `--<flag>` matching a declared constructor param is a ctor flag;
/// the first bare word is the method (or a reserved subcommand). An explicit `--` separates the method
/// from its args.
fn parse_agent_line(
    words: &[String],
    pkg: &crate::grease::pkg::AgentPackage,
) -> Result<ParsedAgentLine, String> {
    use crate::golem::agent::InvokeMode;
    let is_ctor = |k: &str| pkg.constructor_params.iter().any(|p| p == k);
    let mut constructor = Vec::new();
    let mut method = String::new();
    let mut args = Vec::new();
    let mut mode = InvokeMode::Await;
    let mut phantom = None;
    let mut revision = None;
    let mut i = 0;
    // Phase 1: wrapper flags + constructor flags + the method word.
    while i < words.len() {
        let w = &words[i];
        if w == "--" {
            i += 1;
            break;
        }
        if let Some(key) = w.strip_prefix("--") {
            if method.is_empty() {
                // Wrapper flags first (reserved; always before the method).
                match key {
                    "trigger" => {
                        mode = InvokeMode::Trigger;
                        i += 1;
                        continue;
                    }
                    "schedule" => {
                        let val = words.get(i + 1).ok_or("--schedule needs an ISO-8601 time\n")?;
                        mode = InvokeMode::Schedule(val.clone());
                        i += 2;
                        continue;
                    }
                    "phantom" => {
                        let val = words.get(i + 1).ok_or("--phantom needs a UUID\n")?;
                        phantom = Some(val.clone());
                        i += 2;
                        continue;
                    }
                    "revision" => {
                        let val = words.get(i + 1).ok_or("--revision needs a number\n")?;
                        revision = Some(val.clone());
                        i += 2;
                        continue;
                    }
                    _ if is_ctor(key) => {
                        let val = words.get(i + 1).ok_or_else(|| format!("--{key} needs a value\n"))?;
                        constructor.push((key.to_string(), val.clone()));
                        i += 2;
                        continue;
                    }
                    _ => return Err(format!("unknown flag --{key} before the method\n")),
                }
            }
            // After the method: a method arg.
            let val = words.get(i + 1).ok_or_else(|| format!("--{key} needs a value\n"))?;
            args.push((key.to_string(), val.clone()));
            i += 2;
            continue;
        }
        // A bare word: the method (first).
        if method.is_empty() {
            method = w.clone();
            i += 1;
            break;
        }
        i += 1;
    }
    // Phase 2: remaining words are method args.
    while i < words.len() {
        let w = &words[i];
        if w == "--" {
            i += 1;
            continue;
        }
        if let Some(key) = w.strip_prefix("--") {
            let val = words.get(i + 1).ok_or_else(|| format!("--{key} needs a value\n"))?;
            args.push((key.to_string(), val.clone()));
            i += 2;
        } else {
            i += 1;
        }
    }
    Ok(ParsedAgentLine { constructor, method, args, mode, phantom, revision })
}

/// Persist an install marker to `<etc>/<name>.toml`. Returns a user-facing error string on failure.
fn write_install_marker(name: &str, marker: &crate::grease::state::InstallMarker) -> Result<(), String> {
    let marker_toml = toml::to_string_pretty(marker)
        .map_err(|e| format!("grease install: marker serialize error: {e}\n"))?;
    let etc = crate::grease::config::etc_dir();
    let _ = std::fs::create_dir_all(&etc);
    std::fs::write(etc.join(format!("{name}.toml")), marker_toml)
        .map_err(|e| format!("grease install: cannot write marker: {e}\n"))
}

/// `grease info <skill>` text: the skill is not a command, so we describe its envelope + the bundled
/// documents/scripts rather than generated command help.
fn skill_info_text(sk: &crate::grease::pkg::SkillPackage) -> String {
    let mut out = format!("{} — {} [skill]\n", sk.name, sk.description);
    if let Some(use_) = &sk.intended_use {
        out.push_str(&format!("\nIntended use: {use_}\n"));
    }
    if !sk.documents.is_empty() {
        out.push_str("\nDocuments (under /usr/share/skills/");
        out.push_str(&sk.name);
        out.push_str("/):\n");
        for d in &sk.documents {
            out.push_str(&format!("  {}\n", d.path));
        }
    }
    if !sk.scripts.is_empty() {
        out.push_str("\nBundled scripts (on $PATH via /usr/share/skills/");
        out.push_str(&sk.name);
        out.push_str("/bin/):\n");
        for s in &sk.scripts {
            out.push_str(&format!("  {}\n", s.name));
        }
    }
    out.push_str(
        "\nA skill is a capability-context package, not a command; it is surfaced to the model \
         when you run `ask`.\n",
    );
    out
}

/// `grease info <mcp-server>` text: the server endpoint, exposed artifact types, and the cached
/// tool/prompt listings.
fn mcp_info_text(m: &crate::grease::pkg::McpPackage) -> String {
    let mut out = format!("{} — {} [mcp]\n", m.name, m.description);
    out.push_str(&format!("\nServer: {}\n", m.url));
    let mut kinds = Vec::new();
    if m.artifacts.tools {
        kinds.push("tools");
    }
    if m.artifacts.prompts {
        kinds.push("prompts");
    }
    if m.artifacts.resources {
        kinds.push("resources");
    }
    out.push_str(&format!("Artifacts: {}\n", kinds.join(", ")));
    if !m.tools.is_empty() {
        out.push_str(&format!("\nTools (run as `{} <tool>`):\n", m.name));
        for t in &m.tools {
            out.push_str(&format!("  {} — {}\n", t.name, t.description));
        }
    }
    if !m.prompts.is_empty() {
        out.push_str("\nPrompts (installed as $PATH commands):\n");
        for p in &m.prompts {
            out.push_str(&format!("  {} — {}\n", p.name, p.description));
        }
    }
    out
}

/// Whether `line` is a top-level `context summarize` (optionally `sudo`-prefixed) — the one context
/// subcommand that needs the async LLM layer. False for any line with shell operators (`|&;<>` `$`),
/// so `$(context summarize)` / `context summarize | …` fall through to Brush and hit the honest error
/// in `apply_context` (the LLM can't run in Brush's nested runtime — the "Wall C" wall). Matches the
/// operator-bail in [`crate::dispatch_context`].
fn is_context_summarize(line: &str) -> bool {
    if line.chars().any(|c| "|&;<>`$".contains(c)) {
        return false;
    }
    let effective = strip_sudo_prefix(line);
    let mut words = effective.split_whitespace();
    words.next() == Some("context") && words.next() == Some("summarize") && words.next().is_none()
}

/// Whether `line` is a single simple command with no shell operators (pipes, redirects, lists,
/// command/parameter substitution, or background `&`). Quote-aware — a shell metacharacter *inside*
/// a quoted word (e.g. `export --secret K="a|b"`) does not disqualify the line, since Brush's
/// tokenizer classifies it as part of a `Word`, not an `Operator`. Used to scope `export --secret`
/// interception to plain top-level lines; anything with operators falls through to Brush.
fn is_plain_line(line: &str) -> bool {
    match brush_parser::tokenize_str(line) {
        Ok(tokens) => !tokens
            .iter()
            .any(|t| matches!(t, brush_parser::Token::Operator(_, _))),
        // A line that doesn't tokenize isn't a clean simple command — let Brush produce the error.
        Err(_) => false,
    }
}

/// Reconstruct a top-level `ask` command line from parsed [`AskArgs`], for deferring an ask-tail
/// pipeline's confirmation (the deferred path re-runs a line string). Flags come first, then the
/// single-quoted prompt. The captured stdin travels separately via `next_ask_stdin`, so it is NOT
/// embedded here. Single quotes in the prompt are escaped bash-style (`'\''`).
fn ask_reconstruct(args: &crate::ai::ask::AskArgs) -> String {
    let mut line = String::from("ask");
    if args.fresh {
        line.push_str(" --fresh");
    }
    if args.json {
        line.push_str(" --json");
    }
    if let Some(m) = &args.model {
        line.push_str(&format!(" --model {m}"));
    }
    let escaped = args.prompt.replace('\'', r"'\''");
    line.push_str(&format!(" '{escaped}'"));
    line
}

/// The most tool-calling turns the agentic `ask` loop will drive before giving up. Bounds runaway
/// tool use; the loop exits 0 with whatever text it has plus a stderr notice on hitting the cap.
const ASK_MAX_ITERATIONS: usize = 16;

/// Per-stream byte cap on a tool result fed back to the model. Bounds context growth from a `cat` of a
/// large file; the payload is truncated with a marker, the JSON envelope is not.
const ASK_TOOL_RESULT_CAP: usize = 16 * 1024;

/// Truncate a tool-output stream to [`ASK_TOOL_RESULT_CAP`] bytes (on a UTF-8 boundary), appending a
/// marker when clipped. Returns a `String` (lossy) for JSON embedding.
fn truncate_tool_output(bytes: &[u8]) -> String {
    if bytes.len() <= ASK_TOOL_RESULT_CAP {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    // Lossy-decode the whole prefix, then clip to the cap on a char boundary of the resulting string.
    let decoded = String::from_utf8_lossy(bytes);
    let mut end = ASK_TOOL_RESULT_CAP.min(decoded.len());
    while end > 0 && !decoded.is_char_boundary(end) {
        end -= 1;
    }
    let mut s = decoded[..end].to_string();
    s.push_str("…[truncated]");
    s
}

/// Build an MCP `tools/call` arguments object from `--flag value` pairs, coercing each value per the
/// tool's JSON inputSchema (integer/number → number, boolean → bool, array/object → parsed JSON, else
/// string). Bare flags (`--verbose`) become `true`. Errors if a required property is missing.
fn build_mcp_arguments(
    schema: &serde_json::Value,
    flags: &[(String, Option<String>)],
) -> Result<serde_json::Value, String> {
    use serde_json::Value;
    let props = schema.get("properties").and_then(Value::as_object);
    let mut obj = serde_json::Map::new();
    for (key, value) in flags {
        let ty = props
            .and_then(|p| p.get(key))
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str);
        let coerced = match (ty, value) {
            (Some("boolean"), None) => Value::Bool(true),
            (Some("boolean"), Some(v)) => Value::Bool(v == "true" || v == "1" || v == "yes"),
            (Some("integer"), Some(v)) | (Some("number"), Some(v)) => v
                .parse::<f64>()
                .map(|n| serde_json::json!(n))
                .map_err(|_| format!("--{key}: '{v}' is not a number"))?,
            (Some("array"), Some(v)) | (Some("object"), Some(v)) => serde_json::from_str(v)
                .map_err(|e| format!("--{key}: expected JSON: {e}"))?,
            (_, Some(v)) => Value::String(v.clone()),
            // A bare flag with no schema type: treat as a present boolean.
            (_, None) => Value::Bool(true),
        };
        obj.insert(key.clone(), coerced);
    }
    // Check required properties are present.
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        let missing: Vec<String> = required
            .iter()
            .filter_map(Value::as_str)
            .filter(|r| !obj.contains_key(*r))
            .map(String::from)
            .collect();
        if !missing.is_empty() {
            return Err(format!("missing required argument(s): {}", missing.join(", ")));
        }
    }
    Ok(Value::Object(obj))
}

/// The README's default `$PATH` — the resolution namespace clank's package layout installs into.
/// Nothing populates `/usr/lib/{mcp,agents,prompts}/bin` or the skills glob yet (that's `grease`,
/// future); these entries currently resolve to nothing, which is correct — `type`/`which` degrade
/// to "not found" rather than erroring on a missing directory.
const DEFAULT_PATH: &str =
    "/usr/local/bin:/usr/bin:/usr/lib/mcp/bin:/usr/lib/agents/bin:/usr/lib/prompts/bin:/usr/share/skills/*/bin";

/// The README's home directory. Seeded as `$HOME` on the agent (empty env) so `~` expansion and
/// `~/.config/ask/ask.toml` resolve; native keeps the host's real `$HOME`.
const DEFAULT_HOME: &str = "/home/user";

async fn build_shell() -> Result<Shell, brush_core::Error> {
    // NB: clank's builtins are registered here AND their manifests in `registry::build()`; the two
    // must stay in lockstep (the registry drift-guard test enforces it). Adding a builtin via
    // `Shell::register_builtin` directly would bypass the manifest — don't.
    let mut shell = Shell::builder()
        .default_builtins(BuiltinSet::BashMode)
        .builtins(crate::tools::coreutils::builtins())
        .builtins(crate::tools::texttools::builtins())
        .builtins(crate::runtime::ps::builtins())
        .builtins(crate::tools::which::builtins())
        .builtins(crate::tools::man::builtins())
        .builtins(crate::tools::stat::builtins())
        .builtins(crate::tools::find::builtins())
        .builtins(crate::tools::xargs::builtins())
        .builtins(crate::ai::model::builtins())
        .builtins(crate::builtins::context::builtins())
        .builtins(crate::builtins::interceptstub::builtins())
        .build()
        .await?;

    // Set clank's `$PATH` explicitly, overriding whatever Brush's init seeded (empty on the wasm
    // stub, the host's real PATH on native — both wrong for clank's virtual namespace). Read by
    // `$PATH` expansion and by `type`/`which` path resolution alike.
    shell.env_mut().set_global(
        "PATH",
        brush_core::variables::ShellVariable::new(DEFAULT_PATH),
    )?;

    // Seed `$HOME` to the README layout (`/home/user`) only when unset — the agent's wasm env is
    // empty, so `~` expansion and `~/.config/ask/ask.toml` need it; native keeps the host's real
    // `$HOME` (ask.toml is a native location too, per the README).
    if shell.env().get("HOME").is_none() {
        shell.env_mut().set_global(
            "HOME",
            brush_core::variables::ShellVariable::new(DEFAULT_HOME),
        )?;
    }

    Ok(shell)
}

/// Map a Brush result to line output, appending any shell error message to stderr.
fn finish(
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
