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
use crate::process::ProcessKind;
use crate::builtins::promptuser::{AnswerInput, PendingPrompt, Resolution};
use crate::proctable::ProcessTable;
use crate::registry::CommandRegistry;

type BoxError = Box<dyn std::error::Error>;

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
    mcpfs: std::sync::Arc<Vec<crate::mcpfs::ResourceEntry>>,
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
            return LineResult::stderr(
                "clank: a prompt-user question is awaiting a response; answer it first\n",
            );
        }

        self.transcript.lock().unwrap().record_command(line);

        // Reap finished background jobs (a tick-free poll of Brush's job manager): their rows flip
        // `S → Z`. Silent — bash-style "[1]+ Done" notifications are a later increment; `jobs`/`ps`
        // reflect the state.
        self.reap_bg_jobs();

        // Install this session's process table as the active one for the duration of the line, so
        // the `ps` builtin (a Brush builtin, which can't reach `Session` directly) can read it.
        // The guard clears the slot on drop. The transcript slot is the same pattern, read by the
        // Brush-registered `context` builtin in nested contexts ($(context show), context | head).
        let _install = crate::proctable::install(self.proc_table.clone());
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
        let _install_dynreg = crate::dynreg::install(dynreg);
        let _install_mcpfs = crate::mcpfs::install(mcpfs);
        let _install_sysprompt = crate::sysprompt::install(sysprompt);

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
        let ppid = line_pid.unwrap_or(crate::proctable::SHELL_ROOT_PID);
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
                .spawn_bg(crate::process::ProcessKind::Builtin, argv, ppid);
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
                    if *pid == crate::proctable::SHELL_ROOT_PID {
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

    /// Run the agentic `ask` loop: assemble the transcript-as-context first user message, then drive
    /// turns with the injected provider. In A1 the tool set is empty, so the model always answers in
    /// one turn (behavior-identical to the pre-loop `ask`); A2 adds the `shell` tool and makes this a
    /// real tool-calling loop. If no provider is installed (e.g. the native build), degrade to a clean
    /// "not configured" error (exit 4) rather than panicking — the README's "features that require
    /// Golem fail with informative errors."
    ///
    /// Takes `&mut self` because a tool call re-enters `run_command`. The provider is `take()`n for the
    /// duration and **restored before every return** — an early return that forgets to restore would
    /// silently break the next `ask`.
    ///
    /// `blanket_authorized` is the `sudo ask` (or session `allow_all`) grant: when set, the model's
    /// tool calls that hit a `confirm`-policy command run without a per-call refusal. It never
    /// satisfies `sudo-only` (destructive ops still refuse), matching [`authz::decide`].
    async fn run_ask(
        &mut self,
        mut args: crate::ai::ask::AskArgs,
        blanket_authorized: bool,
    ) -> LineResult {
        use crate::ai::ask::AskTurn;

        // Pick up any pipeline stdin captured for this dispatch (`cat x | ask "…"`). Taken so it
        // never leaks into an unrelated later `ask`.
        if let Some(stdin) = self.next_ask_stdin.take() {
            args.stdin = Some(stdin);
        }

        if self.ask_provider.is_none() {
            return LineResult::from_outcome(
                Vec::new(),
                b"ask: no model provider configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        }

        // The base context is the same bytes `context show` renders — "the AI reads exactly what you
        // see." `--fresh` sends no transcript. Rendered to an owned String *before* the loop so the
        // transcript lock is never held across an await.
        let base_transcript = if args.fresh {
            String::new()
        } else {
            String::from_utf8_lossy(&self.transcript.lock().unwrap().render()).into_owned()
        };

        // Resolve the model: `--model` > ask.toml default > built-in DEFAULT_MODEL. Strip the
        // `anthropic/` prefix for the provider (it wants the bare id); an unknown provider prefix is
        // an error before any model call.
        let (model, model_warning) = match self.resolve_ask_model(args.model.as_deref()) {
            Ok(pair) => pair,
            Err(msg) => return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 2),
        };

        let mut trace = Vec::new();
        if let Some(w) = model_warning {
            trace.extend_from_slice(w.as_bytes());
        }

        // The tool surface = the generic shell/prompt_user tools, plus one ToolDefinition per installed
        // MCP tool (mcp__<server>__<tool>) and per installed grease prompt (prompt__<name>). Every
        // `grease install`/`mcp add` thus expands what `ask` can do (README).
        let mut tools = crate::ai::ask::build_ask_tools(&self.registry);
        tools.extend(self.mcp.ask_tool_definitions());
        tools.extend(self.grease.ask_tool_definitions());

        let system = crate::ai::ask::with_json_addendum(
            crate::ai::ask::build_system_prompt_with_capabilities(
                &self.registry,
                &self.mcp,
                &self.grease,
            ),
            args.json,
        );
        let state = AskLoopState {
            system,
            tools,
            history: vec![AskTurn::User(crate::ai::ask::user_content_with_stdin(
                &base_transcript,
                &args.prompt,
                args.stdin.as_deref(),
            ))],
            model,
            trace,
            blanket_authorized,
            json: args.json,
        };
        // `resume` is empty on a fresh loop; the pid carries the paused row on a resume.
        self.drive_ask_loop(state, Vec::new(), None).await
    }

    /// Handle an ask-tail pipeline (`… | ask "q"`): run the upstream, capture its stdout, and
    /// dispatch the `ask` tail at the session layer with those bytes as stdin. `ask` is Confirm-gated
    /// (outbound HTTP), so a bare tail surfaces a confirmation (carrying the captured stdin so it
    /// survives the pause); `… | sudo ask` pre-authorizes. The upstream runs through the normal
    /// `execute` capture path — its own commands carry whatever policy they have (`cat`/`grep` are
    /// Allow); this gates only the `ask` itself.
    async fn run_ask_pipe(
        &mut self,
        pipe: crate::ai::ask::AskTailPipe,
        pid: Option<u32>,
    ) -> LineResult {
        let crate::ai::ask::AskTailPipe {
            upstream,
            args,
            elevated,
        } = pipe;

        // Authorize the ask tail FIRST (before running the upstream), so a denied ask doesn't run the
        // producer for nothing and matches the top-level `ask` gate. `ask` ⇒ Confirm; a `sudo` on the
        // tail (`elevated`) or a session "all" grant pre-authorizes.
        let policy = crate::manifest::AuthorizationPolicy::Confirm; // the `ask` manifest's policy
        let blanket = elevated || self.authz.allow_all;
        match authz::decide(policy, elevated, self.authz.allow_all) {
            Decision::Allow => {}
            Decision::Deny => {
                return self.finish_intercepted(pid, LineResult::denied());
            }
            Decision::Confirm { sudo_grant } => {
                // Capture the upstream now so the piped context is preserved across the pause, then
                // defer the ask with that stdin stashed on the pending confirmation.
                let captured = self.capture_upstream(&upstream).await;
                return self.surface_auth_confirm(
                    Some("ask"),
                    ask_reconstruct(&args),
                    pid,
                    sudo_grant,
                    Some(captured),
                );
            }
        }

        // Approved (allow / sudo / all): run the upstream, capture, and dispatch the ask with stdin.
        let captured = self.capture_upstream(&upstream).await;
        self.next_ask_stdin = Some(captured);
        let result = self.run_ask(args, blanket).await;
        if let Some(pid) = pid {
            self.proc_table.lock().unwrap().complete(pid);
        }
        self.transcript.lock().unwrap().record_output(&result.terminal_output());
        result
    }

    /// Run an upstream pipeline stage and return its stdout as a lossy String (the stdin payload for
    /// an ask-tail pipeline). A nonzero exit is not fatal — pipes feed whatever the producer emitted.
    /// The upstream is not recorded as its own transcript line (the whole `… | ask` line is recorded
    /// by the caller).
    async fn capture_upstream(&mut self, upstream: &str) -> String {
        let result = self.execute(upstream).await;
        String::from_utf8_lossy(&result.stdout).into_owned()
    }

    // ---- ask repl (native-only interactive session with its own transcript) --------------------

    /// Start an `ask repl` session: resolve the model, seed the isolated transcript (empty for
    /// `--fresh`, a copy of the parent for `--inherit`), and stash it on `self.repl`. Returns the
    /// resolved model id for the prompt banner, or an `Err` message (unknown provider / no provider).
    /// Native-only — the durable agent returns an honest message from `eval_line` instead.
    pub fn repl_start(&mut self, args: &crate::ai::ask::ReplArgs) -> Result<String, String> {
        if self.ask_provider.is_none() {
            return Err("ask repl: no model provider configured\n".to_string());
        }
        let (model, _warning) = self.resolve_ask_model(args.model.as_deref())?;
        let transcript = match args.seed {
            crate::ai::ask::ReplSeed::Fresh => Transcript::new(),
            crate::ai::ask::ReplSeed::Inherit => self.transcript.lock().unwrap().clone(),
        };
        self.repl = Some(ReplState { transcript, model });
        Ok(self.repl.as_ref().unwrap().model.clone())
    }

    /// The active REPL's model id, for the `[model]>` prompt. `None` if no REPL is active.
    pub fn repl_model(&self) -> Option<String> {
        self.repl.as_ref().map(|r| r.model.clone())
    }

    /// Handle a REPL meta-command (`:model <id>`, `:new-session`, `:exit`). Returns `Some(output)`
    /// for a handled meta-command (the bool is whether the REPL should exit), or `None` if `line`
    /// isn't a meta-command (the caller then treats it as a prompt via [`Self::repl_turn`]).
    pub fn repl_meta(&mut self, line: &str) -> Option<(String, bool)> {
        let line = line.trim();
        let Some(repl) = self.repl.as_mut() else {
            return None;
        };
        let mut words = line.split_whitespace();
        match words.next()? {
            ":exit" | ":quit" => Some((String::new(), true)),
            ":new-session" => {
                repl.transcript = Transcript::new();
                Some(("(new session)\n".to_string(), false))
            }
            ":model" => match words.next() {
                Some(id) => {
                    // Accept `anthropic/…` or a bare id; store as given (resolved per-turn).
                    repl.model = id.strip_prefix("anthropic/").unwrap_or(id).to_string();
                    Some((format!("model set to {}\n", repl.model), false))
                }
                None => Some((format!("current model: {}\n", repl.model), false)),
            },
            other if other.starts_with(':') => {
                Some((format!("unknown REPL command: {other}\n"), false))
            }
            _ => None, // not a meta-command — it's a prompt
        }
    }

    /// Run one REPL turn: send the isolated transcript + `prompt` to the model, record the exchange
    /// into the REPL transcript, and return the reply text. Conversational only (no shell tools) —
    /// the REPL is a chat surface, distinct from agentic `ask`. Requires an active REPL.
    pub async fn repl_turn(&mut self, prompt: &str) -> String {
        use crate::ai::ask::AskTurn;
        let Some(repl) = self.repl.as_ref() else {
            return "ask repl: no active session\n".to_string();
        };
        let model = repl.model.clone();
        let context = String::from_utf8_lossy(&repl.transcript.render()).into_owned();

        let state = AskLoopState {
            system: crate::ai::ask::build_system_prompt_with_mcp(&self.registry, &self.mcp),
            tools: Vec::new(), // conversational: the REPL doesn't expose shell tools
            history: vec![AskTurn::User(crate::ai::ask::user_content(&context, prompt))],
            model,
            trace: Vec::new(),
            blanket_authorized: false,
            json: false,
        };
        let result = self.drive_ask_loop(state, Vec::new(), None).await;
        let reply = String::from_utf8_lossy(&result.stdout).into_owned();

        // Record the exchange into the REPL's own transcript so the next turn has context.
        if let Some(repl) = self.repl.as_mut() {
            repl.transcript.record_command(&format!("> {prompt}"));
            repl.transcript.record_output(reply.as_bytes());
        }
        reply
    }

    /// End the REPL session: render its transcript to stdout (so it enters the parent transcript once
    /// as rendered output, per the README) and clear `self.repl`. Returns the rendered session bytes.
    pub fn repl_end(&mut self) -> Vec<u8> {
        match self.repl.take() {
            Some(repl) => repl.transcript.render(),
            None => Vec::new(),
        }
    }

    // ---- context summarize (LLM one-shot; inspection only, never mutates the transcript) ---------

    /// `context summarize`: send the rendered transcript to the model and print an AI summary on
    /// stdout. A single tool-less provider `turn` (the [`Self::repl_turn`] shape, not the agentic
    /// `drive_ask_loop`). **Inspection only** — it does NOT mutate the transcript, and the caller
    /// must NOT record its output back (matching `context show`). Runs at the Session async layer
    /// (like `ask`), so it's only reachable from a top-level `context summarize` line — a nested one
    /// (`$(...)`/pipe) hits the honest error in `apply_context`.
    async fn run_context_summarize(&mut self) -> LineResult {
        // Render before awaiting so the transcript lock is never held across the model call.
        let rendered = String::from_utf8_lossy(&self.transcript.lock().unwrap().render()).into_owned();
        if rendered.trim().is_empty() {
            return LineResult::from_outcome(b"(transcript is empty)\n".to_vec(), Vec::new(), 0);
        }

        let (model, _warning) = match self.resolve_ask_model(None) {
            Ok(pair) => pair,
            Err(msg) => return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 2),
        };

        match self.summarize_text(&rendered, &model).await {
            Ok(None) => LineResult::from_outcome(
                Vec::new(),
                b"context summarize: no model provider configured (available on the Golem agent)\n"
                    .to_vec(),
                4,
            ),
            Ok(Some(summary)) => {
                let mut text = summary.into_bytes();
                if !text.ends_with(b"\n") {
                    text.push(b'\n');
                }
                LineResult::from_outcome(text, Vec::new(), 0)
            }
            Err(err) => LineResult::from_outcome(Vec::new(), err.into_bytes(), 4),
        }
    }

    /// One tool-less provider `turn` that summarizes `text` and returns the model's reply. `Ok(None)`
    /// when no provider is configured (native), `Err` on a model error. The shared mechanic behind both
    /// `context summarize` and the auto-compaction step: render/text is prepared by the caller (so the
    /// transcript lock is never held across the await), the provider is `take()`n and restored, and a
    /// single `SUMMARIZE_SYSTEM_PROMPT` turn is sent with no tools.
    async fn summarize_text(&mut self, text: &str, model: &str) -> Result<Option<String>, String> {
        use crate::ai::ask::AskTurn;

        let Some(provider) = self.ask_provider.take() else {
            return Ok(None);
        };
        let resp = provider
            .turn(
                Some(crate::ai::ask::SUMMARIZE_SYSTEM_PROMPT),
                &[AskTurn::User(text.to_string())],
                &[],
                model,
            )
            .await;
        self.ask_provider = Some(provider); // restore before returning

        match resp.error {
            Some(err) => Err(err),
            None => Ok(Some(resp.text)),
        }
    }

    /// The auto-compaction step: after a line's output is recorded and the window may have evicted old
    /// entries, upgrade the leading `[N earlier entries dropped]` count marker into a model-generated
    /// summary block. Runs at most once per line, only when eviction actually happened AND a provider is
    /// configured (agent, or a Fake in tests). On native / no provider / model error, the count marker is
    /// left as-is — the decided fallback, so recording never blocks or fails.
    async fn compact_dropped_span(&mut self) {
        // Snapshot the pending dropped span without holding the lock across the await.
        let pending = self.transcript.lock().unwrap().pending_summary();
        let Some((_count, dropped_text)) = pending else {
            return;
        };
        if dropped_text.trim().is_empty() {
            return;
        }
        let model = match self.resolve_ask_model(None) {
            Ok((model, _warning)) => model,
            Err(_) => return, // no valid model → keep the count marker
        };
        if let Ok(Some(summary)) = self.summarize_text(&dropped_text, &model).await {
            self.transcript.lock().unwrap().set_marker_summary(summary);
        }
    }

    /// Resolve the model id `ask` should target: `--model` (if given) > the ask.toml default > the
    /// built-in [`crate::ai::ask::DEFAULT_MODEL`]. Returns `(bare_model_id, optional_warning)` — the
    /// `anthropic/` prefix is stripped for the provider. An unknown `provider/` prefix is an `Err`
    /// (surfaced before any model call). An ask.toml parse error is a non-fatal warning that falls
    /// back to the built-in default.
    fn resolve_ask_model(&self, cli_model: Option<&str>) -> Result<(String, Option<String>), String> {
        let mut warning = None;
        let chosen = if let Some(m) = cli_model {
            m.to_string()
        } else {
            let home = self.shell_home();
            match crate::ai::config::default_model(&home) {
                Ok(Some(m)) => m,
                Ok(None) => crate::ai::ask::DEFAULT_MODEL.to_string(),
                Err(e) => {
                    warning = Some(format!("ask: {e}; using the built-in default\n"));
                    crate::ai::ask::DEFAULT_MODEL.to_string()
                }
            }
        };

        // Validate/strip the provider prefix. Only `anthropic/` is known; a bare id is anthropic.
        let bare = match chosen.split_once('/') {
            Some(("anthropic", model)) => model.to_string(),
            Some((provider, _)) => {
                return Err(format!(
                    "ask: unknown provider '{provider}' (only anthropic is available)\n"
                ))
            }
            None => chosen,
        };
        Ok((bare, warning))
    }

    /// The shell's `$HOME` (seeded to `/home/user` on the agent), for locating `~/.config/ask/ask.toml`.
    fn shell_home(&self) -> String {
        self.shell
            .env()
            .get_str("HOME", &self.shell)
            .map(|h| h.into_owned())
            .unwrap_or_else(|| DEFAULT_HOME.to_string())
    }

    /// Resolve a line's authorization policy, consulting the static registry AND the dynamic MCP
    /// server manifests. Mirrors [`authz::resolve`] but adds the MCP layer: an installed server name
    /// (leading command) resolves to its `Confirm` manifest. Returns `(policy, elevated, command)`.
    fn resolve_authz(
        &self,
        line: &str,
    ) -> (crate::manifest::AuthorizationPolicy, bool, Option<String>) {
        let (command, elevated) = authz::leading_command(line);
        if let Some(name) = command.as_deref() {
            if self.registry.get(name).is_none() {
                if let Some(m) = self.mcp.manifest_for(name) {
                    return (m.authorization_policy, elevated, command);
                }
                // An installed grease prompt: running it is an outbound LLM call ⇒ Confirm.
                if let Some(m) = self.grease.manifest_for(name) {
                    return (m.authorization_policy, elevated, command);
                }
            }
        }
        authz::resolve(&self.registry, line)
    }

    /// Generated help for an MCP tool line ending in `--help` (or a bare `<server>`): the server's
    /// tool list. `None` if the line isn't an installed-server line or doesn't request help.
    fn mcp_help_for(&self, line: &str) -> Option<String> {
        let inv = match crate::mcp::cmd::parse_tool_invocation(line)? {
            Ok(inv) => inv,
            Err(_) => return None,
        };
        if !self.mcp.is_server(&inv.server) {
            return None;
        }
        // Bare `<server>` or `--help` ⇒ server help; a `<tool> --help` ⇒ the same (tool-level help is
        // the server help in MCP-lite).
        if inv.help || inv.tool.is_none() {
            return self.mcp.server_help(&inv.server);
        }
        None
    }

    /// Drive the `ask` agentic loop from `state` until it completes (model answers, transport error,
    /// or the cap) or pauses for the human. `pending_results` are results already accumulated for the
    /// *current* assistant turn (non-empty only on resume, after a pause mid-batch); `pid` is the
    /// process-table row to pause on surfacing (the ask's own row, threaded through on resume).
    ///
    /// The provider is `take()`n and restored before every return. On a pause, the whole `state` is
    /// stashed in `PendingKind::AgentLoop` and a `pending_prompt` is returned; `answer_prompt` calls
    /// back into this helper to continue.
    async fn drive_ask_loop(
        &mut self,
        mut state: AskLoopState,
        mut pending_results: Vec<crate::ai::ask::AskToolResult>,
        pid: Option<u32>,
    ) -> LineResult {
        use crate::ai::ask::AskTurn;

        let Some(mut provider) = self.ask_provider.take() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"ask: no model provider configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };

        // Tool traces accumulate on `state.trace` (→ stderr); the final model text is stdout. Both land
        // in the transcript via the caller's `record_output` — AI tool lines are never first-class
        // transcript entries.
        let final_text;

        // Bound the number of *model* turns. Count assistant turns already in history so a resumed loop
        // doesn't restart the budget.
        let turns_taken = state
            .history
            .iter()
            .filter(|t| matches!(t, AskTurn::Assistant { .. }))
            .count();

        let mut turn = turns_taken;
        loop {
            if turn >= ASK_MAX_ITERATIONS {
                state.trace.extend_from_slice(
                    format!("[ask] tool-call limit ({ASK_MAX_ITERATIONS}) reached\n").as_bytes(),
                );
                self.ask_provider = Some(provider);
                if let Some(pid) = pid {
                    self.proc_table.lock().unwrap().complete(pid);
                }
                // Under `--json` a truncated loop can't have produced a validated JSON answer, so
                // honor the exit-6 contract rather than a bare exit 0.
                if state.json {
                    state.trace.extend_from_slice(
                        b"ask: --json: tool-call limit reached before a final JSON answer\n",
                    );
                    return LineResult::from_outcome(Vec::new(), state.trace, 6);
                }
                return LineResult::from_outcome(Vec::new(), state.trace, 0);
            }

            let resp = provider
                .turn(Some(&state.system), &state.history, &state.tools, &state.model)
                .await;

            if let Some(err) = resp.error {
                self.ask_provider = Some(provider);
                if let Some(pid) = pid {
                    self.proc_table.lock().unwrap().complete(pid);
                }
                let mut stderr = state.trace;
                stderr.extend_from_slice(err.as_bytes());
                return LineResult::from_outcome(Vec::new(), stderr, 4);
            }
            if resp.tool_calls.is_empty() {
                final_text = resp.text;
                break;
            }
            turn += 1;

            // Execute this turn's calls in order. A pause stashes the remaining calls + accumulated
            // results and returns immediately (non-blocking); `answer_prompt` resumes here.
            //
            // Restore the provider onto `self` for the duration of tool execution: a `prompt__<name>`
            // tool call re-enters `run_ask` (running the stored prompt through the model), which needs
            // the provider available. Re-take it after the batch, before the next `provider.turn`.
            self.ask_provider = Some(provider);
            let calls = resp.tool_calls.clone();
            let mut results = std::mem::take(&mut pending_results);
            for (i, call) in calls.iter().enumerate() {
                let step = Box::pin(self.execute_ask_tool(
                    call,
                    state.blanket_authorized,
                    &mut state.trace,
                ))
                .await;
                match step {
                    ToolStep::Done(r) => results.push(r),
                    ToolStep::Pause(kind) => {
                        // Record the assistant turn before pausing so the stashed history is complete.
                        state.history.push(AskTurn::Assistant {
                            text: resp.text.clone(),
                            tool_calls: calls.clone(),
                        });
                        let pause = AskPause {
                            call: call.clone(),
                            kind,
                            remaining: calls[i + 1..].to_vec(),
                            completed: results,
                        };
                        // Provider already restored on `self` above; leave it there for the resume.
                        return self.surface_agent_pause(state, pause, pid);
                    }
                }
            }

            state.history.push(AskTurn::Assistant {
                text: resp.text,
                tool_calls: calls,
            });
            state.history.push(AskTurn::ToolResults(results));

            // Re-take the provider for the next turn's `provider.turn` (it was restored on `self` for
            // tool execution above). A nested prompt tool call has already returned it to `self`.
            let Some(p) = self.ask_provider.take() else {
                if let Some(pid) = pid {
                    self.proc_table.lock().unwrap().complete(pid);
                }
                return LineResult::from_outcome(
                    Vec::new(),
                    b"ask: model provider went missing mid-loop\n".to_vec(),
                    4,
                );
            };
            provider = p;
        }

        self.ask_provider = Some(provider);
        if let Some(pid) = pid {
            self.proc_table.lock().unwrap().complete(pid);
        }

        // `--json`: enforce the output contract. Valid JSON (after stripping a stray code fence) ⇒
        // the JSON on stdout, exit 0, trace still on stderr. Otherwise exit 6 with the raw text on
        // stderr so it isn't lost (README: "raw model response emitted to stderr").
        if state.json {
            let candidate = crate::ai::ask::strip_json_fence(&final_text);
            match serde_json::from_str::<serde_json::Value>(candidate) {
                Ok(_) => {
                    return LineResult::from_outcome(
                        candidate.as_bytes().to_vec(),
                        state.trace,
                        0,
                    );
                }
                Err(_) => {
                    let mut stderr = state.trace;
                    stderr.extend_from_slice(b"ask: --json: model did not return valid JSON\n");
                    stderr.extend_from_slice(final_text.as_bytes());
                    if !final_text.ends_with('\n') {
                        stderr.push(b'\n');
                    }
                    return LineResult::from_outcome(Vec::new(), stderr, 6);
                }
            }
        }

        LineResult::from_outcome(final_text.into_bytes(), state.trace, 0)
    }

    /// Surface the human-facing prompt for a paused `ask` loop and stash the loop state. Returns a
    /// `pending_prompt` result; the shell does not block. For a `Confirm` pause the prompt is the
    /// README's authorization copy; for `prompt_user` it's the model's question.
    fn surface_agent_pause(
        &mut self,
        state: AskLoopState,
        pause: AskPause,
        pid: Option<u32>,
    ) -> LineResult {
        let prompt = match &pause.kind {
            AskPauseKind::Confirm {
                command,
                sudo_grant,
            } => {
                let name = authz::leading_command(command).0.unwrap_or_else(|| command.clone());
                let synopsis = format!("run `{command}`");
                PendingPrompt {
                    question: authz::confirm_question(&name, &synopsis, *sudo_grant),
                    choices: Some(authz::confirm_choices(*sudo_grant)),
                    secret: false,
                }
            }
            AskPauseKind::PromptUser => {
                // The model's question is the first user-facing text; re-derive it from the call args.
                let question = serde_json::from_str::<serde_json::Value>(&pause.call.arguments_json)
                    .ok()
                    .and_then(|v| v.get("question").and_then(|q| q.as_str()).map(String::from))
                    .unwrap_or_else(|| "the model has a question".to_string());
                PendingPrompt {
                    question,
                    choices: None,
                    secret: false,
                }
            }
        };
        self.surface_pending(
            prompt,
            pid,
            PendingKind::AgentLoop {
                state: Box::new(state),
                pause,
            },
        )
    }

    /// Resume a paused `ask` loop after the human answers. Resolves the paused tool call per its kind,
    /// drains any sibling calls from the same turn (each may pause again), then re-enters
    /// [`Session::drive_ask_loop`] to continue the conversation.
    async fn resolve_agent_loop(
        &mut self,
        resolution: Resolution,
        mut state: Box<AskLoopState>,
        pause: AskPause,
        pid: Option<u32>,
    ) -> LineResult {
        use crate::ai::ask::{AskToolResult, AskTurn};

        // An abort (Ctrl-C, or `kill <paused-pid>`) terminates the WHOLE ask, not just this tool call:
        // exit 130 with the trace so far. The paused row is reaped here.
        if matches!(resolution, Resolution::Aborted) {
            if let Some(pid) = pid {
                self.proc_table.lock().unwrap().complete(pid);
            }
            state.trace.extend_from_slice(b"[ask] aborted by user\n");
            return LineResult::from_outcome(Vec::new(), state.trace, 130);
        }

        let answer_text = match &resolution {
            Resolution::Answered { stdout, .. } => {
                String::from_utf8_lossy(stdout).trim().to_string()
            }
            Resolution::Aborted => unreachable!("handled above"),
            Resolution::InvalidChoice { .. } => unreachable!("handled by answer_prompt"),
        };

        // Resolve the paused call.
        let resolved: AskToolResult = match pause.kind {
            AskPauseKind::Confirm {
                command,
                sudo_grant,
            } => {
                let approved = matches!(answer_text.as_str(), "yes" | "all");
                if answer_text == "all" && !sudo_grant {
                    // Blanket confirm-tier authorization for the rest of THIS loop only (not the
                    // session `allow_all`). Sudo-only never gets an "all".
                    state.blanket_authorized = true;
                }
                if approved {
                    Box::pin(self.run_shell_tool(
                        &pause.call,
                        &command,
                        state.blanket_authorized,
                        &mut state.trace,
                    ))
                    .await
                } else {
                    state
                        .trace
                        .extend_from_slice(format!("[tool] $ {command}\n[tool] denied by user\n").as_bytes());
                    AskToolResult {
                        id: pause.call.id.clone(),
                        name: pause.call.name.clone(),
                        outcome: Err("denied by user".into()),
                    }
                }
            }
            AskPauseKind::PromptUser => {
                let payload = serde_json::json!({ "answer": answer_text });
                AskToolResult {
                    id: pause.call.id.clone(),
                    name: pause.call.name.clone(),
                    outcome: Ok(payload.to_string()),
                }
            }
        };

        let mut results = pause.completed;
        results.push(resolved);

        // Drain the sibling calls from the same turn; any may pause again (re-stashing AgentLoop).
        for (i, call) in pause.remaining.iter().enumerate() {
            let step = Box::pin(self.execute_ask_tool(
                call,
                state.blanket_authorized,
                &mut state.trace,
            ))
            .await;
            match step {
                ToolStep::Done(r) => results.push(r),
                ToolStep::Pause(kind) => {
                    let next = AskPause {
                        call: call.clone(),
                        kind,
                        remaining: pause.remaining[i + 1..].to_vec(),
                        completed: results,
                    };
                    return self.surface_agent_pause(*state, next, pid);
                }
            }
        }

        // All calls in the paused turn are resolved: append their results and continue the loop.
        state.history.push(AskTurn::ToolResults(results));
        self.drive_ask_loop(*state, Vec::new(), pid).await
    }

    /// Attempt one tool call from the agentic loop. Returns either a finished [`AskToolResult`] (the
    /// call ran, was refused by a guard, or malformed) or a [`ToolStep::Pause`] when the call needs the
    /// human — a `confirm`/`sudo-only` command awaiting authorization, or the `prompt_user` tool.
    ///
    /// Guards return `Done(Err(..))` (loop continues): malformed/unknown tool, `ask` recursion, and any
    /// `shell-internal`/`parent-shell` command (`context`/`kill`/`cd`/`export`/… mutate shell state a
    /// tool must not reach — closes the cwd/env leak vector). The authorization pre-check reuses
    /// [`authz`]: `Allow` executes, `Confirm` pauses. Approved lines run through `run_command` (full
    /// session surface — `curl`/`wget` work) with no proc row; a nonzero exit is a *successful* tool
    /// result carrying the code (the model must see failures).
    async fn execute_ask_tool(
        &mut self,
        call: &crate::ai::ask::AskToolCall,
        blanket_authorized: bool,
        trace: &mut Vec<u8>,
    ) -> ToolStep {
        use crate::ai::ask::AskToolResult;

        let done_err = |msg: String| {
            ToolStep::Done(AskToolResult {
                id: call.id.clone(),
                name: call.name.clone(),
                outcome: Err(msg),
            })
        };

        // The `prompt_user` tool: pause and ask the human directly; the answer becomes the result.
        if call.name == crate::ai::ask::PROMPT_USER_TOOL {
            let question = match serde_json::from_str::<serde_json::Value>(&call.arguments_json) {
                Ok(v) => match v.get("question").and_then(|q| q.as_str()) {
                    Some(s) => s.to_string(),
                    None => {
                        return done_err(
                            "malformed arguments: missing string field 'question'".into(),
                        )
                    }
                },
                Err(e) => return done_err(format!("malformed arguments: {e}")),
            };
            trace.extend_from_slice(format!("[tool] prompt_user: {question}\n").as_bytes());
            return ToolStep::Pause(AskPauseKind::PromptUser);
        }

        // An MCP tool (name `mcp__<server>__<tool>`): decode to a `<server> <tool> --args '<json>'`
        // command line and route it through the same shell-tool machinery below (so its Confirm authz
        // pauses under a plain ask, or runs under `sudo ask`). The arguments_json is validated so the
        // `--args` payload is always well-formed JSON.
        let command = if let Some(rest) = call.name.strip_prefix("mcp__") {
            let Some((server, tool)) = rest.split_once("__") else {
                return done_err(format!("malformed MCP tool name '{}'", call.name));
            };
            // The whole arguments object is the tool's arguments (validate it's an object).
            let args_str = if call.arguments_json.trim().is_empty() {
                "{}".to_string()
            } else {
                match serde_json::from_str::<serde_json::Value>(&call.arguments_json) {
                    Ok(serde_json::Value::Object(_)) => call.arguments_json.clone(),
                    Ok(_) => return done_err("MCP tool arguments must be a JSON object".into()),
                    Err(e) => return done_err(format!("malformed arguments: {e}")),
                }
            };
            // Single-quote the JSON so the shell tokenizer preserves it (inner double-quotes survive
            // via the outer-quote-only handling in parse_tool_invocation).
            format!("{server} {tool} --args '{args_str}'")
        } else if let Some(name) = call.name.strip_prefix("prompt__") {
            // An installed grease prompt (`prompt__<name>`): decode the arguments object into
            // `--key value` flags and route the `<name> --flags` line through the shell-tool machinery
            // (so its Confirm authz pauses under a plain ask, runs under `sudo ask`; `run_prompt` fills
            // the body and dispatches to the model). Each value is single-quoted for the tokenizer.
            let args_val = if call.arguments_json.trim().is_empty() {
                serde_json::json!({})
            } else {
                match serde_json::from_str::<serde_json::Value>(&call.arguments_json) {
                    Ok(v @ serde_json::Value::Object(_)) => v,
                    Ok(_) => return done_err("prompt arguments must be a JSON object".into()),
                    Err(e) => return done_err(format!("malformed arguments: {e}")),
                }
            };
            let mut line = name.to_string();
            if let Some(obj) = args_val.as_object() {
                for (k, v) in obj {
                    // Render the value as a string (JSON strings unquoted; others via to_string).
                    let val = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let escaped = val.replace('\'', r"'\''");
                    line.push_str(&format!(" --{k} '{escaped}'"));
                }
            }
            line
        } else if call.name == crate::ai::ask::SHELL_TOOL {
            // Extract the `command` string from the tool arguments.
            match serde_json::from_str::<serde_json::Value>(&call.arguments_json) {
                Ok(v) => match v.get("command").and_then(|c| c.as_str()) {
                    Some(s) => s.to_string(),
                    None => {
                        return done_err(
                            "malformed arguments: missing string field 'command'".into(),
                        )
                    }
                },
                Err(e) => return done_err(format!("malformed arguments: {e}")),
            }
        } else {
            return done_err(format!("unknown tool '{}'", call.name));
        };

        // Guard: `ask` cannot call itself.
        if crate::ai::ask::classify(&command).is_some() {
            trace.extend_from_slice(format!("[tool] $ {command}\n[tool] refused: ask cannot call itself\n").as_bytes());
            return done_err("ask cannot call itself".into());
        }
        // Guard: shell-internal / parent-shell commands mutate state a subprocess tool can't reach.
        // (`prompt-user` as a bare shell line lands here too — it's ShellInternal — and is refused;
        // the model reaches the human through the dedicated `prompt_user` tool above instead.)
        let (_policy, _elevated, cmd_name) = authz::resolve(&self.registry, &command);
        if let Some(name) = cmd_name.as_deref() {
            if let Some(m) = self.registry.get(name) {
                use crate::manifest::ExecutionScope;
                if matches!(
                    m.execution_scope,
                    ExecutionScope::ShellInternal | ExecutionScope::ParentShell
                ) {
                    trace.extend_from_slice(format!("[tool] $ {command}\n[tool] refused: shell-internal\n").as_bytes());
                    return done_err(format!(
                        "{name} is a shell-internal command, not available as a tool; it mutates \
                         shell state ask cannot access"
                    ));
                }
            }
        }

        // Authorization pre-check (MCP-aware — an MCP tool line resolves to its Confirm manifest).
        // `command` has no leading `sudo` (a model line won't carry it), so elevation is driven
        // entirely by `blanket_authorized` (confirm-tier only).
        let (policy, elevated, _) = self.resolve_authz(&command);
        match authz::decide(policy, elevated, blanket_authorized) {
            Decision::Allow => {}
            Decision::Confirm { sudo_grant } => {
                // Pause and ask the human to authorize this command line (A3).
                trace.extend_from_slice(format!("[tool] $ {command}\n[tool] awaiting authorization\n").as_bytes());
                return ToolStep::Pause(AskPauseKind::Confirm { command, sudo_grant });
            }
            Decision::Deny => {
                trace.extend_from_slice(format!("[tool] $ {command}\n[tool] refused: denied\n").as_bytes());
                return done_err("denied by policy".into());
            }
        }

        ToolStep::Done(self.run_shell_tool(call, &command, blanket_authorized, trace).await)
    }

    /// Run an authorized `shell` command line and build its tool result (JSON stdout/stderr/exit,
    /// truncated). Shared by the inline path and the post-approval resume. Emits the `[tool]` trace.
    async fn run_shell_tool(
        &mut self,
        call: &crate::ai::ask::AskToolCall,
        command: &str,
        blanket_authorized: bool,
        trace: &mut Vec<u8>,
    ) -> crate::ai::ask::AskToolResult {
        let result = self.run_command(command, None, blanket_authorized).await;
        trace.extend_from_slice(
            format!("[tool] $ {command}\n[tool] exit {}\n", result.exit_code).as_bytes(),
        );
        let payload = serde_json::json!({
            "stdout": truncate_tool_output(&result.stdout),
            "stderr": truncate_tool_output(&result.stderr),
            "exit_code": result.exit_code,
        });
        crate::ai::ask::AskToolResult {
            id: call.id.clone(),
            name: call.name.clone(),
            outcome: Ok(payload.to_string()),
        }
    }

    /// Dispatch a parsed `mcp` management command. HTTP-performing subcommands (`add`, `reload`,
    /// `session open`/`close`) require the injected transport; the sync ones (`list`, `tools`,
    /// `remove`, `session list`/`info`) work without it.
    /// Dispatch a parsed `grease` command.
    async fn run_grease(&mut self, cmd: crate::grease::cmd::GreaseCommand) -> LineResult {
        use crate::grease::cmd::GreaseCommand;
        match cmd {
            GreaseCommand::RegistryAdd { url, key } => self.grease_registry_add(&url, key.as_deref()),
            GreaseCommand::RegistryList => self.grease_registry_list(),
            GreaseCommand::RegistryRemove { url } => self.grease_registry_remove(&url),
            GreaseCommand::List => self.grease_list(),
            GreaseCommand::Info { name } => self.grease_info(&name),
            GreaseCommand::Install { name, artifacts } => self.grease_install(&name, artifacts).await,
            GreaseCommand::Remove { name } => self.grease_remove(&name),
            GreaseCommand::Search { query } => self.grease_search(&query).await,
            GreaseCommand::Update { name } => self.grease_update(name.as_deref()).await,
        }
    }

    /// `grease list`: installed packages (all kinds), each tagged with its kind.
    fn grease_list(&self) -> LineResult {
        let packages = self.grease.packages();
        if packages.is_empty() {
            return LineResult::continue_with_stdout(b"no packages installed\n".to_vec());
        }
        let mut out = String::new();
        for p in packages {
            out.push_str(&format!(
                "{}  [{}]  {}\n",
                p.name(),
                p.kind().label(),
                p.payload.description()
            ));
        }
        LineResult::continue_with_stdout(out.into_bytes())
    }

    /// `grease info <name>`: an installed package's metadata. Command packages (prompt/script) show
    /// their generated help; skills (not commands) show the envelope + bundled documents/scripts.
    fn grease_info(&self, name: &str) -> LineResult {
        if let Some(help) = self.grease.pkg_help(name) {
            return LineResult::continue_with_stdout(help.into_bytes());
        }
        if let Some(sk) = self.grease.skill(name) {
            return LineResult::continue_with_stdout(skill_info_text(sk).into_bytes());
        }
        if let Some(m) = self.grease.mcp(name) {
            return LineResult::continue_with_stdout(mcp_info_text(m).into_bytes());
        }
        LineResult::from_outcome(
            Vec::new(),
            format!("grease: '{name}' is not installed\n").into_bytes(),
            1,
        )
    }

    /// `grease install <name>`: fetch the package from the first configured registry that has it,
    /// verify its sha256 (+ signature if the registry is signed), persist it to the store, and register
    /// it. The `artifacts` flags select which of an MCP server's artifact types to install (ignored for
    /// other kinds). A prompt becomes a Confirm command that runs `ask`; a script runs local shell; an
    /// MCP server's tools become `<server> <tool>` commands.
    async fn grease_install(
        &mut self,
        name: &str,
        artifacts: crate::grease::cmd::ArtifactFlags,
    ) -> LineResult {
        if !crate::grease::config::is_valid_name(name) {
            return LineResult::from_outcome(
                Vec::new(),
                format!("grease install: '{name}' is not a valid kebab-case package name\n").into_bytes(),
                2,
            );
        }
        // Reject a name that collides with a static builtin (mirrors `mcp_add`).
        if self.registry.get(name).is_some() {
            return LineResult::from_outcome(
                Vec::new(),
                format!("grease install: '{name}' collides with a built-in command\n").into_bytes(),
                2,
            );
        }
        let registries = crate::grease::config::list_registries();
        if registries.is_empty() {
            return LineResult::from_outcome(
                Vec::new(),
                b"grease install: no registries configured (try `grease registry add <url>`)\n".to_vec(),
                1,
            );
        }
        let Some(http) = self.mcp_http.as_ref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"grease install: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };

        // Try each registry in order: look up the package's expected sha256 in the registry's
        // index.json, then GET <url>/packages/<name>.json and verify the body matches. The fetch is
        // done here (while `http` is borrowed); persistence happens in `grease_finish_install` (which
        // needs `&mut self`), so we capture the results and drop the borrow first.
        let mut last_err = String::from("package not found in any configured registry");
        // (registry, body, index-entry) captured from the first registry that has the package.
        let mut fetched: Option<(String, Vec<u8>, IndexEntry)> = None;
        'registries: for base in &registries {
            // Integrity metadata from the index (best-effort — a loose registry may omit it).
            let entry = fetch_index_entry(http.as_ref(), base, name).await;
            // Try the JSON payload first, then the `.md` prompt-authoring form. Whichever the registry
            // serves, the raw bytes returned are exactly what integrity is verified over below.
            for ext in ["json", "md"] {
                let url = format!("{}/packages/{name}.{ext}", base.trim_end_matches('/'));
                match http.request("GET", &url, &[], None).await {
                    Ok(resp) if resp.status == 200 => {
                        fetched = Some((base.clone(), resp.body, entry));
                        break 'registries;
                    }
                    Ok(resp) if resp.status == 404 => {
                        last_err = format!("package '{name}' not found (404) at {base}");
                    }
                    Ok(resp) => {
                        last_err = format!("registry {base} returned HTTP {}", resp.status);
                    }
                    Err(e) => {
                        last_err = format!("registry {base}: {e}");
                    }
                }
            }
        }
        let Some((registry, body, entry)) = fetched else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("grease install: {last_err}\n").into_bytes(),
                4,
            );
        };

        // Content-addressed integrity: verify the fetched body against the registry's advertised hash.
        // A mismatch is a hard reject (tamper/corruption); a missing index hash falls back to
        // record-only (older/loose registries still work), with a stderr note.
        let actual = crate::grease::pkg::sha256_hex(&body);
        let mut note = Vec::new();
        match &entry.sha256 {
            Some(exp) if exp.eq_ignore_ascii_case(&actual) => {} // verified
            Some(exp) => {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!(
                        "grease install: integrity check failed for '{name}': \
                         expected {exp}, got {actual}\n"
                    )
                    .into_bytes(),
                    4,
                );
            }
            None => {
                note.extend_from_slice(
                    format!("grease: no integrity hash in {registry} index — recording fetched digest\n")
                        .as_bytes(),
                );
            }
        }

        // Signature verification: if the registry was configured with a trusted key (`grease registry
        // add --key`), the payload's detached ed25519 signature MUST verify against it. A configured
        // key with a missing/invalid signature is a HARD reject (a signed registry must sign its
        // packages). No configured key ⇒ unsigned registry (record-only, as before), with a note.
        let mut signature_verified = false;
        let mut signer: Option<String> = None;
        if let Some(trusted_key) = crate::grease::config::registry_key(&registry) {
            let Some(sig) = entry.sig.as_deref() else {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!(
                        "grease install: '{name}' has no signature but {registry} is a signed \
                         registry — refusing to install\n"
                    )
                    .into_bytes(),
                    4,
                );
            };
            if let Err(e) = crate::grease::pkg::verify_signature(&body, sig, &trusted_key) {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("grease install: signature verification failed for '{name}': {e}\n")
                        .into_bytes(),
                    4,
                );
            }
            signature_verified = true;
            signer = entry.signer.clone().or(Some("registry key".to_string()));
        } else {
            note.extend_from_slice(
                format!("grease: {registry} is unsigned (no trusted key) — installing unsigned\n")
                    .as_bytes(),
            );
        }

        // Transparency-log auditing (the other half of README:647): if the registry advertises an
        // RFC-6962 inclusion proof for this package AND the registry is signed (trust is rooted in the
        // same registry key), verify the payload's inclusion in the log against the advertised root. A
        // present-but-invalid proof is a HARD reject; an absent proof leaves the package
        // not-log-audited (as unsigned leaves it unsigned). The root is registry-advertised (not a
        // public witnessed log) — see [[clank-grease]].
        let mut log_verified = false;
        let mut log_index: Option<u64> = None;
        if signature_verified {
            if let Some(log) = &entry.log {
                match verify_log_inclusion(&actual, log) {
                    Ok(()) => {
                        log_verified = true;
                        log_index = Some(log.leaf_index);
                    }
                    Err(e) => {
                        return LineResult::from_outcome(
                            Vec::new(),
                            format!("grease install: transparency-log check failed for '{name}': {e}\n")
                                .into_bytes(),
                            4,
                        );
                    }
                }
            }
        }

        // An MCP-server package needs a live step (initialize + tools/list + prompts/list +
        // resources/list) to enrich its cached surface before persistence — done async here. Other
        // kinds persist synchronously.
        let integrity = InstallIntegrity {
            sha256: actual,
            verified: entry.sha256.is_some(),
            signature_verified,
            signer,
            log_verified,
            log_index,
        };

        // A prompt authored as Markdown (leading `---` frontmatter) is converted to the canonical prompt
        // JSON shape here — AFTER integrity was verified over the raw `.md` bytes (above), so the store
        // and boot path (`load_one`) stay JSON-uniform and never need `.md` awareness. Integrity is NOT
        // re-checked against the converted JSON: the served `.md` bytes are what the registry signed/
        // logged. A JSON body passes through untouched.
        let body = if is_markdown_frontmatter(&body) {
            match crate::grease::pkg::PromptPackage::from_markdown(&body) {
                Ok(p) => p.to_json().into_bytes(),
                Err(e) => {
                    return LineResult::from_outcome(
                        Vec::new(),
                        format!("grease install: {e}\n").into_bytes(),
                        4,
                    )
                }
            }
        } else {
            body
        };

        if crate::grease::pkg::payload_kind(&body) == Ok(crate::grease::pkg::PackageKind::Mcp) {
            return self.grease_finish_install_mcp(name, &registry, &body, integrity, artifacts, note).await;
        }

        self.grease_finish_install(name, &registry, &body, integrity, note)
    }

    /// The MCP-server install path: parse the minimal payload, fetch the live artifact surface
    /// (tools/prompts, and resources if selected), enrich the payload with the cached listings,
    /// persist it, register the server into `McpState` (so its tools become `<server> <tool>`
    /// commands), materialize any prompts as standalone prompt packages, materialize static resources,
    /// and write the marker. Reuses the existing `mcp_install` machinery for tool registration.
    async fn grease_finish_install_mcp(
        &mut self,
        name: &str,
        registry: &str,
        body: &[u8],
        integrity: InstallIntegrity,
        artifacts: crate::grease::cmd::ArtifactFlags,
        mut note: Vec<u8>,
    ) -> LineResult {
        // Parse + name-check the minimal registry payload.
        let mut pkg = match crate::grease::pkg::McpPackage::from_json(body) {
            Ok(p) => p,
            Err(e) => return LineResult::from_outcome(Vec::new(), format!("grease install: {e}\n").into_bytes(), 4),
        };
        if pkg.name != name {
            return LineResult::from_outcome(
                Vec::new(),
                format!("grease install: registry returned package '{}' for request '{name}'\n", pkg.name).into_bytes(),
                4,
            );
        }
        // The install-line flags select which artifact types to expose (no flags = all three).
        pkg.artifacts =
            crate::grease::pkg::McpArtifacts::from_flags(artifacts.tools, artifacts.prompts, artifacts.resources);

        let Some(http) = self.mcp_http.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"grease install: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };

        // Build an MCP client against the server and initialize once.
        let config = crate::mcp::config::McpServerConfig {
            url: pkg.url.clone(),
            enabled: true,
            auth_env: pkg.auth_env.clone(),
            auth_header: None,
        };
        let auth = config.resolve_auth();
        let mut client = crate::mcp::client::McpClient::new(http, &pkg.url, auth);
        let init = match client.initialize().await {
            Ok(i) => i,
            Err(e) => {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("grease install: {name}: {}\n", e.message).into_bytes(),
                    e.exit_code,
                )
            }
        };
        let session = init.session_id.clone();

        // Fetch the selected artifact surfaces. tools/list is required when --tools; prompts/list and
        // resources/list are best-effort (a server may not support them → treated as empty).
        let mut tool_specs = Vec::new();
        if pkg.artifacts.tools {
            match client.list_tools(session.as_deref()).await {
                Ok(t) => tool_specs = t,
                Err(e) => {
                    return LineResult::from_outcome(
                        Vec::new(),
                        format!("grease install: {name}: tools/list: {}\n", e.message).into_bytes(),
                        e.exit_code,
                    )
                }
            }
        }
        let prompt_specs = if pkg.artifacts.prompts {
            client.list_prompts(session.as_deref()).await.unwrap_or_default()
        } else {
            Vec::new()
        };

        // Cache the tool + prompt listings in the payload (so `load()` rebuilds offline).
        pkg.tools = tool_specs
            .iter()
            .map(|t| crate::grease::pkg::McpToolCache {
                name: t.name.clone(),
                description: t.description.clone().unwrap_or_default(),
                input_schema: t.input_schema.to_string(),
            })
            .collect();
        pkg.prompts = prompt_specs
            .iter()
            .map(|p| crate::grease::pkg::McpPromptCache {
                name: p.name.clone(),
                description: p.description.clone().unwrap_or_default(),
            })
            .collect();

        // Materialize any prompts as standalone prompt packages (fetch each body via prompts/get).
        let mut installed_prompts = Vec::new();
        for p in &prompt_specs {
            let args = serde_json::json!({});
            let body_text = client.get_prompt(&p.name, args, session.as_deref()).await.unwrap_or_default();
            installed_prompts.push((p.clone(), body_text));
        }

        // Materialize selected resources (static files under /mnt/mcp/<server>/) + resource templates
        // (executables in /usr/lib/mcp/bin). Best-effort.
        if pkg.artifacts.resources {
            pkg.resources = materialize_mcp_resources(name, &mut client, session.as_deref()).await;
            // Templates: fetch `resources/templates/list` and cache as `<server>-<tname>` executables.
            let templates = client.list_resource_templates(session.as_deref()).await.unwrap_or_default();
            pkg.templates = templates
                .iter()
                .filter_map(|t| {
                    let tname = t.name.clone()?;
                    let cmd = format!("{name}-{tname}");
                    if !crate::grease::config::is_valid_name(&cmd) {
                        return None;
                    }
                    Some(crate::grease::pkg::McpTemplateCache {
                        name: cmd,
                        uri_template: t.uri_template.clone(),
                        description: t.description.clone().unwrap_or_default(),
                    })
                })
                .collect();
        }
        let resource_count = pkg.resources.len();
        let template_count = pkg.templates.len();

        // Persist the enriched payload + marker.
        let payload = crate::grease::state::Payload::Mcp(pkg.clone());
        if let Err(msg) = self.persist_package(name, crate::grease::pkg::PackageKind::Mcp, &payload) {
            return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 1);
        }
        let marker = integrity.to_marker(crate::grease::pkg::PackageKind::Mcp, registry);
        if let Err(msg) = write_install_marker(name, &marker) {
            return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 1);
        }

        // Register the server + tools into `McpState` (so `<server> <tool>` dispatch + the mcp bin stub
        // work), reusing the mcp machinery.
        let tool_count = tool_specs.len();
        if pkg.artifacts.tools {
            let mcp_tools: Vec<crate::mcp::state::McpTool> = tool_specs.into_iter().map(Into::into).collect();
            self.mcp.set_installed(name, config, mcp_tools);
            if let Some(help) = self.mcp.server_help(name) {
                let _ = crate::mcp::config::write_bin_stub(name, &help);
            }
        }

        // Materialize the prompt packages (each becomes an installed grease prompt on $PATH).
        let prompt_count = installed_prompts.len();
        for (spec, body_text) in installed_prompts {
            self.install_mcp_prompt(&spec, &body_text, registry);
        }

        // Write a /usr/lib/mcp/bin stub for each resource-template executable (so which/type/ls see it).
        for t in &pkg.templates {
            let help = format!(
                "{} — MCP resource template ({}). Run `{} <arg…>` to read the constructed URI.\n",
                t.name, t.uri_template, t.name
            );
            let _ = crate::mcp::config::write_bin_stub(&t.name, &help);
        }

        // Register the grease package view.
        self.grease.set_installed(crate::grease::state::InstalledPackage { marker, payload });

        note.extend_from_slice(
            format!(
                "installed {name} [mcp] ({})\n\
                 {tool_count} tools, {prompt_count} prompts, {resource_count} resources, \
                 {template_count} templates\n\
                 tools run as `{name} <tool>`\n",
                integrity.summary()
            )
            .as_bytes(),
        );
        LineResult::continue_with_stdout(note)
    }

    /// Verify + persist a fetched package payload and register it, dispatching on the payload's
    /// declared `kind`. `sha256` is the (already-computed) digest of `body`; `verified` marks whether
    /// it matched the registry's advertised hash; `signature_verified`/`signer` record the ed25519
    /// signing status resolved in `grease_install`.
    fn grease_finish_install(
        &mut self,
        name: &str,
        registry: &str,
        body: &[u8],
        integrity: InstallIntegrity,
        note: Vec<u8>,
    ) -> LineResult {
        let kind = match crate::grease::pkg::payload_kind(body) {
            Ok(k) => k,
            Err(e) => {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("grease install: {e}\n").into_bytes(),
                    4,
                )
            }
        };
        // Parse the payload for this kind and confirm its own name matches the request (guards a
        // misconfigured registry).
        let payload = match self.parse_and_check_payload(name, kind, body) {
            Ok(p) => p,
            Err(msg) => return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 4),
        };

        // Persist the typed payload + write the marker + materialize the kind's on-disk surface.
        if let Err(msg) = self.persist_package(name, kind, &payload) {
            return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 1);
        }
        let marker = integrity.to_marker(kind, registry);
        if let Err(msg) = write_install_marker(name, &marker) {
            return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 1);
        }
        let installed = crate::grease::state::InstalledPackage { marker, payload };
        // Materialize the kind's on-disk surface (bin stub / skill dir tree) — needs the help text,
        // which is derived from the registered package, so register first.
        self.grease.set_installed(installed);
        self.materialize_package(name, kind);

        let run_hint = match kind {
            crate::grease::pkg::PackageKind::Skill => {
                format!("see it with `grease info {name}`")
            }
            _ => format!("run it with `{name}`"),
        };
        let mut out = note; // any record-only/unsigned note first
        out.extend_from_slice(
            format!(
                "installed {name} [{}] ({})\n{run_hint}\n",
                kind.label(),
                integrity.summary()
            )
            .as_bytes(),
        );
        LineResult::continue_with_stdout(out)
    }

    /// Install an MCP server's prompt as a standalone grease prompt package (README: MCP prompts are
    /// installed to `/usr/lib/prompts/bin` and are indistinguishable from standalone prompts). The
    /// prompt's declared arguments become the package arguments; `{{arg}}` placeholders in the fetched
    /// body are already resolved server-side for the empty-arg fetch, so v1 stores the fetched body as
    /// a non-parameterized prompt (re-fetch with args is a future refinement).
    fn install_mcp_prompt(&mut self, spec: &crate::mcp::client::PromptSpec, body: &str, registry: &str) {
        let pkg = crate::grease::pkg::PromptPackage {
            name: spec.name.clone(),
            description: spec.description.clone().unwrap_or_default(),
            model: None,
            arguments: Vec::new(),
            body: body.to_string(),
        };
        // Persist as a prompt package + marker + bin stub, and register it.
        let payload = crate::grease::state::Payload::Prompt(pkg);
        if self.persist_package(&spec.name, crate::grease::pkg::PackageKind::Prompt, &payload).is_err() {
            return;
        }
        let sha = crate::grease::pkg::sha256_hex(body.as_bytes());
        let marker = crate::grease::state::InstallMarker {
            kind: crate::grease::pkg::PackageKind::Prompt,
            registry: registry.to_string(),
            sha256: sha,
            verified: false,
            signature_verified: false,
            signer: None,
            log_verified: false,
            log_index: None,
        };
        if write_install_marker(&spec.name, &marker).is_err() {
            return;
        }
        self.grease.set_installed(crate::grease::state::InstalledPackage { marker, payload });
        self.materialize_package(&spec.name, crate::grease::pkg::PackageKind::Prompt);
    }

    /// Parse a fetched payload for `kind` into a [`crate::grease::state::Payload`], verifying the
    /// package's own name matches the requested `name`. Returns an error string on parse/name mismatch.
    fn parse_and_check_payload(
        &self,
        name: &str,
        kind: crate::grease::pkg::PackageKind,
        body: &[u8],
    ) -> Result<crate::grease::state::Payload, String> {
        use crate::grease::pkg::{
            AgentPackage, McpPackage, PackageKind, PromptPackage, ScriptPackage, SkillPackage,
        };
        use crate::grease::state::Payload;
        let (payload, pkg_name) = match kind {
            PackageKind::Prompt => {
                let p = PromptPackage::from_json(body).map_err(|e| format!("grease install: {e}\n"))?;
                let n = p.name.clone();
                (Payload::Prompt(p), n)
            }
            PackageKind::Script => {
                let s = ScriptPackage::from_json(body).map_err(|e| format!("grease install: {e}\n"))?;
                let n = s.name.clone();
                (Payload::Script(s), n)
            }
            PackageKind::Skill => {
                let s = SkillPackage::from_json(body).map_err(|e| format!("grease install: {e}\n"))?;
                let n = s.name.clone();
                (Payload::Skill(s), n)
            }
            PackageKind::Mcp => {
                let m = McpPackage::from_json(body).map_err(|e| format!("grease install: {e}\n"))?;
                let n = m.name.clone();
                (Payload::Mcp(m), n)
            }
            PackageKind::Agent => {
                let a = AgentPackage::from_json(body).map_err(|e| format!("grease install: {e}\n"))?;
                let n = a.name.clone();
                (Payload::Agent(a), n)
            }
        };
        if pkg_name != name {
            return Err(format!(
                "grease install: registry returned package '{pkg_name}' for request '{name}'\n"
            ));
        }
        Ok(payload)
    }

    /// Persist a typed payload to `<store>/<name>/<kind>.json`.
    fn persist_package(
        &self,
        name: &str,
        kind: crate::grease::pkg::PackageKind,
        payload: &crate::grease::state::Payload,
    ) -> Result<(), String> {
        use crate::grease::state::Payload;
        let store = crate::grease::config::store_dir().join(name);
        std::fs::create_dir_all(&store)
            .map_err(|e| format!("grease install: cannot create store dir: {e}\n"))?;
        let json = match payload {
            Payload::Prompt(p) => p.to_json(),
            Payload::Script(s) => s.to_json(),
            Payload::Skill(s) => s.to_json(),
            Payload::Mcp(m) => m.to_json(),
            Payload::Agent(a) => a.to_json(),
        };
        std::fs::write(store.join(kind.payload_file()), json)
            .map_err(|e| format!("grease install: cannot write payload: {e}\n"))
    }

    /// Materialize a kind's on-disk surface after registration: a bin stub for command packages
    /// (prompt→`/usr/lib/prompts/bin`, script→`/usr/bin`) or the skill dir tree (docs + bundled
    /// `bin/` scripts) for a skill. Best-effort — the durable payload is already persisted.
    fn materialize_package(&self, name: &str, kind: crate::grease::pkg::PackageKind) {
        use crate::grease::pkg::PackageKind;
        match kind {
            PackageKind::Prompt => {
                let help = self
                    .grease
                    .pkg_help(name)
                    .unwrap_or_else(|| format!("{name} — installed prompt\n"));
                let _ = crate::grease::config::write_bin_stub(
                    &crate::grease::config::bin_dir(),
                    name,
                    &help,
                    "prompt",
                );
            }
            PackageKind::Script => {
                let help = self
                    .grease
                    .pkg_help(name)
                    .unwrap_or_else(|| format!("{name} — installed script\n"));
                let _ = crate::grease::config::write_bin_stub(
                    &crate::grease::config::script_bin_dir(),
                    name,
                    &help,
                    "script",
                );
            }
            PackageKind::Skill => {
                if let Some(sk) = self.grease.skill(name) {
                    let _ = crate::grease::config::materialize_skill(sk);
                }
            }
            PackageKind::Mcp => {
                // MCP registration into `McpState` (tools) + prompt materialization happens in the
                // async `grease_finish_install_mcp` path (it needs the live server); nothing to do
                // synchronously here.
            }
            PackageKind::Agent => {
                let help = self
                    .grease
                    .pkg_help(name)
                    .unwrap_or_else(|| format!("{name} — installed agent\n"));
                let _ = crate::grease::config::write_bin_stub(
                    &crate::grease::config::agent_bin_dir(),
                    name,
                    &help,
                    "agent",
                );
            }
        }
    }

    /// `grease remove <name>`: delete the store, marker, and the kind's on-disk surface, and
    /// deregister.
    fn grease_remove(&mut self, name: &str) -> LineResult {
        let Some(kind) = self.grease.kind_of(name) else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("grease remove: '{name}' is not installed\n").into_bytes(),
                1,
            );
        };
        let _ = std::fs::remove_file(crate::grease::config::etc_dir().join(format!("{name}.toml")));
        let _ = std::fs::remove_dir_all(crate::grease::config::store_dir().join(name));
        match kind {
            crate::grease::pkg::PackageKind::Prompt => {
                let _ = std::fs::remove_file(crate::grease::config::bin_dir().join(name));
            }
            crate::grease::pkg::PackageKind::Script => {
                let _ = std::fs::remove_file(crate::grease::config::script_bin_dir().join(name));
            }
            crate::grease::pkg::PackageKind::Skill => {
                let _ = std::fs::remove_dir_all(crate::grease::config::skills_dir().join(name));
            }
            crate::grease::pkg::PackageKind::Mcp => {
                // Deregister the server from `McpState` (also removes its /usr/lib/mcp/bin stub) and
                // remove any materialized resource tree under /mnt/mcp/<name>/.
                let _ = crate::mcp::config::remove(name);
                self.mcp.remove(name);
                let _ = std::fs::remove_dir_all(crate::grease::config::mcp_mount_dir().join(name));
            }
            crate::grease::pkg::PackageKind::Agent => {
                let _ = std::fs::remove_file(crate::grease::config::agent_bin_dir().join(name));
            }
        }
        self.grease.remove(name);
        LineResult::continue_with_stdout(format!("removed {name}\n").into_bytes())
    }

    /// `grease search <query>`: fetch each registry's `index.json` and list matching package names.
    async fn grease_search(&mut self, query: &str) -> LineResult {
        let registries = crate::grease::config::list_registries();
        if registries.is_empty() {
            return LineResult::from_outcome(
                Vec::new(),
                b"grease search: no registries configured\n".to_vec(),
                1,
            );
        }
        let Some(http) = self.mcp_http.as_ref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"grease search: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };
        let mut hits = Vec::new();
        for base in &registries {
            let url = format!("{}/index.json", base.trim_end_matches('/'));
            if let Ok(resp) = http.request("GET", &url, &[], None).await {
                if resp.status == 200 {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&resp.body) {
                        if let Some(arr) = v.get("packages").and_then(|p| p.as_array()) {
                            for pkg in arr {
                                let name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or("");
                                let desc = pkg.get("description").and_then(|d| d.as_str()).unwrap_or("");
                                let kind = pkg.get("kind").and_then(|k| k.as_str()).unwrap_or("prompt");
                                if name.contains(query) || desc.contains(query) {
                                    hits.push(format!("{name}  [{kind}]  {desc}"));
                                }
                            }
                        }
                    }
                }
            }
        }
        if hits.is_empty() {
            return LineResult::continue_with_stdout(format!("no packages match '{query}'\n").into_bytes());
        }
        hits.sort();
        hits.dedup();
        LineResult::continue_with_stdout(format!("{}\n", hits.join("\n")).into_bytes())
    }

    /// `grease update [<name>]`: re-fetch + re-verify + re-persist installed packages (all, or one).
    async fn grease_update(&mut self, name: Option<&str>) -> LineResult {
        let targets: Vec<String> = match name {
            Some(n) if self.grease.get(n).is_some() => vec![n.to_string()],
            Some(n) => {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("grease update: '{n}' is not installed\n").into_bytes(),
                    1,
                )
            }
            None => self.grease.packages().iter().map(|p| p.name().to_string()).collect(),
        };
        if targets.is_empty() {
            return LineResult::continue_with_stdout(b"nothing to update\n".to_vec());
        }
        let mut out = String::new();
        for t in targets {
            // Re-install preserving the package's existing artifact selection (for MCP; a no-op for
            // other kinds). The stored payload carries the prior `artifacts`, so pass its flags.
            let flags = self
                .grease
                .mcp(&t)
                .map(|m| crate::grease::cmd::ArtifactFlags {
                    tools: m.artifacts.tools,
                    prompts: m.artifacts.prompts,
                    resources: m.artifacts.resources,
                })
                .unwrap_or_default();
            let result = Box::pin(self.grease_install(&t, flags)).await;
            out.push_str(&String::from_utf8_lossy(&result.terminal_output()));
        }
        LineResult::continue_with_stdout(out.into_bytes())
    }

    /// `grease registry add <url> [--key <base64-ed25519-pubkey>]`: record a registry URL and, if
    /// given, its trusted signing key. The key is validated (must decode to a 32-byte ed25519 key)
    /// before it's stored, so a typo is caught at `add` time, not at install time.
    fn grease_registry_add(&self, url: &str, key: Option<&str>) -> LineResult {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return LineResult::from_outcome(
                Vec::new(),
                format!("grease registry add: '{url}' is not an http(s) URL\n").into_bytes(),
                2,
            );
        }
        if let Some(k) = key {
            if let Err(e) = crate::grease::pkg::validate_public_key(k) {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("grease registry add: {e}\n").into_bytes(),
                    2,
                );
            }
        }
        match crate::grease::config::add_registry(url, key) {
            Ok(true) => {
                let msg = if key.is_some() {
                    format!("added registry {url} (signed)\n")
                } else {
                    format!("added registry {url}\n")
                };
                LineResult::continue_with_stdout(msg.into_bytes())
            }
            Ok(false) => {
                LineResult::continue_with_stdout(format!("registry {url} already present\n").into_bytes())
            }
            Err(e) => LineResult::from_outcome(Vec::new(), format!("grease: {e}\n").into_bytes(), 1),
        }
    }

    /// `grease registry list`: the configured registry URLs.
    fn grease_registry_list(&self) -> LineResult {
        let urls = crate::grease::config::list_registries();
        if urls.is_empty() {
            return LineResult::continue_with_stdout(b"no registries configured\n".to_vec());
        }
        LineResult::continue_with_stdout(format!("{}\n", urls.join("\n")).into_bytes())
    }

    /// `grease registry remove <url>`: drop a registry URL.
    fn grease_registry_remove(&self, url: &str) -> LineResult {
        match crate::grease::config::remove_registry(url) {
            Ok(true) => LineResult::continue_with_stdout(format!("removed registry {url}\n").into_bytes()),
            Ok(false) => LineResult::from_outcome(
                Vec::new(),
                format!("grease: registry {url} was not configured\n").into_bytes(),
                1,
            ),
            Err(e) => LineResult::from_outcome(Vec::new(), format!("grease: {e}\n").into_bytes(), 1),
        }
    }

    async fn run_mcp(&mut self, cmd: crate::mcp::cmd::McpCommand) -> LineResult {
        use crate::mcp::cmd::McpCommand;
        match cmd {
            McpCommand::List => self.mcp_list(),
            McpCommand::Tools { server } => self.mcp_tools(&server),
            McpCommand::Remove { name } => self.mcp_remove(&name),
            McpCommand::Watch { uri } => self.run_mcp_watch(&uri).await,
            McpCommand::ResourceInfo { path } => self.run_mcp_resource_info(&path),
            McpCommand::Add {
                name,
                url,
                auth_env,
                auth_header,
            } => self.mcp_add(&name, &url, auth_env, auth_header).await,
            McpCommand::Reload { name } => self.mcp_reload(name.as_deref()).await,
            McpCommand::SessionList => self.mcp_session_list(),
            McpCommand::SessionInfo { id } => self.mcp_session_info(&id),
            McpCommand::SessionOpen { server } => self.mcp_session_open(&server).await,
            McpCommand::SessionClose { id } => self.mcp_session_close(&id).await,
        }
    }

    /// `mcp list`: configured servers with url/enabled/install status/tool count or error.
    fn mcp_list(&self) -> LineResult {
        let names = crate::mcp::config::list_names();
        if names.is_empty() {
            return LineResult::continue_with_stdout(b"no MCP servers configured\n".to_vec());
        }
        let mut out = String::new();
        for name in &names {
            match self.mcp.get(name) {
                Some(s) if s.installed => out.push_str(&format!(
                    "{name}  {}  enabled  {} tools\n",
                    s.config.url,
                    s.tools.len()
                )),
                Some(s) => out.push_str(&format!(
                    "{name}  {}  not installed  ({})\n",
                    s.config.url,
                    s.last_error.as_deref().unwrap_or("unknown error")
                )),
                None => {
                    // Configured on disk but not yet loaded into this session.
                    let url = crate::mcp::config::load(name)
                        .ok()
                        .flatten()
                        .map(|c| c.url)
                        .unwrap_or_default();
                    out.push_str(&format!("{name}  {url}  not loaded (run `mcp reload {name}`)\n"));
                }
            }
        }
        LineResult::continue_with_stdout(out.into_bytes())
    }

    /// `mcp tools <server>`: list an installed server's tools.
    fn mcp_tools(&self, server: &str) -> LineResult {
        match self.mcp.get(server) {
            Some(s) if s.installed => {
                let mut out = String::new();
                for t in &s.tools {
                    out.push_str(&format!(
                        "{}  {}\n",
                        t.name,
                        t.description.as_deref().unwrap_or("")
                    ));
                }
                LineResult::continue_with_stdout(out.into_bytes())
            }
            Some(_) => LineResult::from_outcome(
                Vec::new(),
                format!("mcp tools: '{server}' is configured but not installed\n").into_bytes(),
                1,
            ),
            None => LineResult::from_outcome(
                Vec::new(),
                format!("mcp tools: no such server '{server}'\n").into_bytes(),
                1,
            ),
        }
    }

    /// `mcp remove <server>`: delete the config + stub and forget the server.
    fn mcp_remove(&mut self, name: &str) -> LineResult {
        match crate::mcp::config::remove(name) {
            Ok(()) => {
                self.mcp.remove(name);
                LineResult::continue_with_stdout(format!("removed MCP server '{name}'\n").into_bytes())
            }
            Err(e) => LineResult::from_outcome(Vec::new(), format!("mcp remove: {e}\n").into_bytes(), 1),
        }
    }

    /// `mcp add <name> <url>`: write the config, then install (initialize + tools/list). An install
    /// failure keeps the config as "configured, not installed" and exits 4.
    async fn mcp_add(
        &mut self,
        name: &str,
        url: &str,
        auth_env: Option<String>,
        auth_header: Option<String>,
    ) -> LineResult {
        if !crate::mcp::config::is_valid_name(name) {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp add: invalid server name '{name}' (use kebab-case: [a-z0-9-])\n").into_bytes(),
                2,
            );
        }
        // Reject a name that collides with a built-in command (it would shadow it).
        if self.registry.get(name).is_some() {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp add: '{name}' collides with a built-in command\n").into_bytes(),
                2,
            );
        }
        let mut config = crate::mcp::config::McpServerConfig::new(url);
        config.auth_env = auth_env;
        config.auth_header = auth_header;
        if let Err(e) = crate::mcp::config::save(name, &config) {
            return LineResult::from_outcome(Vec::new(), format!("mcp add: {e}\n").into_bytes(), 1);
        }
        self.mcp_install(name, config).await
    }

    /// `mcp reload [<name>]`: re-read config(s) and re-install the enabled ones.
    async fn mcp_reload(&mut self, name: Option<&str>) -> LineResult {
        let names: Vec<String> = match name {
            Some(n) => vec![n.to_string()],
            None => crate::mcp::config::list_names(),
        };
        let mut out = String::new();
        let mut any_err = false;
        for n in names {
            let config = match crate::mcp::config::load(&n) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    out.push_str(&format!("mcp reload: no config for '{n}'\n"));
                    any_err = true;
                    continue;
                }
                Err(e) => {
                    out.push_str(&format!("mcp reload: {e}\n"));
                    any_err = true;
                    continue;
                }
            };
            if !config.enabled {
                self.mcp.remove(&n);
                out.push_str(&format!("{n}: disabled (skipped)\n"));
                continue;
            }
            let result = self.mcp_install(&n, config).await;
            out.push_str(&String::from_utf8_lossy(&result.terminal_output()));
            if result.exit_code != 0 {
                any_err = true;
            }
        }
        LineResult::from_outcome(out.into_bytes(), Vec::new(), u8::from(any_err))
    }

    /// Shared install path: initialize + tools/list, record the result in `McpState`, write the
    /// `/usr/lib/mcp/bin` stub on success. A transport/HTTP failure records "configured, not
    /// installed" and exits 4.
    async fn mcp_install(
        &mut self,
        name: &str,
        config: crate::mcp::config::McpServerConfig,
    ) -> LineResult {
        let Some(http) = self.mcp_http.as_deref() else {
            self.mcp.set_failed(name, config, "no HTTP transport".into());
            return LineResult::from_outcome(
                Vec::new(),
                b"mcp: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };
        let auth = config.resolve_auth();
        let mut client = crate::mcp::client::McpClient::new(http, &config.url, auth);
        let init = match client.initialize().await {
            Ok(i) => i,
            Err(e) => {
                let msg = format!("mcp: {name}: {}\n", e.message);
                self.mcp.set_failed(name, config, e.message);
                return LineResult::from_outcome(Vec::new(), msg.into_bytes(), e.exit_code);
            }
        };
        let tools = match client.list_tools(init.session_id.as_deref()).await {
            Ok(t) => t,
            Err(e) => {
                let msg = format!("mcp: {name}: {}\n", e.message);
                self.mcp.set_failed(name, config, e.message);
                return LineResult::from_outcome(Vec::new(), msg.into_bytes(), e.exit_code);
            }
        };
        let tool_count = tools.len();
        let mcp_tools: Vec<crate::mcp::state::McpTool> = tools.into_iter().map(Into::into).collect();
        self.mcp.set_installed(name, config, mcp_tools);
        // Write the /usr/lib/mcp/bin stub so which/ls/type see the server as a $PATH command.
        if let Some(help) = self.mcp.server_help(name) {
            let _ = crate::mcp::config::write_bin_stub(name, &help);
        }
        LineResult::continue_with_stdout(
            format!("installed MCP server '{name}' ({tool_count} tools)\n").into_bytes(),
        )
    }

    /// `mcp session list`: local id, server, server session id, protocol.
    fn mcp_session_list(&self) -> LineResult {
        let sessions = self.mcp.sessions();
        if sessions.is_empty() {
            return LineResult::continue_with_stdout(b"no open MCP sessions\n".to_vec());
        }
        let mut out = String::new();
        for s in sessions {
            out.push_str(&format!(
                "{}  {}  {}  {}\n",
                s.local_id,
                s.server,
                s.server_session_id.as_deref().unwrap_or("-"),
                s.protocol_version
            ));
        }
        LineResult::continue_with_stdout(out.into_bytes())
    }

    /// `mcp session info <id>`: server info, protocol, capabilities.
    fn mcp_session_info(&self, id: &str) -> LineResult {
        match self.mcp.session(id) {
            Some(s) => {
                let out = format!(
                    "id:         {}\nserver:     {}\nserver info: {}\nprotocol:   {}\ncapabilities: {}\n",
                    s.local_id, s.server, s.server_info, s.protocol_version, s.capabilities
                );
                LineResult::continue_with_stdout(out.into_bytes())
            }
            None => LineResult::from_outcome(
                Vec::new(),
                format!("mcp session info: no such session '{id}'\n").into_bytes(),
                1,
            ),
        }
    }

    /// `mcp session open <server>`: explicit initialize, record the session, print its ids.
    async fn mcp_session_open(&mut self, server: &str) -> LineResult {
        let Some(config) = self.mcp.get(server).map(|s| s.config.clone()) else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp session open: no such installed server '{server}'\n").into_bytes(),
                1,
            );
        };
        let Some(http) = self.mcp_http.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"mcp: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };
        let auth = config.resolve_auth();
        let mut client = crate::mcp::client::McpClient::new(http, &config.url, auth);
        match client.initialize().await {
            Ok(init) => {
                let local_id = self.mcp.open_session(server, &init);
                LineResult::continue_with_stdout(
                    format!(
                        "opened session {local_id} ({})\n",
                        init.session_id.as_deref().unwrap_or("no server session id")
                    )
                    .into_bytes(),
                )
            }
            Err(e) => LineResult::from_outcome(
                Vec::new(),
                format!("mcp session open: {}\n", e.message).into_bytes(),
                e.exit_code,
            ),
        }
    }

    /// `mcp session close <id>`: DELETE the server session, remove it locally. A 405 refusal still
    /// removes the local session (with a note).
    async fn mcp_session_close(&mut self, id: &str) -> LineResult {
        let Some((server, server_sid)) = self.mcp.close_session(id) else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp session close: no such session '{id}'\n").into_bytes(),
                1,
            );
        };
        // If there's no server-issued session id, there's nothing to DELETE — local removal is enough.
        let Some(server_sid) = server_sid else {
            return LineResult::continue_with_stdout(format!("closed session {id}\n").into_bytes());
        };
        let config = self.mcp.get(&server).map(|s| s.config.clone());
        let (Some(config), Some(http)) = (config, self.mcp_http.as_deref()) else {
            return LineResult::continue_with_stdout(
                format!("closed session {id} (locally; server not reachable)\n").into_bytes(),
            );
        };
        let auth = config.resolve_auth();
        let client = crate::mcp::client::McpClient::new(http, &config.url, auth);
        match client.close_session(&server_sid).await {
            Ok(()) => LineResult::continue_with_stdout(format!("closed session {id}\n").into_bytes()),
            Err(e) => LineResult::from_outcome(
                format!("closed session {id} locally\n").into_bytes(),
                format!("mcp session close: {}\n", e.message).into_bytes(),
                e.exit_code,
            ),
        }
    }

    /// Whether `line`'s leading word is an installed MCP server (and it isn't `mcp` itself). Drives
    /// the dynamic `<server> <tool>` dispatch.
    fn is_mcp_tool_line(&self, line: &str) -> bool {
        match crate::mcp::cmd::parse_tool_invocation(line) {
            Some(Ok(inv)) => self.mcp.is_server(&inv.server),
            _ => false,
        }
    }

    /// If `line` is a top-level `cat /mnt/mcp/<server>/<path>` (optionally `sudo`-prefixed, one
    /// operand, no operators) naming a DYNAMIC MCP resource, return its `(server, uri)`. Static
    /// resources are real files that Brush's `cat` reads directly, so they return `None` here.
    fn dynamic_mcp_read_target(&self, line: &str) -> Option<(String, String)> {
        // Reject anything with shell operators (the Wall-C wall — a live read can't run in a pipe).
        if line.chars().any(|c| "|&;<>`$".contains(c)) {
            return None;
        }
        let words = crate::ai::ask::dequote_words(line)?;
        let mut it = words.iter();
        let mut first = it.next()?.as_str();
        if first == "sudo" {
            first = it.next()?.as_str();
        }
        if first != "cat" {
            return None;
        }
        // Exactly one non-flag operand, and it must be an /mnt/mcp path.
        let operands: Vec<&String> = it.filter(|w| !w.starts_with('-')).collect();
        if operands.len() != 1 {
            return None;
        }
        let path = operands[0];
        if !crate::mcpfs::is_mcp_path(path) {
            return None;
        }
        let index = self.grease.mcp_resource_index();
        match crate::mcpfs::classify(path, &index) {
            crate::mcpfs::McpPathKind::Dynamic { server, uri } => Some((server, uri)),
            _ => None,
        }
    }

    /// Fetch a dynamic MCP resource live (`resources/read`) and print its content. Reuses the server's
    /// stored config for the endpoint + auth.
    async fn run_mcp_resource_read(&mut self, server: &str, uri: &str) -> LineResult {
        let Some(m) = self.grease.mcp(server) else {
            return LineResult::from_outcome(Vec::new(), b"cat: mcp resource: server not installed\n".to_vec(), 1);
        };
        let url = m.url.clone();
        let auth_env = m.auth_env.clone();
        let Some(http) = self.mcp_http.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"cat: mcp resource: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };
        let config = crate::mcp::config::McpServerConfig { url: url.clone(), enabled: true, auth_env, auth_header: None };
        let auth = config.resolve_auth();
        let mut client = crate::mcp::client::McpClient::new(http, &url, auth);
        let session = client.initialize().await.ok().and_then(|i| i.session_id);
        match client.read_resource(uri, session.as_deref()).await {
            Ok(content) => LineResult::continue_with_stdout(content.into_bytes()),
            Err(e) => LineResult::from_outcome(Vec::new(), format!("cat: {uri}: {}\n", e.message).into_bytes(), e.exit_code),
        }
    }

    /// `mcp resource info <path>` — print the full MCP annotation set for a mounted resource. Reads
    /// from the cached resource index (no live fetch); an unknown path is an error.
    fn run_mcp_resource_info(&self, path: &str) -> LineResult {
        if !crate::mcpfs::is_mcp_path(path) {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp resource info: '{path}' is not a /mnt/mcp resource\n").into_bytes(),
                2,
            );
        }
        // Split `/mnt/mcp/<server>/<rel>`.
        let rel = path.trim_start_matches("/mnt/mcp").trim_start_matches('/');
        let Some((server, sub)) = rel.split_once('/') else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp resource info: '{path}' names a server, not a resource\n").into_bytes(),
                2,
            );
        };
        let Some(res) = self.grease.mcp_resource_entry(server, sub) else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp resource info: no such resource '{path}'\n").into_bytes(),
                1,
            );
        };
        let mut out = format!("uri: {}\n", res.uri);
        out.push_str(&format!("kind: {}\n", if res.is_static { "static" } else { "dynamic" }));
        if !res.description.is_empty() {
            out.push_str(&format!("description: {}\n", res.description));
        }
        if let Some(m) = &res.mime_type {
            out.push_str(&format!("mime-type: {m}\n"));
        }
        if let Some(s) = res.size {
            out.push_str(&format!("size: {s}\n"));
        }
        if let Some(lm) = &res.last_modified {
            out.push_str(&format!("last-modified: {lm}\n"));
        }
        if let Some(a) = &res.audience {
            out.push_str(&format!("audience: {a}\n"));
        }
        if let Some(p) = res.priority {
            out.push_str(&format!("priority: {p}\n"));
        }
        LineResult::continue_with_stdout(out.into_bytes())
    }

    /// `mcp watch <uri>` — a BOUNDED poll-based subscription (the durable agent can't hold a long-lived
    /// push stream across serialized invocations, and the transport is one-shot request/response). We
    /// `resources/subscribe` then poll `resources/read` a fixed number of times, printing the content
    /// each time it changes. Honest about being polling, not push.
    async fn run_mcp_watch(&mut self, uri: &str) -> LineResult {
        // Resolve which installed server owns this URI (by scheme/prefix match against its resources).
        let server = self.grease.mcp_packages().iter().find_map(|m| {
            let owns = m.resources.iter().any(|r| r.uri == uri)
                || uri.split_once("://").map(|(s, _)| s) == Some(m.name.as_str());
            if owns {
                Some((m.name.clone(), m.url.clone(), m.auth_env.clone()))
            } else {
                None
            }
        });
        let Some((_name, url, auth_env)) = server else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("mcp watch: no installed server owns '{uri}'\n").into_bytes(),
                1,
            );
        };
        let Some(http) = self.mcp_http.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"mcp watch: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };
        let config = crate::mcp::config::McpServerConfig { url: url.clone(), enabled: true, auth_env, auth_header: None };
        let auth = config.resolve_auth();
        let mut client = crate::mcp::client::McpClient::new(http, &url, auth);
        let session = client.initialize().await.ok().and_then(|i| i.session_id);
        let _ = client.subscribe_resource(uri, session.as_deref()).await; // best-effort

        // Bounded poll loop: read the resource a fixed number of times, printing on change.
        const POLLS: usize = 3;
        let mut out = format!(
            "mcp watch {uri}: polling {POLLS}× (the durable agent can't hold a push stream; this is a \
             bounded poll, not a live subscription)\n"
        );
        let mut last: Option<String> = None;
        for i in 0..POLLS {
            match client.read_resource(uri, session.as_deref()).await {
                Ok(content) => {
                    if last.as_deref() != Some(content.as_str()) {
                        out.push_str(&format!("[poll {}] {}\n", i + 1, content.trim_end()));
                        last = Some(content);
                    } else {
                        out.push_str(&format!("[poll {}] (unchanged)\n", i + 1));
                    }
                }
                Err(e) => {
                    out.push_str(&format!("[poll {}] error: {}\n", i + 1, e.message));
                }
            }
        }
        out.push_str("mcp watch: done (bounded poll complete)\n");
        LineResult::continue_with_stdout(out.into_bytes())
    }

    /// Whether `line`'s leading word is an installed MCP resource-template executable
    /// (`<server>-<template>`). Top-level only (Wall-C — the read awaits under the reactor).
    fn is_mcp_template_line(&self, line: &str) -> bool {
        let Some(word) = prompt_leading_word(line) else {
            return false;
        };
        self.grease.is_mcp_template(&word)
    }

    /// Run an installed MCP resource template: substitute the CLI args into the `{param}` placeholders
    /// of the stored URI template, then read the constructed resource live and print it (README:767).
    /// Positional args fill the template's `{param}` placeholders in order; `--param value` fills by
    /// name. The read awaits under the reactor (top-level only).
    async fn run_mcp_template(&mut self, line: &str) -> LineResult {
        let words = match crate::ai::ask::dequote_words(line) {
            Some(w) => w,
            None => return LineResult::from_outcome(Vec::new(), b"mcp template: parse error\n".to_vec(), 2),
        };
        // Strip a leading sudo (the gate already resolved authz).
        let rest = if words.first().map(String::as_str) == Some("sudo") { &words[1..] } else { &words[..] };
        let cmd = rest[0].clone();
        let Some((url, auth_env, template)) = self.grease.mcp_template(&cmd) else {
            return LineResult::denied(); // is_mcp_template_line gated it
        };
        // Build the concrete URI: fill `{param}` placeholders. `--name value` fills by name; bare
        // positionals fill the remaining `{…}` slots left-to-right.
        let uri = match fill_uri_template(&template, &rest[1..]) {
            Ok(u) => u,
            Err(e) => return LineResult::from_outcome(Vec::new(), format!("{cmd}: {e}\n").into_bytes(), 2),
        };
        let Some(http) = self.mcp_http.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("{cmd}: no HTTP transport configured (available on the Golem agent)\n").into_bytes(),
                4,
            );
        };
        let config = crate::mcp::config::McpServerConfig { url: url.clone(), enabled: true, auth_env, auth_header: None };
        let auth = config.resolve_auth();
        let mut client = crate::mcp::client::McpClient::new(http, &url, auth);
        let session = client.initialize().await.ok().and_then(|i| i.session_id);
        match client.read_resource(&uri, session.as_deref()).await {
            Ok(content) => LineResult::continue_with_stdout(content.into_bytes()),
            Err(e) => LineResult::from_outcome(Vec::new(), format!("{cmd}: {uri}: {}\n", e.message).into_bytes(), e.exit_code),
        }
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

    /// Whether `line`'s leading word is an installed Golem agent. Drives the `run_agent` dispatch.
    /// Top-level only (a remote invocation awaits under the Session reactor; not reachable from Brush's
    /// nested runtime — the Wall-C wall).
    fn is_agent_line(&self, line: &str) -> bool {
        let Some(word) = prompt_leading_word(line) else {
            return false;
        };
        self.grease.is_agent(&word)
    }

    /// Run an installed Golem agent: parse the ctor/wrapper-flags/method/args, validate the method (or
    /// dispatch a reserved subcommand), and invoke it via the injected wRPC invoker in the selected mode
    /// (await / trigger / schedule). Missing invoker → honest "needs a cluster"; unknown method → exit 2.
    async fn run_agent(&mut self, line: &str) -> LineResult {
        let words = match crate::ai::ask::dequote_words(line) {
            Some(w) => w,
            None => return LineResult::from_outcome(Vec::new(), b"agent: parse error\n".to_vec(), 2),
        };
        // Strip a leading sudo (gate already resolved).
        let rest = if words.first().map(String::as_str) == Some("sudo") { &words[1..] } else { &words[..] };
        let name = rest[0].clone();
        let Some(pkg) = self.grease.agent(&name).cloned() else {
            return LineResult::denied(); // is_agent_line gated it
        };

        let parsed = match parse_agent_line(&rest[1..], &pkg) {
            Ok(p) => p,
            Err(msg) => return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 2),
        };

        // `--revision` has no wasm-rpc constructor slot — it's a golem:api concept (component-revision
        // targeting). Honest limitation rather than a silently-ignored flag.
        if parsed.revision.is_some() {
            return LineResult::from_outcome(
                Vec::new(),
                format!(
                    "{name}: --revision targeting is not supported on this SDK surface \
                     (component-revision selection is a golem:api concern; the invocation targets the \
                     running revision)\n"
                )
                .into_bytes(),
                2,
            );
        }

        // `--help` or no method → agent help.
        if parsed.method.is_empty() {
            let help = self.grease.pkg_help(&name).unwrap_or_default();
            return LineResult::continue_with_stdout(help.into_bytes());
        }

        // Reserved subcommands (README:832) — cannot be method names.
        match parsed.method.as_str() {
            "oplog" | "status" if pkg.ephemeral => {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("{name}: '{}' is not available for an ephemeral agent type\n", parsed.method)
                        .into_bytes(),
                    2,
                );
            }
            "oplog" => return self.run_agent_reserved(&name, &pkg, &parsed, "oplog").await,
            "status" => return self.run_agent_reserved(&name, &pkg, &parsed, "status").await,
            "stream" | "repl" => {
                // Long-lived/interactive — the durable agent serializes invocations and can't park on a
                // stream/REPL loop (same constraint as `ask repl`). Honest pointer.
                return LineResult::from_outcome(
                    Vec::new(),
                    format!(
                        "{name}: '{}' (interactive/streaming) is a native-terminal feature; the durable \
                         agent can't hold a long-lived session (Golem serializes invocations). Use the \
                         await/--trigger/--schedule invocation modes instead.\n",
                        parsed.method
                    )
                    .into_bytes(),
                    2,
                );
            }
            _ => {}
        }

        if pkg.method(&parsed.method).is_none() {
            return LineResult::from_outcome(
                Vec::new(),
                format!("{name}: unknown method '{}' (try `{name} --help`)\n", parsed.method).into_bytes(),
                2,
            );
        }

        let inv = crate::golem::agent::AgentInvocation {
            agent_type: pkg.agent_type.clone(),
            constructor: parsed.constructor,
            method: parsed.method,
            args: parsed.args,
            mode: parsed.mode.clone(),
            phantom: parsed.phantom,
        };

        let Some(invoker) = self.agent_invoker.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                format!(
                    "{name}: Golem agent invocation requires a configured cluster \
                     (unavailable on this target)\n"
                )
                .into_bytes(),
                4,
            );
        };

        // Structured audit event for the Golem invocation (README:627): agent type, ordered constructor
        // params, method, mode, and phantom UUID. Logged to mcp.log (the outbound-call log) with the
        // agent identity so it correlates with Golem cluster logs. (revision + await-mode idempotency-key
        // have no fields yet — honest-stubbed / not surfaced by the SDK on this path.)
        let ctor = inv
            .constructor
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        let mode = match &inv.mode {
            crate::golem::agent::InvokeMode::Await => "await".to_string(),
            crate::golem::agent::InvokeMode::Trigger => "trigger".to_string(),
            crate::golem::agent::InvokeMode::Schedule(when) => format!("schedule:{when}"),
        };
        crate::logging::Record::new("agent-invoke")
            .field("type", &inv.agent_type)
            .field("ctor", &ctor)
            .field("method", &inv.method)
            .field("mode", &mode)
            .field("phantom", inv.phantom.as_deref().unwrap_or(""))
            .emit(crate::logging::LogFile::Mcp);

        match parsed.mode {
            crate::golem::agent::InvokeMode::Await => match invoker.invoke(&inv).await {
                Ok(result) => {
                    let mut out = result.into_bytes();
                    if !out.ends_with(b"\n") {
                        out.push(b'\n');
                    }
                    LineResult::continue_with_stdout(out)
                }
                Err(e) => LineResult::from_outcome(Vec::new(), format!("{name}: {e}\n").into_bytes(), 1),
            },
            crate::golem::agent::InvokeMode::Trigger | crate::golem::agent::InvokeMode::Schedule(_) => {
                // Fire-and-forget / deferred: returns a handle (+ a PID row for `ps`/`kill`).
                match invoker.invoke_async(&inv).await {
                    Ok(handle) => {
                        // Spawn an S-state proc row for the pending invocation (README: all modes return
                        // a PID); retain the cancel token so `kill <pid>` can cancel it.
                        let pid = self.spawn_agent_invocation_row(line, handle.cancel_token.clone());
                        LineResult::continue_with_stdout(
                            format!("[{pid}] {name}: {}\n", handle.note).into_bytes(),
                        )
                    }
                    Err(e) => LineResult::from_outcome(Vec::new(), format!("{name}: {e}\n").into_bytes(), 1),
                }
            }
        }
    }

    /// Dispatch a `golem` cluster command through the injected [`crate::golem::cluster::GolemCluster`] seam.
    /// `interrupt`/`resume` are honest-stubbed (no host primitive); everything else needs a configured
    /// cluster (honest error otherwise).
    async fn run_golem(&mut self, cmd: crate::golem::cluster::GolemCommand) -> LineResult {
        use crate::golem::cluster::GolemCommand as G;
        // interrupt/resume have no golem-rust host func — report honestly regardless of cluster.
        if let G::AgentInterrupt { pid } | G::AgentResume { pid } = &cmd {
            let verb = if matches!(cmd, G::AgentInterrupt { .. }) { "interrupt" } else { "resume" };
            return LineResult::from_outcome(
                Vec::new(),
                format!(
                    "golem agent {verb}: not available on this SDK surface — agent {verb} (pid {pid}) \
                     is a Golem control-plane operation with no guest host binding\n"
                )
                .into_bytes(),
                2,
            );
        }
        let Some(cluster) = self.golem_cluster.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"golem: requires a configured Golem cluster (unavailable on this target)\n".to_vec(),
                4,
            );
        };
        let result = match cmd {
            G::AgentList => cluster.agent_list().await,
            G::AgentOplog { agent_type, ctor, .. } => cluster.agent_oplog(&agent_type, &ctor).await,
            G::AgentStatus { agent_type, ctor } => cluster.agent_status(&agent_type, &ctor).await,
            G::Connect { identity } => cluster.connect(&identity).await,
            G::Oplog => cluster.self_oplog().await,
            G::Rollback => cluster.rollback().await,
            G::Fork => cluster.fork().await,
            G::AgentInterrupt { .. } | G::AgentResume { .. } => unreachable!(),
        };
        match result {
            Ok(text) => {
                let mut out = text.into_bytes();
                if !out.ends_with(b"\n") {
                    out.push(b'\n');
                }
                LineResult::continue_with_stdout(out)
            }
            Err(e) => LineResult::from_outcome(Vec::new(), format!("golem: {e}\n").into_bytes(), 1),
        }
    }

    /// Run an agent's reserved `oplog`/`status` subcommand via the golem cluster seam (the injected
    /// `AgentInvoker` also fronts these). v1 routes them through a dedicated cluster call; without a
    /// cluster it's the honest error.
    async fn run_agent_reserved(
        &mut self,
        name: &str,
        pkg: &crate::grease::pkg::AgentPackage,
        parsed: &ParsedAgentLine,
        sub: &str,
    ) -> LineResult {
        let Some(cluster) = self.golem_cluster.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                format!(
                    "{name}: '{sub}' requires a configured Golem cluster (unavailable on this target)\n"
                )
                .into_bytes(),
                4,
            );
        };
        let ctor: Vec<(String, String)> = parsed.constructor.clone();
        let result = match sub {
            "oplog" => cluster.agent_oplog(&pkg.agent_type, &ctor).await,
            "status" => cluster.agent_status(&pkg.agent_type, &ctor).await,
            _ => Err("unknown reserved subcommand".to_string()),
        };
        match result {
            Ok(text) => LineResult::continue_with_stdout(text.into_bytes()),
            Err(e) => LineResult::from_outcome(Vec::new(), format!("{name}: {sub}: {e}\n").into_bytes(), 1),
        }
    }

    /// Spawn an `S`-state `AgentInvocation` proc row for a triggered/scheduled invocation, and record
    /// its cancel token so `kill <pid>` can cancel it. Returns the PID.
    fn spawn_agent_invocation_row(&mut self, line: &str, cancel_token: Option<String>) -> u32 {
        let argv: Vec<String> = line.split_whitespace().map(String::from).collect();
        let pid = self.proc_table.lock().unwrap().spawn_bg(
            crate::process::ProcessKind::AgentInvocation,
            argv,
            crate::proctable::SHELL_ROOT_PID,
        );
        self.pending_invocations.push(PendingInvocation { pid, cancel_token });
        pid
    }

    /// Generated help for an installed command-package line ending in `--help` (prompt or script).
    /// `None` if the line isn't an installed command package or doesn't request help.
    fn pkg_help_for(&self, line: &str) -> Option<String> {
        let words = crate::ai::ask::dequote_words(line)?;
        let name = words.first()?;
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

    /// Run one input line for terminal-style callers. This keeps the original API used by the REPLs.
    pub async fn run_line(&mut self, line: &str) -> (Vec<u8>, Flow) {
        let result = self.eval_line(line).await;
        (result.terminal_output(), result.flow)
    }

    /// Whether a `prompt-user` question is currently awaiting a response.
    pub fn has_pending_prompt(&self) -> bool {
        self.pending.is_some()
    }

    /// Handle a `prompt-user` line: parse it, record the pending prompt (durable state), leave the
    /// process row paused (`P`), and return immediately with `pending_prompt` set. The shell does
    /// not block — the caller collects a human answer and delivers it via [`answer_prompt`].
    fn surface_prompt(&mut self, line: &str, pid: Option<u32>) -> LineResult {
        let args = match promptuser::parse(line) {
            Ok(args) => args,
            Err(e) => return self.finish_intercepted(pid, LineResult::stderr(format!("{e}\n"))),
        };
        // Piping into `prompt-user` (`X | prompt-user ...`) is a later increment; for now stdin is
        // never wired, so no markdown is prepended.
        let pending = args.into_pending(None);
        self.surface_pending(pending, pid, PendingKind::UserPrompt)
    }

    /// Surface an authorization confirmation for a gated command: pause, record the pending
    /// confirmation (with the command to run on approval), and return `pending_prompt` immediately.
    fn surface_auth_confirm(
        &mut self,
        command_name: Option<&str>,
        gated_command: String,
        pid: Option<u32>,
        sudo_grant: bool,
        ask_stdin: Option<String>,
    ) -> LineResult {
        let name = command_name.unwrap_or("command");
        // Capability disclosure: a `grease install <pkg>` confirmation discloses what the package is
        // and does (name, source registries, that it runs via `ask` = LLM + shell tools under
        // per-command authz) BEFORE the human approves — README "discloses capability requests before
        // completing". Only what's knowable pre-fetch is shown; declared args are one `grease info`
        // away after install.
        let question = if let Some(question) = self.grease_install_disclosure(&gated_command, sudo_grant)
        {
            question
        } else {
            let synopsis = self
                .registry
                .get(name)
                .map(|m| m.synopsis.clone())
                .or_else(|| self.mcp.manifest_for(name).map(|m| m.synopsis))
                .unwrap_or_else(|| "run this command".to_string());
            authz::confirm_question(name, &synopsis, sudo_grant)
        };
        let prompt = PendingPrompt {
            question,
            choices: Some(authz::confirm_choices(sudo_grant)),
            secret: false,
        };
        self.surface_pending(
            prompt,
            pid,
            PendingKind::AuthConfirm {
                command: gated_command,
                sudo_grant,
                ask_stdin,
            },
        )
    }

    /// If `gated_command` is a `grease install <pkg>` line, build a capability-disclosure confirmation
    /// prompt naming the package, its source registries, and its `ask` capability. `None` otherwise
    /// (the caller falls back to the generic confirm text).
    fn grease_install_disclosure(&self, gated_command: &str, sudo_grant: bool) -> Option<String> {
        let cmd = crate::grease::cmd::classify(gated_command)?.ok()?;
        let crate::grease::cmd::GreaseCommand::Install { name, .. } = cmd else {
            return None;
        };
        let registries = crate::grease::config::list_registries();
        let from = if registries.is_empty() {
            "no configured registry".to_string()
        } else {
            registries.join(", ")
        };
        let tail = if sudo_grant { "(y)es, (n)o" } else { "(y)es, (n)o, (a)ll" };
        // The disclosure fires before the fetch, so the package's kind isn't known yet — disclose the
        // full capability an install can grant: a prompt runs via ask (outbound LLM); a script runs
        // local shell commands; a skill installs model-facing context + `$PATH` scripts. Each is
        // Confirm-gated per run.
        Some(format!(
            "Install package \"{name}\" from {from}? Depending on its kind it may run via ask \
             (outbound LLM), execute local shell commands, or install a skill (model context + \
             $PATH scripts); each is confirmed per run unless you use sudo. {tail}"
        ))
    }

    /// Shared tail of the surface paths: pause the row, record the question, stash the pending
    /// state, and return a `pending_prompt` result.
    fn surface_pending(
        &mut self,
        prompt: PendingPrompt,
        pid: Option<u32>,
        kind: PendingKind,
    ) -> LineResult {
        if let Some(pid) = pid {
            self.proc_table.lock().unwrap().pause(pid);
        }
        let mut stdout = prompt.question.clone().into_bytes();
        stdout.push(b'\n');
        self.transcript.lock().unwrap().record_output(&stdout);

        self.pending = Some(Pending {
            prompt: prompt.clone(),
            pid,
            kind,
        });
        LineResult {
            stdout,
            stderr: Vec::new(),
            exit_code: 0,
            flow: Flow::Continue,
            pending_prompt: Some(prompt),
        }
    }

    /// Deliver a response to the outstanding question (a `prompt-user` prompt or an authorization
    /// confirmation). `response` is `Some(text)` for an answer or `None` for an abort. Resumes the
    /// paused row.
    ///
    /// For a `prompt-user` prompt: a valid answer → the response on stdout, exit `0`; abort → exit
    /// `130`; an answer outside `--choices` → exit `1` with the prompt left pending to re-ask.
    ///
    /// For an authorization confirmation: `yes` (or `all`) → runs the gated command; `all` also
    /// grants blanket `confirm` approval for the session; `no`/abort → exit `5` (denied).
    /// Answer (or abort) the outstanding `prompt-user`/authorization question. Logs the resolved line's
    /// terminal `end` event to shell.log once it finishes (a resolution can itself re-pause — an approved
    /// `ask` whose tool call prompts again — in which case the `end` is deferred to the next resolution).
    pub async fn answer_prompt(&mut self, response: Option<String>) -> LineResult {
        let _log = crate::logging::install(self.log_sink.clone());
        // The paused row's PID, for the shell.log end event (its `start` was logged under this PID).
        let paused_pid = self.pending.as_ref().and_then(|p| p.pid);
        let result = self.answer_prompt_inner(response).await;
        // Only a truly-resolved line (no longer pending) gets its terminal event; a re-pause defers.
        if paused_pid.is_some() && result.pending_prompt.is_none() {
            let mut rec = crate::logging::Record::new("end");
            if let Some(pid) = paused_pid {
                rec = rec.field("pid", pid.to_string());
            }
            rec.field("exit", result.exit_code.to_string())
                .emit(crate::logging::LogFile::Shell);
        }
        result
    }

    async fn answer_prompt_inner(&mut self, response: Option<String>) -> LineResult {
        let Some(pending) = self.pending.take() else {
            self.pending = None;
            return LineResult::stderr("clank: no prompt-user question is awaiting a response\n");
        };

        let answer = match response {
            Some(text) => AnswerInput::Response(text),
            None => AnswerInput::Abort,
        };

        let resolution = promptuser::resolve(&pending.prompt, answer);
        if let Resolution::InvalidChoice { message } = resolution {
            // Prompt stays pending — re-ask. Don't touch the row (still `P`).
            self.pending = Some(pending);
            return LineResult::stderr(message);
        }

        // Resolved: resume the row (it will be reaped by the specific path below).
        if let Some(pid) = pending.pid {
            self.proc_table.lock().unwrap().resume(pid);
        }

        match pending.kind {
            PendingKind::UserPrompt => self.resolve_user_prompt(resolution, pending.pid),
            PendingKind::AuthConfirm {
                command,
                sudo_grant,
                ask_stdin,
            } => {
                // Restore any pre-captured pipeline stdin so a deferred `cat x | ask` tail sees it
                // when re-run (consumed by `run_ask` via `next_ask_stdin`).
                self.next_ask_stdin = ask_stdin;
                self.resolve_auth_confirm(resolution, &command, sudo_grant, pending.pid)
                    .await
            }
            PendingKind::AgentLoop { state, pause } => {
                let result = self
                    .resolve_agent_loop(resolution, state, pause, pending.pid)
                    .await;
                // A resumed ask that completes (not re-paused) records its output like the direct
                // path. A re-pause returns a fresh `pending_prompt`; don't record that as final.
                if result.pending_prompt.is_none() {
                    self.transcript
                        .lock()
                        .unwrap()
                        .record_output(&result.terminal_output());
                }
                result
            }
        }
    }

    /// Resolve a `prompt-user` response: the answer to stdout (exit 0) or an abort (exit 130), reap
    /// the row, and record the transcript (unless `--secret`).
    fn resolve_user_prompt(&mut self, resolution: Resolution, pid: Option<u32>) -> LineResult {
        if let Some(pid) = pid {
            self.proc_table.lock().unwrap().complete(pid);
        }
        let (stdout, exit_code, secret) = match resolution {
            Resolution::Answered { stdout, secret } => (stdout, 0, secret),
            Resolution::Aborted => (Vec::new(), 130, false),
            Resolution::InvalidChoice { .. } => unreachable!("handled by caller"),
        };
        if !secret {
            self.transcript.lock().unwrap().record_output(&stdout);
        }
        LineResult {
            stdout,
            stderr: Vec::new(),
            exit_code,
            flow: Flow::Continue,
            pending_prompt: None,
        }
    }

    /// Resolve an authorization confirmation: on approval run the gated command (recording it), on
    /// denial reap the row and return exit `5`. A response of "all" also sets the session grant.
    async fn resolve_auth_confirm(
        &mut self,
        resolution: Resolution,
        command: &str,
        _sudo_grant: bool,
        pid: Option<u32>,
    ) -> LineResult {
        let approved = matches!(&resolution, Resolution::Answered { stdout, .. }
            if matches!(String::from_utf8_lossy(stdout).trim(), "yes" | "all"));
        let grant_all = matches!(&resolution, Resolution::Answered { stdout, .. }
            if String::from_utf8_lossy(stdout).trim() == "all");

        if !approved {
            // "no" or abort → denied (exit 5). Reap the row. Drop any pre-captured pipeline stdin so
            // it can't leak into an unrelated later `ask`.
            self.next_ask_stdin = None;
            if let Some(pid) = pid {
                self.proc_table.lock().unwrap().complete(pid);
            }
            let result = LineResult::denied();
            self.transcript.lock().unwrap().record_output(&result.terminal_output());
            return result;
        }

        if grant_all {
            self.authz.allow_all = true;
        }

        // Approved: run the gated command, reusing the row (still `R` after resume) and reaping it.
        // `blanket_authorized` is `false` here: approving a bare `ask`'s outbound-HTTP confirmation is
        // not the same as `sudo ask` — the ask's own tool calls still gate individually. A prior "all"
        // grant (now on `self.authz.allow_all`) is separately honored inside the tool executor.
        let blanket = self.authz.allow_all;
        let result = self.run_command(command, pid, blanket).await;
        // `context summarize` is inspection output — never recorded back (like `context show`). Every
        // other gated command records normally.
        if !is_context_summarize(command) {
            self.transcript.lock().unwrap().record_output(&result.terminal_output());
        }
        result
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
        .builtins(crate::ps::builtins())
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
mod tests {
    use super::*;

    /// Drive a closure on a fresh current-thread runtime (mirrors how `Session` is used natively).
    fn on_rt<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// Structured evaluation exposes stdout, stderr, and exit code for agent callers.
    #[test]
    fn eval_line_reports_streams_and_exit_status() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();

            let result = session.eval_line("echo hi").await;
            assert_eq!(String::from_utf8(result.stdout).unwrap(), "hi\n");
            assert!(result.stderr.is_empty());
            assert_eq!(result.exit_code, 0);
            assert_eq!(result.flow, Flow::Continue);

            let result = session.eval_line("false").await;
            assert!(result.stdout.is_empty());
            assert!(result.stderr.is_empty());
            assert_eq!(result.exit_code, 1);
            assert_eq!(result.flow, Flow::Continue);
        });
    }

    /// End-to-end through the public API: a completed command shows `Z` in `ps`, and `ps` sees its
    /// own row as `R` (spawned before execution, completed only after) — like real Unix.
    #[test]
    fn ps_reflects_completed_and_running_rows() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let (_out, _flow) = session.run_line("echo hi").await;
            let (ps_out, _flow) = session.run_line("ps").await;
            let ps_out = String::from_utf8(ps_out).unwrap();

            // The prior `echo hi` line completed → its row is Z.
            let echo_row = ps_out
                .lines()
                .find(|l| l.contains("echo hi"))
                .expect("ps should list the completed `echo hi` line");
            assert!(
                echo_row.contains('Z'),
                "completed line should be Z, got: {echo_row}"
            );

            // The `ps` invocation itself is still running while it renders → its row is R.
            let ps_row = ps_out
                .lines()
                .find(|l| l.trim_end().ends_with("ps"))
                .expect("ps should list itself");
            assert!(
                ps_row.contains('R'),
                "ps's own row should be R, got: {ps_row}"
            );

            // The synthetic root is present.
            assert!(ps_out.contains("clank"));
        });
    }

    /// `prompt-user` is intercepted before Brush dispatch: an invocation Brush would reject
    /// differently (unknown command) instead surfaces `promptuser::parse`'s own error, proving
    /// the interception — not Brush — handled the line. Also proves the line still shows `Z` in
    /// `ps` (the process-table row completes normally for intercepted lines, same as `context`).
    #[test]
    fn prompt_user_is_intercepted_before_brush_dispatch() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let (out, flow) = session.run_line("prompt-user --confirm").await;
            let out = String::from_utf8(out).unwrap();
            assert!(
                out.contains("missing question"),
                "expected promptuser's own parse error, got: {out}"
            );
            assert_eq!(flow, Flow::Continue);

            let (ps_out, _) = session.run_line("ps").await;
            let ps_out = String::from_utf8(ps_out).unwrap();
            let row = ps_out
                .lines()
                .find(|l| l.contains("prompt-user"))
                .expect("ps should list the prompt-user line");
            assert!(row.contains('Z'), "completed line should be Z, got: {row}");
        });
    }

    /// `type` for a clank-intercepted command resolves through clank's own dispatch (Brush's `type`
    /// can't see it): `type curl` → "curl is a shell builtin", exit 0. This is the README's "type
    /// authoritative for all commands" made true end-to-end through `eval_line`.
    #[test]
    fn type_resolves_intercepted_command_as_builtin() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            for name in typecmd::INTERCEPTED {
                let result = session.eval_line(&format!("type {name}")).await;
                assert_eq!(result.exit_code, 0, "type {name} should exit 0");
                assert_eq!(
                    String::from_utf8(result.stdout).unwrap(),
                    format!("{name} is a shell builtin\n"),
                    "type {name} should report a shell builtin"
                );
            }

            // `-t` prints the bare word, like Brush.
            let result = session.eval_line("type -t curl").await;
            assert_eq!(String::from_utf8(result.stdout).unwrap(), "builtin\n");
        });
    }

    /// `type` for a Brush-registered builtin (`cat`) falls through to Brush unchanged — clank does
    /// NOT intercept it. Proves the fallthrough half of the design: clank owns only the intercepted
    /// names, Brush keeps everything else.
    #[test]
    fn type_falls_through_to_brush_for_registered_builtin() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let result = session.eval_line("type cat").await;
            let out = String::from_utf8(result.terminal_output()).unwrap();
            // Brush's `type` resolves `cat` (a registered builtin) — clank didn't short-circuit it.
            assert!(
                out.contains("cat") && out.contains("builtin"),
                "Brush's type should resolve cat as a builtin, got: {out}"
            );
        });
    }

    /// What a [`FakeProvider`] recorded about a single `turn` call, so tests can assert what context
    /// `ask` assembled (transcript-as-context) and which model/tools it used.
    #[derive(Clone, Default)]
    struct SeenTurn {
        system: Option<String>,
        history: Vec<crate::ai::ask::AskTurn>,
        tools: Vec<crate::ai::ask::AskTool>,
        model: String,
    }

    impl SeenTurn {
        /// The prompt text from the first user turn (the transcript-as-context body). Mirrors the old
        /// `AskRequest.prompt`/`transcript` accessors the tests used.
        fn user_content(&self) -> String {
            match self.history.first() {
                Some(crate::ai::ask::AskTurn::User(s)) => s.clone(),
                _ => String::new(),
            }
        }
    }

    /// A fake `AskProvider` for tests: replays a scripted queue of [`AskResponse`]s (one per turn) and
    /// records every `turn` call it saw. A single-response script is the common one-turn case.
    #[derive(Clone, Default)]
    struct FakeProvider {
        /// Scripted responses, consumed front-to-back. When exhausted, a terminal empty text is
        /// returned (so a mis-scripted test terminates rather than looping).
        scripted: std::sync::Arc<Mutex<std::collections::VecDeque<crate::ai::ask::AskResponse>>>,
        /// Every `turn` call, in order.
        seen: std::sync::Arc<Mutex<Vec<SeenTurn>>>,
    }

    impl FakeProvider {
        /// A provider that replies once with `reply` text and records what it saw.
        fn reply(reply: &str, seen: std::sync::Arc<Mutex<Vec<SeenTurn>>>) -> Self {
            Self::scripted(
                vec![crate::ai::ask::AskResponse::text(reply)],
                seen,
            )
        }

        /// A provider driven by an explicit script of per-turn responses.
        fn scripted(
            responses: Vec<crate::ai::ask::AskResponse>,
            seen: std::sync::Arc<Mutex<Vec<SeenTurn>>>,
        ) -> Self {
            Self {
                scripted: std::sync::Arc::new(Mutex::new(responses.into())),
                seen,
            }
        }
    }

    /// A single-`shell`-tool-call response for scripting the agentic loop in tests.
    fn shell_tool_call(id: &str, command: &str) -> crate::ai::ask::AskResponse {
        crate::ai::ask::AskResponse {
            text: String::new(),
            tool_calls: vec![crate::ai::ask::AskToolCall {
                id: id.to_string(),
                name: crate::ai::ask::SHELL_TOOL.to_string(),
                arguments_json: serde_json::json!({ "command": command }).to_string(),
            }],
            finished_for_tools: true,
            error: None,
        }
    }

    /// The tool result the loop fed back for `call_id` in the most recent `ToolResults` turn the
    /// provider saw, if any.
    fn last_tool_result(
        seen: &std::sync::Arc<Mutex<Vec<SeenTurn>>>,
        call_id: &str,
    ) -> Option<crate::ai::ask::AskToolResult> {
        let turns = seen.lock().unwrap();
        for st in turns.iter().rev() {
            for turn in st.history.iter().rev() {
                if let crate::ai::ask::AskTurn::ToolResults(results) = turn {
                    if let Some(r) = results.iter().find(|r| r.id == call_id) {
                        return Some(r.clone());
                    }
                }
            }
        }
        None
    }

    #[async_trait::async_trait(?Send)]
    impl crate::ai::ask::AskProvider for FakeProvider {
        async fn turn(
            &self,
            system: Option<&str>,
            history: &[crate::ai::ask::AskTurn],
            tools: &[crate::ai::ask::AskTool],
            model: &str,
        ) -> crate::ai::ask::AskResponse {
            self.seen.lock().unwrap().push(SeenTurn {
                system: system.map(str::to_string),
                history: history.to_vec(),
                tools: tools.to_vec(),
                model: model.to_string(),
            });
            self.scripted
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| crate::ai::ask::AskResponse::text(""))
        }
    }

    // ---- MCP core (C1) --------------------------------------------------------------------------

    /// A scripted [`McpHttp`](crate::mcp::client::McpHttp) fake: replays JSON responses and records
    /// requests. Shared by the MCP session tests.
    struct FakeMcpHttp {
        responses: std::sync::Arc<Mutex<std::collections::VecDeque<crate::mcp::client::HttpResponse>>>,
        seen: std::sync::Arc<Mutex<Vec<(String, String)>>>,
    }

    impl FakeMcpHttp {
        fn new(responses: Vec<crate::mcp::client::HttpResponse>) -> Self {
            Self {
                responses: std::sync::Arc::new(Mutex::new(responses.into())),
                seen: std::sync::Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl crate::mcp::client::McpHttp for FakeMcpHttp {
        async fn request(
            &self,
            method: &str,
            url: &str,
            _headers: &[(String, String)],
            body: Option<Vec<u8>>,
        ) -> Result<crate::mcp::client::HttpResponse, String> {
            let method_and_body =
                format!("{method} {}", String::from_utf8_lossy(&body.unwrap_or_default()));
            self.seen.lock().unwrap().push((url.to_string(), method_and_body));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "no scripted response".to_string())
        }
    }

    fn mcp_json(value: serde_json::Value) -> crate::mcp::client::HttpResponse {
        crate::mcp::client::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: value.to_string().into_bytes(),
        }
    }

    /// A URL-routed fake HTTP transport for grease tests: unlike the order-based `FakeMcpHttp`, it maps
    /// a URL substring → response, so grease's `index.json` + `packages/<name>.json` fetches don't
    /// collide. An unmatched URL is a 404.
    struct FakeGreaseHttp {
        routes: Vec<(String, crate::mcp::client::HttpResponse)>,
    }
    impl FakeGreaseHttp {
        fn new(routes: Vec<(&str, crate::mcp::client::HttpResponse)>) -> Self {
            Self { routes: routes.into_iter().map(|(u, r)| (u.to_string(), r)).collect() }
        }
    }
    #[async_trait::async_trait(?Send)]
    impl crate::mcp::client::McpHttp for FakeGreaseHttp {
        async fn request(
            &self,
            _method: &str,
            url: &str,
            _headers: &[(String, String)],
            _body: Option<Vec<u8>>,
        ) -> Result<crate::mcp::client::HttpResponse, String> {
            for (pat, resp) in &self.routes {
                if url.contains(pat.as_str()) {
                    return Ok(resp.clone());
                }
            }
            Ok(crate::mcp::client::HttpResponse { status: 404, headers: vec![], body: Vec::new() })
        }
    }

    /// A 200 JSON response (for `FakeGreaseHttp` routes).
    fn grease_json(value: serde_json::Value) -> crate::mcp::client::HttpResponse {
        crate::mcp::client::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: value.to_string().into_bytes(),
        }
    }

    /// A 200 text response (for a `.md` prompt body served by `FakeGreaseHttp`).
    fn grease_text(body: &str) -> crate::mcp::client::HttpResponse {
        crate::mcp::client::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/markdown".into())],
            body: body.as_bytes().to_vec(),
        }
    }

    /// A fresh temp `$CLANK_GREASE_*` triple, exported for the duration of the guard (serializes grease
    /// tests via the shared lock, clears the vars on drop).
    struct GreaseDirsGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl Drop for GreaseDirsGuard {
        fn drop(&mut self) {
            std::env::remove_var("CLANK_GREASE_ETC");
            std::env::remove_var("CLANK_GREASE_STORE");
            std::env::remove_var("CLANK_GREASE_BIN");
            std::env::remove_var("CLANK_GREASE_SCRIPT_BIN");
            std::env::remove_var("CLANK_GREASE_SKILLS");
            std::env::remove_var("CLANK_GREASE_MCP_MOUNT");
            std::env::remove_var("CLANK_GREASE_AGENT_BIN");
        }
    }
    fn set_grease_dirs() -> GreaseDirsGuard {
        let lock = crate::grease::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("clank_grease_sess_{}_{n}", std::process::id()));
        for sub in ["etc", "store", "bin", "script-bin", "skills", "mnt-mcp", "agent-bin"] {
            std::fs::create_dir_all(base.join(sub)).unwrap();
        }
        std::env::set_var("CLANK_GREASE_ETC", base.join("etc"));
        std::env::set_var("CLANK_GREASE_STORE", base.join("store"));
        std::env::set_var("CLANK_GREASE_BIN", base.join("bin"));
        std::env::set_var("CLANK_GREASE_MCP_MOUNT", base.join("mnt-mcp"));
        std::env::set_var("CLANK_GREASE_AGENT_BIN", base.join("agent-bin"));
        std::env::set_var("CLANK_GREASE_SCRIPT_BIN", base.join("script-bin"));
        std::env::set_var("CLANK_GREASE_SKILLS", base.join("skills"));
        GreaseDirsGuard { _lock: lock }
    }

    struct McpDirsGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        bin: String,
    }
    impl Drop for McpDirsGuard {
        fn drop(&mut self) {
            std::env::remove_var("CLANK_MCP_ETC");
            std::env::remove_var("CLANK_MCP_BIN");
        }
    }

    /// A fresh temp `$CLANK_MCP_ETC/$CLANK_MCP_BIN` pair, exported for the duration of the returned
    /// guard (which serializes MCP tests via the shared lock and clears the vars on drop).
    fn set_mcp_dirs() -> McpDirsGuard {
        let lock = crate::mcp::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("clank_mcp_sess_{}_{n}", std::process::id()));
        let etc = base.join("etc");
        let bin = base.join("bin");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::env::set_var("CLANK_MCP_ETC", &etc);
        std::env::set_var("CLANK_MCP_BIN", &bin);
        McpDirsGuard {
            _lock: lock,
            bin: bin.to_str().unwrap().to_string(),
        }
    }

    /// The initialize + initialized + tools/list responses for a server offering one `echo` tool.
    fn mcp_install_script() -> Vec<crate::mcp::client::HttpResponse> {
        let mut init = mcp_json(serde_json::json!({
            "jsonrpc":"2.0","id":1,"result":{
                "protocolVersion":"2025-03-26",
                "serverInfo":{"name":"demo","version":"1.0"},
                "capabilities":{"tools":{}}}}));
        init.headers.push(("mcp-session-id".into(), "srv-1".into()));
        vec![
            init,
            mcp_json(serde_json::json!({})), // initialized notification
            mcp_json(serde_json::json!({"jsonrpc":"2.0","id":2,"result":{
                "tools":[{"name":"echo","description":"echoes input",
                          "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}})),
        ]
    }

    /// A hybrid fake for grease-MCP install tests: grease registry URLs (`/index.json`, `/packages/`)
    /// are matched by URL substring; the MCP endpoint (any other URL) is answered by JSON-RPC method
    /// name parsed from the request body. Covers initialize/tools/list/prompts/list/prompts/get/
    /// resources/list/resources/read.
    struct FakeMcpArtifactHttp {
        routes: Vec<(String, crate::mcp::client::HttpResponse)>,
    }
    impl FakeMcpArtifactHttp {
        fn new(routes: Vec<(&str, crate::mcp::client::HttpResponse)>) -> Self {
            Self { routes: routes.into_iter().map(|(u, r)| (u.to_string(), r)).collect() }
        }
    }
    #[async_trait::async_trait(?Send)]
    impl crate::mcp::client::McpHttp for FakeMcpArtifactHttp {
        async fn request(
            &self,
            _method: &str,
            url: &str,
            _headers: &[(String, String)],
            body: Option<Vec<u8>>,
        ) -> Result<crate::mcp::client::HttpResponse, String> {
            // Grease registry fetches route by URL.
            for (pat, resp) in &self.routes {
                if pat.starts_with('/') && url.contains(pat.as_str()) {
                    return Ok(resp.clone());
                }
            }
            // Otherwise it's an MCP JSON-RPC POST — route by the `method` field in the body. For
            // `resources/read`, a more specific `resources/read:<uri>` route wins (lets a test make one
            // resource's read fail → dynamic while another succeeds → static).
            let body = body.unwrap_or_default();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
            let m = v.get("method").and_then(|x| x.as_str()).unwrap_or("");
            if m == "resources/read" {
                if let Some(uri) = v.get("params").and_then(|p| p.get("uri")).and_then(|u| u.as_str()) {
                    let keyed = format!("resources/read:{uri}");
                    for (pat, resp) in &self.routes {
                        if *pat == keyed {
                            return Ok(resp.clone());
                        }
                    }
                }
            }
            for (pat, resp) in &self.routes {
                if pat == m {
                    return Ok(resp.clone());
                }
            }
            // Unmapped MCP method → an empty-result success (notifications, etc.).
            Ok(mcp_json(serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}})))
        }
    }

    /// End-to-end: `grease install <server>` for a `kind:mcp` package fetches the server's live surface
    /// (initialize + tools/list + prompts/list + prompts/get + resources/list/read), registers the
    /// tools into `McpState` (so `<server> <tool>` works), materializes prompts as $PATH commands and
    /// static resources under /mnt/mcp, and caches the surface so a fresh Session rebuilds it offline.
    #[test]
    fn grease_install_an_mcp_server_registers_tools_prompts_resources() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let _mcp = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();

            // The grease registry payload: a minimal mcp package pointing at the server URL.
            let pkg = serde_json::json!({
                "kind": "mcp", "name": "demo",
                "description": "a demo MCP server", "url": "https://mcp.demo/x"
            });
            let mut init = mcp_json(serde_json::json!({
                "jsonrpc":"2.0","id":1,"result":{
                    "protocolVersion":"2025-03-26","serverInfo":{"name":"demo","version":"1"},
                    "capabilities":{"tools":{},"prompts":{},"resources":{}}}}));
            init.headers.push(("mcp-session-id".into(), "s-1".into()));
            let tools = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":2,"result":{
                "tools":[{"name":"echo","description":"echo it",
                    "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}}));
            let prompts_list = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":3,"result":{
                "prompts":[{"name":"summarize-diff","description":"summarize a diff"}]}}));
            let prompts_get = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":4,"result":{
                "messages":[{"role":"user","content":{"type":"text","text":"Summarize this diff."}}]}}));
            let res_list = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":5,"result":{
                "resources":[{"uri":"file:///repo/README.md","name":"readme","mimeType":"text/plain"}]}}));
            let res_read = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":6,"result":{
                "contents":[{"uri":"file:///repo/README.md","text":"# Hello from the resource"}]}}));

            session.set_mcp_http(Box::new(FakeMcpArtifactHttp::new(vec![
                ("/packages/", grease_json(pkg)),
                ("initialize", init),
                ("tools/list", tools),
                ("prompts/list", prompts_list),
                ("prompts/get", prompts_get),
                ("resources/list", res_list),
                ("resources/read", res_read),
            ])));
            session.run_line("grease registry add https://reg.example").await;

            let inst = session.eval_line("sudo grease install demo").await;
            assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
            let out = String::from_utf8(inst.stdout).unwrap();
            assert!(out.contains("installed demo [mcp]"), "install output: {out}");
            assert!(out.contains("1 tools") && out.contains("1 prompts"), "counts: {out}");

            // The server is registered in McpState: `<server> <tool>` is a recognized tool line.
            assert!(session.is_mcp_tool_line("demo echo --text hi"));
            assert!(session.grease.is_mcp("demo"));

            // The prompt was materialized as a standalone $PATH prompt.
            assert!(session.grease.is_prompt("summarize-diff"));

            // The static resource was materialized under /mnt/mcp/demo/.
            let res_path = crate::grease::config::mcp_mount_dir().join("demo/repo/README.md");
            assert_eq!(
                std::fs::read_to_string(&res_path).unwrap(),
                "# Hello from the resource"
            );

            // `grease info demo` describes the server + its artifacts.
            let info = String::from_utf8(session.eval_line("grease info demo").await.stdout).unwrap();
            assert!(info.contains("[mcp]") && info.contains("https://mcp.demo/x"), "info: {info}");
            assert!(info.contains("echo") && info.contains("summarize-diff"), "info lists artifacts: {info}");

            // A FRESH Session rebuilds the tool surface from the cached payload (no live fetch).
            let session2 = Session::new().await.unwrap();
            assert!(session2.is_mcp_tool_line("demo echo --text hi"), "boot reconstruction failed");

            // Remove deregisters from McpState + deletes the resource mount.
            let rm = session.eval_line("sudo grease remove demo").await;
            assert_eq!(rm.exit_code, 0);
            assert!(!session.grease.is_mcp("demo"));
            assert!(!session.is_mcp_tool_line("demo echo --text hi"));
        });
    }

    /// The /mnt/mcp virtual-fs: an MCP server with one STATIC resource (readable at install → real
    /// file) and one DYNAMIC resource (install read fails → served live). `ls /mnt/mcp/<server>/` lists
    /// both; a top-level `cat` of the dynamic resource fetches it live via resources/read; the same cat
    /// inside `$()` hits the honest Wall-C stub.
    #[test]
    fn mcp_resources_virtual_fs_static_and_dynamic() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let _mcp = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();

            let pkg = serde_json::json!({
                "kind": "mcp", "name": "srv", "description": "s", "url": "https://mcp.srv/x"
            });
            let mut init = mcp_json(serde_json::json!({
                "jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26",
                "serverInfo":{"name":"srv","version":"1"},"capabilities":{"resources":{}}}}));
            init.headers.push(("mcp-session-id".into(), "s-1".into()));
            let res_list = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"resources":[
                {"uri":"file:///docs/guide.md","name":"guide"},
                {"uri":"live://metrics/cpu","name":"cpu"}]}}));
            // Static resource: install read succeeds → materialized as a real file.
            let static_read = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":3,"result":{
                "contents":[{"uri":"file:///docs/guide.md","text":"static guide body"}]}}));
            // Dynamic resource: install read FAILS (500) → recorded dynamic; a later live read succeeds.
            let dyn_read_fail = crate::mcp::client::HttpResponse { status: 500, headers: vec![], body: Vec::new() };
            let dyn_read_ok = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":9,"result":{
                "contents":[{"uri":"live://metrics/cpu","text":"cpu: 42%"}]}}));

            // Two calls to the dynamic URI: first (install) fails, second (live cat) succeeds. The fake
            // routes by exact key, so use a queue-per-URI via two separate installs of the session http.
            session.set_mcp_http(Box::new(FakeMcpArtifactHttp::new(vec![
                ("/packages/", grease_json(pkg)),
                ("initialize", init.clone()),
                ("resources/list", res_list.clone()),
                ("resources/read:file:///docs/guide.md", static_read),
                ("resources/read:live://metrics/cpu", dyn_read_fail),
            ])));
            session.run_line("grease registry add https://reg.example").await;

            let inst = session.eval_line("sudo grease install srv --resources").await;
            assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));

            // The static resource is a real file.
            let static_path = crate::grease::config::mcp_mount_dir().join("srv/docs/guide.md");
            assert_eq!(std::fs::read_to_string(&static_path).unwrap(), "static guide body");

            // `ls /mnt/mcp/srv/` lists both the static dir (docs) and the dynamic dir (metrics).
            let ls = String::from_utf8(session.eval_line("ls /mnt/mcp/srv").await.stdout).unwrap();
            assert!(ls.contains("docs") && ls.contains("metrics"), "ls: {ls}");
            // `ls /mnt/mcp` lists the server.
            let ls_root = String::from_utf8(session.eval_line("ls /mnt/mcp").await.stdout).unwrap();
            assert!(ls_root.contains("srv"), "ls root: {ls_root}");

            // Now point the dynamic URI at a SUCCESSFUL read for the live `cat`.
            session.set_mcp_http(Box::new(FakeMcpArtifactHttp::new(vec![
                ("initialize", init),
                ("resources/read:live://metrics/cpu", dyn_read_ok),
            ])));
            // Top-level `cat` of the dynamic resource fetches it live.
            let cat = session.eval_line("cat /mnt/mcp/srv/metrics/cpu").await;
            assert_eq!(cat.exit_code, 0, "dynamic cat stderr: {}", String::from_utf8_lossy(&cat.stderr));
            assert_eq!(String::from_utf8(cat.stdout).unwrap(), "cpu: 42%");

            // The same read inside `$()` does NOT do the live fetch (Wall-C) — Brush's cat finds no
            // real file → nonzero. (We only assert it doesn't crash / doesn't print the live body.)
            let subst = session.eval_line("echo $(cat /mnt/mcp/srv/metrics/cpu)").await;
            assert!(!String::from_utf8_lossy(&subst.stdout).contains("cpu: 42%"), "dynamic read must not run in $()");
        });
    }

    /// MCP resource templates + `mcp resource info` + `stat`. Install a server with a resource template
    /// (`resources/templates/list`) → a `<server>-<name>` executable; running it substitutes the arg
    /// into the URI template and reads the constructed resource. `mcp resource info` shows annotations;
    /// `ls /mnt/mcp/<server>` lists the template stub.
    #[test]
    fn mcp_templates_and_resource_info() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let _mcp = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();

            let pkg = serde_json::json!({
                "kind": "mcp", "name": "gh", "description": "github", "url": "https://mcp.gh/x"
            });
            let mut init = mcp_json(serde_json::json!({
                "jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26",
                "serverInfo":{"name":"gh","version":"1"},"capabilities":{"resources":{}}}}));
            init.headers.push(("mcp-session-id".into(), "s-1".into()));
            // One static resource (with annotations) + one template.
            let res_list = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"resources":[
                {"uri":"file:///repo/README.md","name":"readme","mimeType":"text/markdown",
                 "size":42,"annotations":{"lastModified":"2026-01-01T00:00:00Z","priority":0.8,
                 "audience":["user","assistant"]}}]}}));
            let static_read = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":3,"result":{
                "contents":[{"uri":"file:///repo/README.md","text":"readme body"}]}}));
            let tmpl_list = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":4,"result":{
                "resourceTemplates":[{"uriTemplate":"github://repo/{path}","name":"file-lookup",
                 "description":"look up a repo file"}]}}));
            // The template read: constructed URI github://repo/src/main.rs.
            let tmpl_read = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":5,"result":{
                "contents":[{"uri":"github://repo/src/main.rs","text":"fn main() {}"}]}}));

            session.set_mcp_http(Box::new(FakeMcpArtifactHttp::new(vec![
                ("/packages/", grease_json(pkg)),
                ("initialize", init.clone()),
                ("resources/list", res_list),
                ("resources/templates/list", tmpl_list),
                ("resources/read:file:///repo/README.md", static_read),
                ("resources/read:github://repo/src/main.rs", tmpl_read),
            ])));
            session.run_line("grease registry add https://reg.example").await;

            let inst = session.eval_line("sudo grease install gh --resources").await;
            assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
            assert!(String::from_utf8(inst.stdout).unwrap().contains("1 templates"), "reports templates");

            // The template executable exists (`type`/`ls` see it).
            let ls = String::from_utf8(session.eval_line("ls /mnt/mcp/gh").await.stdout).unwrap();
            assert!(ls.contains("gh-file-lookup"), "template stub listed: {ls}");

            // Running the template with a positional arg substitutes {path} and reads the URI.
            let run = session.eval_line("gh-file-lookup src/main.rs").await;
            assert_eq!(run.exit_code, 0, "template run stderr: {}", String::from_utf8_lossy(&run.stderr));
            assert_eq!(String::from_utf8(run.stdout).unwrap(), "fn main() {}");

            // `mcp resource info` shows the annotations.
            let info = String::from_utf8(
                session.eval_line("mcp resource info /mnt/mcp/gh/repo/README.md").await.stdout,
            )
            .unwrap();
            assert!(info.contains("2026-01-01"), "info shows lastModified: {info}");
            assert!(info.contains("priority: 0.8"), "info shows priority: {info}");
            assert!(info.contains("user,assistant"), "info shows audience: {info}");
        });
    }

    /// `mcp watch <uri>` is a bounded poll (not a push stream) — it reads the resource N times and
    /// stops, honest about the limitation.
    #[test]
    fn mcp_watch_is_a_bounded_poll() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let _mcp = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();
            let pkg = serde_json::json!({
                "kind": "mcp", "name": "metrics", "description": "m", "url": "https://mcp.m/x"
            });
            let mut init = mcp_json(serde_json::json!({
                "jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26",
                "serverInfo":{"name":"metrics","version":"1"},"capabilities":{"resources":{}}}}));
            init.headers.push(("mcp-session-id".into(), "s-1".into()));
            // resources/list makes the server own the metrics:// uri.
            let res_list = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"resources":[
                {"uri":"metrics://cpu","name":"cpu"}]}}));
            // resources/read for the dynamic uri (install read fails → dynamic; watch reads succeed).
            let read_ok = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":9,"result":{
                "contents":[{"uri":"metrics://cpu","text":"cpu: 10%"}]}}));

            session.set_mcp_http(Box::new(FakeMcpArtifactHttp::new(vec![
                ("/packages/", grease_json(pkg)),
                ("initialize", init),
                ("resources/list", res_list),
                ("resources/templates/list", mcp_json(serde_json::json!({"jsonrpc":"2.0","id":3,"result":{"resourceTemplates":[]}}))),
                ("resources/read:metrics://cpu", read_ok),
                ("resources/subscribe", mcp_json(serde_json::json!({"jsonrpc":"2.0","id":4,"result":{}}))),
            ])));
            session.run_line("grease registry add https://reg.example").await;
            session.eval_line("sudo grease install metrics --resources").await;

            let watch = session.eval_line("mcp watch metrics://cpu").await;
            assert_eq!(watch.exit_code, 0);
            let out = String::from_utf8(watch.stdout).unwrap();
            assert!(out.contains("bounded poll"), "honest about polling: {out}");
            assert!(out.contains("cpu: 10%"), "prints the resource content: {out}");
            assert!(out.contains("done"), "terminates: {out}");
        });
    }

    /// A scripted [`crate::golem::agent::AgentInvoker`]: records the invocation it saw and returns a fixed
    /// reply (the native stand-in for the durable `WasmRpc` binding, which needs a cluster).
    struct FakeAgentInvoker {
        reply: String,
        seen: std::sync::Arc<Mutex<Option<crate::golem::agent::AgentInvocation>>>,
    }
    #[async_trait::async_trait(?Send)]
    impl crate::golem::agent::AgentInvoker for FakeAgentInvoker {
        async fn invoke(
            &self,
            inv: &crate::golem::agent::AgentInvocation,
        ) -> Result<String, String> {
            *self.seen.lock().unwrap() = Some(inv.clone());
            Ok(self.reply.clone())
        }
        async fn invoke_async(
            &self,
            inv: &crate::golem::agent::AgentInvocation,
        ) -> Result<crate::golem::agent::InvokeHandle, String> {
            *self.seen.lock().unwrap() = Some(inv.clone());
            let (token, note) = match &inv.mode {
                crate::golem::agent::InvokeMode::Trigger => (None, "triggered".to_string()),
                crate::golem::agent::InvokeMode::Schedule(w) => {
                    (Some("tok".to_string()), format!("scheduled for {w}"))
                }
                crate::golem::agent::InvokeMode::Await => (None, String::new()),
            };
            Ok(crate::golem::agent::InvokeHandle { cancel_token: token, note })
        }
    }

    /// A scripted [`crate::golem::cluster::GolemCluster`] recording the calls it saw.
    struct FakeGolemCluster;
    #[async_trait::async_trait(?Send)]
    impl crate::golem::cluster::GolemCluster for FakeGolemCluster {
        async fn agent_list(&self) -> Result<String, String> {
            Ok("agent-1\nagent-2".to_string())
        }
        async fn agent_oplog(&self, t: &str, _c: &[(String, String)]) -> Result<String, String> {
            Ok(format!("oplog for {t}"))
        }
        async fn agent_status(&self, t: &str, _c: &[(String, String)]) -> Result<String, String> {
            Ok(format!("status for {t}"))
        }
        async fn connect(&self, id: &str) -> Result<String, String> {
            Ok(format!("connected to {id}"))
        }
        async fn self_oplog(&self) -> Result<String, String> {
            Ok("self oplog".to_string())
        }
        async fn rollback(&self) -> Result<String, String> {
            Ok("rolled back".to_string())
        }
        async fn fork(&self) -> Result<String, String> {
            Ok("forked".to_string())
        }
    }

    /// End-to-end: `grease install golem:<name>` registers a `/usr/lib/agents/bin/<name>` command;
    /// running `<agent> --<ctor> v <method> -- --<arg> v` parses the invocation and dispatches it
    /// through the injected invoker (await mode), printing the result. Missing method → exit 2; no
    /// invoker → honest "needs a cluster".
    #[test]
    fn grease_install_then_invoke_a_golem_agent() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(None));
            session.set_agent_invoker(Box::new(FakeAgentInvoker {
                reply: "added sku abc123".into(),
                seen: seen.clone(),
            }));

            let pkg = serde_json::json!({
                "kind": "agent", "name": "shopping-cart",
                "description": "a shopping cart", "agent-type": "ShoppingCart",
                "constructor-params": ["userid"],
                "methods": [{"name": "add-item", "description": "add an item", "params": ["sku"]}]
            });
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
            session.run_line("grease registry add https://reg.example").await;

            let inst = session.eval_line("sudo grease install shopping-cart").await;
            assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
            assert!(String::from_utf8(inst.stdout).unwrap().contains("[agent]"));
            assert!(session.grease.is_agent("shopping-cart"));
            // The agent bin stub landed in the agents bin dir.
            assert!(crate::grease::config::agent_bin_dir().join("shopping-cart").exists());

            // `--help` describes the type + methods (no invocation).
            let help = session.eval_line("shopping-cart --help").await;
            assert_eq!(help.exit_code, 0);
            let help_s = String::from_utf8(help.stdout).unwrap();
            assert!(help_s.contains("ShoppingCart") && help_s.contains("add-item"), "help: {help_s}");

            // An unknown method → exit 2 (no invocation).
            let bad = session.eval_line("sudo shopping-cart --userid jd frobnicate").await;
            assert_eq!(bad.exit_code, 2);
            assert!(String::from_utf8(bad.stderr).unwrap().contains("unknown method"));
            assert!(seen.lock().unwrap().is_none(), "no invocation on unknown method");

            // Invoke it (sudo pre-authorizes the Confirm) → the invoker sees the parsed invocation.
            let run = session.eval_line("sudo shopping-cart --userid jd add-item -- --sku abc123").await;
            assert_eq!(run.exit_code, 0, "run stderr: {}", String::from_utf8_lossy(&run.stderr));
            assert_eq!(String::from_utf8(run.stdout).unwrap().trim_end(), "added sku abc123");
            let inv = seen.lock().unwrap().clone().unwrap();
            assert_eq!(inv.agent_type, "ShoppingCart");
            assert_eq!(inv.constructor, vec![("userid".to_string(), "jd".to_string())]);
            assert_eq!(inv.method, "add-item");
            assert_eq!(inv.args, vec![("sku".to_string(), "abc123".to_string())]);

            // A bare (non-sudo) agent run confirms (remote invocation is a Confirm capability).
            let confirm = session.eval_line("shopping-cart --userid jd add-item -- --sku x").await;
            assert!(confirm.pending_prompt.is_some(), "agent run should confirm without sudo");
            session.answer_prompt(Some("no".into())).await;

            // Remove deregisters + deletes the stub.
            let rm = session.eval_line("sudo grease remove shopping-cart").await;
            assert_eq!(rm.exit_code, 0);
            assert!(!session.grease.is_agent("shopping-cart"));
            assert!(!crate::grease::config::agent_bin_dir().join("shopping-cart").exists());
        });
    }

    /// Without an injected invoker (the native default), an installed agent command reports an honest
    /// "needs a cluster" error (exit 4) rather than crashing.
    #[test]
    fn agent_invocation_without_a_cluster_errors_honestly() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap(); // no set_agent_invoker
            let pkg = serde_json::json!({
                "kind": "agent", "name": "counter", "description": "c", "agent-type": "Counter",
                "constructor-params": ["id"],
                "methods": [{"name": "increment", "params": []}]
            });
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
            session.run_line("grease registry add https://reg.example").await;
            session.eval_line("sudo grease install counter").await;

            let run = session.eval_line("sudo counter --id x increment").await;
            assert_eq!(run.exit_code, 4);
            assert!(String::from_utf8(run.stderr).unwrap().contains("requires a configured cluster"));
        });
    }

    /// Install a `golem:shopping-cart` agent (helper) — a durable ShoppingCart with one method.
    async fn install_shopping_cart(session: &mut Session) {
        let pkg = serde_json::json!({
            "kind": "agent", "name": "shopping-cart", "description": "cart",
            "agent-type": "ShoppingCart", "constructor-params": ["userid"],
            "methods": [{"name": "add-item", "params": ["sku"]}], "ephemeral": false
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
        session.run_line("grease registry add https://reg.example").await;
        session.eval_line("sudo grease install shopping-cart").await;
    }

    /// `--trigger` invokes in fire-and-forget mode: the invoker sees Trigger, a PID row is spawned, and
    /// `kill <pid>` clears the tracking (the fire-and-forget can't be cancelled remotely).
    #[test]
    fn agent_trigger_mode_and_kill() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(None));
            session.set_agent_invoker(Box::new(FakeAgentInvoker { reply: String::new(), seen: seen.clone() }));
            install_shopping_cart(&mut session).await;

            let run = session.eval_line("sudo shopping-cart --userid jd --trigger add-item -- --sku abc").await;
            assert_eq!(run.exit_code, 0, "trigger stderr: {}", String::from_utf8_lossy(&run.stderr));
            let out = String::from_utf8(run.stdout).unwrap();
            assert!(out.contains("triggered"), "reports triggered: {out}");
            let inv = seen.lock().unwrap().clone().unwrap();
            assert_eq!(inv.mode, crate::golem::agent::InvokeMode::Trigger);
            assert_eq!(inv.args, vec![("sku".to_string(), "abc".to_string())]);

            // The trigger spawned a PID row; `kill <pid>` clears it. Extract the pid from "[<pid>] …".
            let pid: u32 = out.trim_start_matches('[').split(']').next().unwrap().parse().unwrap();
            let kill = session.eval_line(&format!("kill {pid}")).await;
            assert_eq!(kill.exit_code, 0);
            assert!(String::from_utf8(kill.stdout).unwrap().contains("cannot cancel"), "fire-and-forget");
        });
    }

    /// `--schedule` reaches Schedule mode with a cancel token; `kill` reports it cancelled.
    #[test]
    fn agent_schedule_mode_and_kill_cancels() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(None));
            session.set_agent_invoker(Box::new(FakeAgentInvoker { reply: String::new(), seen: seen.clone() }));
            install_shopping_cart(&mut session).await;

            let run = session
                .eval_line("sudo shopping-cart --userid jd --schedule 2026-06-01T09:00:00Z add-item -- --sku x")
                .await;
            assert_eq!(run.exit_code, 0, "schedule stderr: {}", String::from_utf8_lossy(&run.stderr));
            let out = String::from_utf8(run.stdout).unwrap();
            assert!(out.contains("scheduled for 2026-06-01"), "reports schedule: {out}");
            assert_eq!(
                seen.lock().unwrap().clone().unwrap().mode,
                crate::golem::agent::InvokeMode::Schedule("2026-06-01T09:00:00Z".to_string())
            );
            let pid: u32 = out.trim_start_matches('[').split(']').next().unwrap().parse().unwrap();
            let kill = session.eval_line(&format!("kill {pid}")).await;
            assert!(String::from_utf8(kill.stdout).unwrap().contains("cancelled"), "scheduled cancels");
        });
    }

    /// The honest-stubbed features: `--revision`, reserved `stream`/`repl`, and the ephemeral gate.
    #[test]
    fn agent_honest_stubs() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(None));
            session.set_agent_invoker(Box::new(FakeAgentInvoker { reply: "ok".into(), seen }));
            install_shopping_cart(&mut session).await;

            // --revision → honest exit 2 (no SDK slot).
            let rev = session.eval_line("sudo shopping-cart --userid jd --revision 3 add-item -- --sku x").await;
            assert_eq!(rev.exit_code, 2);
            assert!(String::from_utf8(rev.stderr).unwrap().contains("--revision targeting is not supported"));

            // stream/repl → honest (interactive/streaming not on the durable agent).
            let stream = session.eval_line("sudo shopping-cart --userid jd stream").await;
            assert_eq!(stream.exit_code, 2);
            assert!(String::from_utf8(stream.stderr).unwrap().contains("interactive/streaming"));
        });
    }

    /// The `golem` command dispatches through the injected cluster; interrupt/resume are honest-stubbed;
    /// no cluster → the honest no-cluster error.
    #[test]
    fn golem_command_dispatch_and_honest_stubs() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();

            // No cluster injected → honest error (but NOT for interrupt/resume, which are honest anyway).
            let no_cluster = session.eval_line("golem agent list").await;
            assert_eq!(no_cluster.exit_code, 4);
            assert!(String::from_utf8(no_cluster.stderr).unwrap().contains("requires a configured Golem cluster"));

            // interrupt/resume are honest-stubbed regardless of cluster.
            let interrupt = session.eval_line("golem agent interrupt 42").await;
            assert_eq!(interrupt.exit_code, 2);
            assert!(String::from_utf8(interrupt.stderr).unwrap().contains("no guest host binding"));

            // With a cluster: list/status/fork/oplog dispatch.
            session.set_golem_cluster(Box::new(FakeGolemCluster));
            // list/oplog/status are Allow (read-only); fork/rollback are Confirm → sudo pre-authorizes.
            assert!(String::from_utf8(session.eval_line("golem agent list").await.stdout).unwrap().contains("agent-1"));
            assert!(String::from_utf8(session.eval_line("sudo golem fork").await.stdout).unwrap().contains("forked"));
            assert!(String::from_utf8(session.eval_line("golem oplog").await.stdout).unwrap().contains("self oplog"));
            let status = session.eval_line("golem agent status --type ShoppingCart --userid jd").await;
            assert!(String::from_utf8(status.stdout).unwrap().contains("status for ShoppingCart"));
            // `type golem` resolves (the new intercepted verb).
            assert!(String::from_utf8(session.eval_line("type golem").await.stdout).unwrap().contains("golem"));
        });
    }

    /// `mcp add` installs a server: config written, tools fetched, `mcp list`/`mcp tools`/`which`
    /// `grease registry add/list/remove` through `eval_line`: the registry list is persisted and
    /// surfaced. `registry` is Allow (local config only — no network, no pause).
    #[test]
    fn grease_registry_add_list_remove() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();

            let list0 = session.eval_line("grease registry list").await;
            assert!(String::from_utf8(list0.stdout).unwrap().contains("no registries configured"));

            let add = session.eval_line("grease registry add https://reg.example").await;
            assert_eq!(add.exit_code, 0);
            assert!(add.pending_prompt.is_none(), "registry add is Allow — no pause");
            assert!(String::from_utf8(add.stdout).unwrap().contains("added registry"));

            let list1 = session.eval_line("grease registry list").await;
            assert!(String::from_utf8(list1.stdout).unwrap().contains("https://reg.example"));

            let rm = session.eval_line("grease registry remove https://reg.example").await;
            assert_eq!(rm.exit_code, 0);
            let list2 = session.eval_line("grease registry list").await;
            assert!(String::from_utf8(list2.stdout).unwrap().contains("no registries configured"));
        });
    }

    /// End-to-end: `grease install` fetches a prompt package, persists it, registers it as a command,
    /// and running the installed prompt name dispatches to the model with the (filled) body. Uses the
    /// scripted fake HTTP transport (reused from MCP — grease shares the `McpHttp` seam).
    #[test]
    fn grease_install_then_run_a_prompt() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply("the summary", seen.clone())));

            // Script the registry: GET /packages/tldr.json → a parameterized prompt package.
            let pkg = serde_json::json!({
                "name": "tldr",
                "description": "summarize a file",
                "arguments": [{"name":"file","required":true}],
                "body": "Summarize the file {{file}} concisely."
            });
            // No index route → the index lookup 404s → record-only install (these tests don't assert
            // on integrity; the verify path has its own dedicated tests).
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
            session.run_line("grease registry add https://reg.example").await;

            // Install (sudo pre-authorizes the Confirm).
            let inst = session.eval_line("sudo grease install tldr").await;
            assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
            assert!(String::from_utf8(inst.stdout).unwrap().contains("installed tldr"));

            // It's now an installed prompt: `grease list` shows it, `type tldr` sees it.
            let list = session.eval_line("grease list").await;
            assert!(String::from_utf8(list.stdout).unwrap().contains("tldr"));

            // `tldr --help` shows its generated help (with the arg), no confirmation.
            let help = session.eval_line("tldr --help").await;
            assert_eq!(help.exit_code, 0);
            assert!(String::from_utf8(help.stdout).unwrap().contains("--file"));

            // Missing required arg → exit 2 (no model call).
            let miss = session.eval_line("sudo tldr").await;
            assert_eq!(miss.exit_code, 2);
            assert!(String::from_utf8(miss.stderr).unwrap().contains("missing required argument --file"));

            // Run it with the arg (sudo pre-authorizes the prompt's Confirm) → the model sees the
            // FILLED body.
            let run = session.eval_line("sudo tldr --file report.md").await;
            assert_eq!(run.exit_code, 0);
            assert_eq!(String::from_utf8(run.stdout).unwrap(), "the summary");
            let content = seen.lock().unwrap()[0].user_content();
            assert!(content.contains("Summarize the file report.md concisely."), "got: {content}");

            // A bare (non-sudo) prompt run confirms (outbound LLM).
            let confirm = session.eval_line("tldr --file x.md").await;
            assert!(confirm.pending_prompt.is_some(), "prompt run should confirm without sudo");
            session.answer_prompt(Some("no".into())).await;

            // Remove deregisters: the name is no longer an installed prompt.
            let rm = session.eval_line("sudo grease remove tldr").await;
            assert_eq!(rm.exit_code, 0);
            assert!(!session.grease.is_prompt("tldr"));
        });
    }

    /// A prompt authored as a `.md` file with YAML frontmatter installs identically to a JSON prompt:
    /// grease fetches `/packages/<name>.md`, converts the frontmatter → the canonical PromptPackage,
    /// and the installed command fills `{{var}}` and dispatches to the model.
    #[test]
    fn grease_install_then_run_a_markdown_prompt() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply("the summary", seen.clone())));

            // The registry serves the prompt as a Markdown file (no `.json`), routed on the `.md` suffix
            // so the `.json` fetch 404s first and the `.md` fetch succeeds.
            let md = "---\n\
                      name: tldr\n\
                      description: summarize a file\n\
                      arguments:\n\
                      \x20 - name: file\n\
                      \x20   required: true\n\
                      ---\n\
                      Summarize the file {{file}} concisely.\n";
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![(
                "/packages/tldr.md",
                grease_text(md),
            )])));
            session.run_line("grease registry add https://reg.example").await;

            let inst = session.eval_line("sudo grease install tldr").await;
            assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
            assert!(String::from_utf8(inst.stdout).unwrap().contains("installed tldr"));

            // Installed as a prompt with the declared arg (from the frontmatter).
            let help = session.eval_line("tldr --help").await;
            assert_eq!(help.exit_code, 0);
            assert!(String::from_utf8(help.stdout).unwrap().contains("--file"));

            // Running fills the body (converted from the `.md` body) and dispatches to the model.
            let run = session.eval_line("sudo tldr --file report.md").await;
            assert_eq!(run.exit_code, 0);
            assert_eq!(String::from_utf8(run.stdout).unwrap(), "the summary");
            let content = seen.lock().unwrap()[0].user_content();
            assert!(content.contains("Summarize the file report.md concisely."), "got: {content}");
        });
    }

    /// `cat /proc/clank/system-prompt` reflects LIVE state: after installing a grease prompt, the proc
    /// file lists it as a `prompt__<name>` tool (the exact prompt the model sees), not just the static
    /// base surface.
    #[test]
    fn proc_system_prompt_reflects_installed_prompts() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();

            // Before install: the proc file has the base surface but NOT our prompt.
            let before = String::from_utf8(
                session.eval_line("cat /proc/clank/system-prompt").await.stdout,
            )
            .unwrap();
            assert!(!before.contains("prompt__hello"), "not installed yet");

            let pkg = serde_json::json!({
                "kind": "prompt", "name": "hello", "description": "say hi", "body": "Say hi."
            });
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
            session.run_line("grease registry add https://reg.example").await;
            session.eval_line("sudo grease install hello").await;

            // After install: the proc file lists the installed prompt tool + the "Installed prompt
            // tools" heading from build_system_prompt_with_capabilities.
            let after = String::from_utf8(
                session.eval_line("cat /proc/clank/system-prompt").await.stdout,
            )
            .unwrap();
            assert!(after.contains("prompt__hello"), "system prompt lists the installed prompt: {after}");
            assert!(after.contains("Installed prompt tools"), "and its heading: {after}");
        });
    }

    /// End-to-end: `grease install` fetches a `kind:script` package, persists it to the store, writes
    /// its bin stub to the SCRIPT bin dir (not the prompt dir), registers it as a Confirm command, and
    /// running the installed name executes the FILLED shell body through Brush (`run_string`) — no LLM.
    #[test]
    fn grease_install_then_run_a_script() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();

            // A parameterized shell-script package.
            let pkg = serde_json::json!({
                "kind": "script",
                "name": "greet",
                "description": "print a greeting",
                "arguments": [{"name":"who","required":true}],
                "body": "echo hello {{who}}"
            });
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
            session.run_line("grease registry add https://reg.example").await;

            let inst = session.eval_line("sudo grease install greet").await;
            assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
            let out = String::from_utf8(inst.stdout).unwrap();
            assert!(out.contains("installed greet"), "install output: {out}");
            assert!(out.contains("[script]"), "install output names the kind: {out}");

            // It's an installed SCRIPT (not a prompt), and its stub is in the script bin dir.
            assert!(session.grease.is_script("greet"));
            assert!(!session.grease.is_prompt("greet"));
            assert!(crate::grease::config::script_bin_dir().join("greet").exists());
            assert!(!crate::grease::config::bin_dir().join("greet").exists());

            // `grease list` shows it tagged as a script.
            let list = String::from_utf8(session.eval_line("grease list").await.stdout).unwrap();
            assert!(list.contains("greet") && list.contains("[script]"), "list: {list}");

            // `greet --help` shows generated help disclosing the local-shell capability, no confirm.
            let help = session.eval_line("greet --help").await;
            assert_eq!(help.exit_code, 0);
            let help_s = String::from_utf8(help.stdout).unwrap();
            assert!(help_s.contains("--who"), "help: {help_s}");
            assert!(help_s.contains("local shell"), "help discloses shell capability: {help_s}");

            // Missing required arg → exit 2, no shell run.
            let miss = session.eval_line("sudo greet").await;
            assert_eq!(miss.exit_code, 2);
            assert!(String::from_utf8(miss.stderr).unwrap().contains("missing required argument --who"));

            // Run it (sudo pre-authorizes the Confirm) → the FILLED shell body runs locally.
            let run = session.eval_line("sudo greet --who world").await;
            assert_eq!(run.exit_code, 0, "run stderr: {}", String::from_utf8_lossy(&run.stderr));
            assert_eq!(String::from_utf8(run.stdout).unwrap().trim_end(), "hello world");

            // A bare (non-sudo) script run confirms (running local shell is a Confirm capability).
            let confirm = session.eval_line("greet --who x").await;
            assert!(confirm.pending_prompt.is_some(), "script run should confirm without sudo");
            session.answer_prompt(Some("no".into())).await;

            // Remove deregisters and deletes the script stub.
            let rm = session.eval_line("sudo grease remove greet").await;
            assert_eq!(rm.exit_code, 0);
            assert!(!session.grease.is_script("greet"));
            assert!(!crate::grease::config::script_bin_dir().join("greet").exists());
        });
    }

    /// End-to-end: `grease install` fetches a `kind:skill` package, materializes its dir tree (docs +
    /// bundled `bin/` scripts), and surfaces it to the model in the system prompt — but a skill is NOT
    /// a command (no manifest, no `ask` tool, no `run_command` arm).
    #[test]
    fn grease_install_a_skill_materializes_and_surfaces_it() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();

            let pkg = serde_json::json!({
                "kind": "skill",
                "name": "code-review",
                "description": "review code carefully",
                "intended-use": "when the user asks for a code review",
                "documents": [{"path": "SKILL.md", "content": "Review for correctness first."}],
                "scripts": [{"name": "lint-all", "body": "echo linting"}]
            });
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
            session.run_line("grease registry add https://reg.example").await;

            let inst = session.eval_line("sudo grease install code-review").await;
            assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
            let out = String::from_utf8(inst.stdout).unwrap();
            assert!(out.contains("installed code-review") && out.contains("[skill]"), "out: {out}");

            // The dir tree is materialized: doc + bundled bin script.
            let skill_root = crate::grease::config::skills_dir().join("code-review");
            assert_eq!(
                std::fs::read_to_string(skill_root.join("SKILL.md")).unwrap(),
                "Review for correctness first."
            );
            assert_eq!(
                std::fs::read_to_string(skill_root.join("bin/lint-all")).unwrap(),
                "echo linting"
            );

            // A skill is NOT a command: not a script/prompt, no manifest, no ask tool.
            assert!(session.grease.is_skill("code-review"));
            assert!(!session.grease.is_script("code-review"));
            assert!(!session.grease.is_prompt("code-review"));
            assert!(session.grease.manifest_for("code-review").is_none());
            assert!(session.grease.ask_tool_definitions().is_empty());

            // `grease info` describes the envelope + bundles.
            let info = String::from_utf8(session.eval_line("grease info code-review").await.stdout).unwrap();
            assert!(info.contains("[skill]") && info.contains("SKILL.md") && info.contains("lint-all"), "info: {info}");

            // The skill is surfaced in the agentic system prompt (context, not a callable tool).
            let sys = crate::ai::ask::build_system_prompt_with_capabilities(
                &session.registry,
                &session.mcp,
                &session.grease,
            );
            assert!(sys.contains("Installed skills"), "system prompt lists skills: …");
            assert!(sys.contains("code-review") && sys.contains("when the user asks for a code review"));

            // Remove deletes the dir tree and deregisters.
            let rm = session.eval_line("sudo grease remove code-review").await;
            assert_eq!(rm.exit_code, 0);
            assert!(!session.grease.is_skill("code-review"));
            assert!(!skill_root.exists());
        });
    }

    /// A deterministic ed25519 keypair (from a fixed 32-byte seed) + a signer over `body`. Returns
    /// `(pubkey_b64, sig_b64)`. Dev-only (native `ed25519-dalek` signing side), no RNG.
    fn sign_payload(body: &[u8]) -> (String, String) {
        use base64::Engine;
        use ed25519_dalek::{Signer, SigningKey};
        let seed = [7u8; 32]; // fixed seed → deterministic key across runs
        let sk = SigningKey::from_bytes(&seed);
        let pk_b64 = base64::engine::general_purpose::STANDARD.encode(sk.verifying_key().to_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sk.sign(body).to_bytes());
        (pk_b64, sig_b64)
    }

    /// A signed registry (configured with `--key`) installs a package whose ed25519 signature verifies,
    /// records the signer, and surfaces "signed" in the output. The signature is over the EXACT bytes
    /// the fake registry serves.
    #[test]
    fn grease_install_verifies_a_valid_signature() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();

            let pkg = serde_json::json!({
                "kind": "prompt", "name": "signed-pkg", "description": "d", "body": "hi"
            });
            let body = grease_json(pkg.clone()).body; // the exact bytes the registry serves
            let (pubkey, sig) = sign_payload(&body);
            let index = serde_json::json!({
                "packages": [{
                    "name": "signed-pkg", "description": "d",
                    "sha256": crate::grease::pkg::sha256_hex(&body),
                    "sig": sig, "signer": "alice"
                }]
            });
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
                ("/index.json", grease_json(index)),
                ("/packages/", crate::mcp::client::HttpResponse {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body,
                }),
            ])));
            session.run_line(&format!("grease registry add https://reg.example --key {pubkey}")).await;

            let inst = session.eval_line("sudo grease install signed-pkg").await;
            assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
            let out = String::from_utf8(inst.stdout).unwrap();
            assert!(out.contains("signed"), "install reports signed: {out}");
            assert!(session.grease.is_prompt("signed-pkg"));
            // `grease info` shows the signer.
            let info = String::from_utf8(session.eval_line("grease info signed-pkg").await.stdout).unwrap();
            assert!(info.contains("signed by alice"), "info shows signer: {info}");
        });
    }

    /// A signed registry REJECTS a package whose signature does not verify (wrong signature) — hard
    /// exit 4, nothing installed.
    #[test]
    fn grease_install_rejects_a_bad_signature() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();

            let pkg = serde_json::json!({
                "kind": "prompt", "name": "bad-sig", "description": "d", "body": "hi"
            });
            let body = grease_json(pkg.clone()).body;
            let (pubkey, _good_sig) = sign_payload(&body);
            // A signature over DIFFERENT bytes → verify fails against `body`.
            let (_pk2, wrong_sig) = sign_payload(b"some other content");
            let index = serde_json::json!({
                "packages": [{
                    "name": "bad-sig", "description": "d",
                    "sha256": crate::grease::pkg::sha256_hex(&body),
                    "sig": wrong_sig, "signer": "mallory"
                }]
            });
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
                ("/index.json", grease_json(index)),
                ("/packages/", crate::mcp::client::HttpResponse {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body,
                }),
            ])));
            session.run_line(&format!("grease registry add https://reg.example --key {pubkey}")).await;

            let inst = session.eval_line("sudo grease install bad-sig").await;
            assert_eq!(inst.exit_code, 4, "a bad signature must reject");
            assert!(String::from_utf8(inst.stderr).unwrap().contains("signature verification failed"));
            assert!(!session.grease.is_prompt("bad-sig"), "nothing installed on sig failure");
        });
    }

    /// A signed registry REJECTS a package that carries NO signature (a signed registry must sign its
    /// packages) — hard exit 4.
    #[test]
    fn grease_install_rejects_unsigned_package_from_signed_registry() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();
            let pkg = serde_json::json!({
                "kind": "prompt", "name": "nosig", "description": "d", "body": "hi"
            });
            let body = grease_json(pkg.clone()).body;
            let (pubkey, _sig) = sign_payload(&body);
            let index = serde_json::json!({
                "packages": [{ "name": "nosig", "description": "d",
                    "sha256": crate::grease::pkg::sha256_hex(&body) }] // NO sig field
            });
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
                ("/index.json", grease_json(index)),
                ("/packages/", crate::mcp::client::HttpResponse {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body,
                }),
            ])));
            session.run_line(&format!("grease registry add https://reg.example --key {pubkey}")).await;

            let inst = session.eval_line("sudo grease install nosig").await;
            assert_eq!(inst.exit_code, 4);
            assert!(String::from_utf8(inst.stderr).unwrap().contains("no signature"));
            assert!(!session.grease.is_prompt("nosig"));
        });
    }

    /// An UNsigned registry (no `--key`) still installs, marked unsigned (record-only signing).
    #[test]
    fn grease_install_from_unsigned_registry_is_record_only() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();
            let pkg = serde_json::json!({
                "kind": "prompt", "name": "plain", "description": "d", "body": "hi"
            });
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
            session.run_line("grease registry add https://reg.example").await; // no --key

            let inst = session.eval_line("sudo grease install plain").await;
            assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
            assert!(session.grease.is_prompt("plain"));
            let info = String::from_utf8(session.eval_line("grease info plain").await.stdout).unwrap();
            assert!(info.contains("unsigned"), "info shows unsigned: {info}");
        });
    }

    /// RFC-6962 leaf/node hashers for building a fixture transparency log in tests.
    fn rfc_leaf(data: &[u8]) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update([0x00]);
        h.update(data);
        h.finalize().into()
    }
    fn rfc_node(l: &[u8], r: &[u8]) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update([0x01]);
        h.update(l);
        h.update(r);
        h.finalize().into()
    }

    /// A signed registry whose index carries a valid RFC-6962 inclusion proof installs, records
    /// `log_verified`, and `grease info` shows the transparency-log index. Uses a 2-leaf tree; our
    /// package's content-hash is leaf 0, some other entry is leaf 1.
    #[test]
    fn grease_install_verifies_transparency_log_inclusion() {
        on_rt(async {
            use base64::Engine;
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();
            let pkg = serde_json::json!({
                "kind": "prompt", "name": "logged", "description": "d", "body": "hi"
            });
            let body = grease_json(pkg.clone()).body;
            let (pubkey, sig) = sign_payload(&body);
            // Build a 2-leaf tree: leaf 0 = our package's sha256-hex string (the log leaf), leaf 1 = a
            // sibling. Proof for leaf 0 is [leaf_hash(sibling)]; root = node(leaf0, leaf1).
            let leaf0 = crate::grease::pkg::sha256_hex(&body);
            let sibling = b"another-package-digest".to_vec();
            let h0 = rfc_leaf(leaf0.as_bytes());
            let h1 = rfc_leaf(&sibling);
            let root = rfc_node(&h0, &h1);
            let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
            let index = serde_json::json!({
                "packages": [{
                    "name": "logged", "description": "d",
                    "sha256": leaf0, "sig": sig, "signer": "alice",
                    "log": {
                        "leaf-index": 0, "tree-size": 2,
                        "root": b64(&root), "proof": [b64(&h1)]
                    }
                }]
            });
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
                ("/index.json", grease_json(index)),
                ("/packages/", crate::mcp::client::HttpResponse {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body,
                }),
            ])));
            session.run_line(&format!("grease registry add https://reg.example --key {pubkey}")).await;

            let inst = session.eval_line("sudo grease install logged").await;
            assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
            assert!(String::from_utf8(inst.stdout).unwrap().contains("in log"), "reports in-log");
            let info = String::from_utf8(session.eval_line("grease info logged").await.stdout).unwrap();
            assert!(info.contains("transparency log @0"), "info shows log index: {info}");
        });
    }

    /// A tampered inclusion proof (wrong root) is a HARD reject (exit 4, nothing installed).
    #[test]
    fn grease_install_rejects_bad_transparency_log_proof() {
        on_rt(async {
            use base64::Engine;
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();
            let pkg = serde_json::json!({
                "kind": "prompt", "name": "badlog", "description": "d", "body": "hi"
            });
            let body = grease_json(pkg.clone()).body;
            let (pubkey, sig) = sign_payload(&body);
            let leaf0 = crate::grease::pkg::sha256_hex(&body);
            let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
            // A bogus root that won't match the recomputed one.
            let index = serde_json::json!({
                "packages": [{
                    "name": "badlog", "description": "d",
                    "sha256": leaf0, "sig": sig, "signer": "alice",
                    "log": { "leaf-index": 0, "tree-size": 2,
                        "root": b64(&[0u8; 32]), "proof": [b64(&[1u8; 32])] }
                }]
            });
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
                ("/index.json", grease_json(index)),
                ("/packages/", crate::mcp::client::HttpResponse {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body,
                }),
            ])));
            session.run_line(&format!("grease registry add https://reg.example --key {pubkey}")).await;

            let inst = session.eval_line("sudo grease install badlog").await;
            assert_eq!(inst.exit_code, 4, "a bad log proof must reject");
            assert!(String::from_utf8(inst.stderr).unwrap().contains("transparency-log check failed"));
            assert!(!session.grease.is_prompt("badlog"), "nothing installed on log failure");
        });
    }

    /// A bare `grease install` surfaces a capability-disclosure confirmation naming the package, its
    /// source registries, and the ask capability (README "discloses capability requests"). `sudo`
    /// pre-authorizes (no pause).
    #[test]
    fn grease_install_discloses_capabilities() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();
            session.run_line("grease registry add https://reg.example/pkgs").await;

            let surface = session.eval_line("grease install tldr").await;
            let q = surface.pending_prompt.expect("install should confirm").question;
            assert!(q.contains("\"tldr\""), "discloses the package name: {q}");
            assert!(q.contains("https://reg.example/pkgs"), "discloses the source registry: {q}");
            assert!(q.contains("run via ask"), "discloses the ask capability: {q}");
            assert!(q.contains("local shell"), "discloses the local-shell capability: {q}");
            // Deny to leave state clean.
            session.answer_prompt(Some("no".into())).await;

            // `sudo grease install` pre-authorizes — no pause (it then errors on the fetch, which is
            // fine; we're only asserting the no-pause behavior here).
            let sudo = session.eval_line("sudo grease install tldr").await;
            assert!(sudo.pending_prompt.is_none(), "sudo should not pause");
        });
    }

    /// A matching index sha256 → verified install.
    #[test]
    fn grease_install_verifies_matching_sha256() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let pkg = serde_json::json!({
                "name": "vpkg", "description": "verified package", "body": "hello."
            });
            let good = crate::grease::pkg::sha256_hex(pkg.to_string().as_bytes());
            let mut session = Session::new().await.unwrap();
            let index = serde_json::json!({
                "packages": [{"name":"vpkg","description":"verified package","sha256": good}]
            });
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
                ("/index.json", grease_json(index)),
                ("/packages/", grease_json(pkg)),
            ])));
            session.run_line("grease registry add https://reg.example").await;
            let inst = session.eval_line("sudo grease install vpkg").await;
            assert_eq!(inst.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&inst.stderr));
            assert!(String::from_utf8(inst.stdout).unwrap().contains("verified"));
            assert!(session.grease.is_prompt("vpkg"));
        });
    }

    /// A mismatched index sha256 → reject (exit 4), nothing persisted.
    #[test]
    fn grease_install_rejects_sha256_mismatch() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let pkg = serde_json::json!({"name":"vpkg","description":"d","body":"hello."});
            let mut session = Session::new().await.unwrap();
            let index = serde_json::json!({
                "packages": [{"name":"vpkg","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]
            });
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
                ("/index.json", grease_json(index)),
                ("/packages/", grease_json(pkg)),
            ])));
            session.run_line("grease registry add https://reg.example").await;
            let inst = session.eval_line("sudo grease install vpkg").await;
            assert_eq!(inst.exit_code, 4);
            assert!(String::from_utf8(inst.stderr).unwrap().contains("integrity check failed"));
            assert!(!session.grease.is_prompt("vpkg"), "a mismatched package must not install");
            assert!(!crate::grease::config::store_dir().join("vpkg").exists());
        });
    }

    /// A registry index with no sha256 for the package → record-only install, with a stderr note.
    #[test]
    fn grease_install_record_only_without_index_hash() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();
            let pkg = serde_json::json!({"name":"loose","description":"d","body":"hi."});
            // Index present but with no sha256 field for the package.
            let index = serde_json::json!({"packages":[{"name":"loose","description":"d"}]});
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
                ("/index.json", grease_json(index)),
                ("/packages/", grease_json(pkg)),
            ])));
            session.run_line("grease registry add https://reg.example").await;
            let inst = session.eval_line("sudo grease install loose").await;
            assert_eq!(inst.exit_code, 0);
            let out = String::from_utf8(inst.stdout).unwrap();
            assert!(out.contains("no integrity hash"), "expected record-only note, got: {out}");
            assert!(out.contains("unverified"));
            assert!(session.grease.is_prompt("loose"));
        });
    }

    /// An installed prompt is exposed to the model as a `prompt__<name>` tool: it appears in the tool
    /// surface + the system prompt, and a scripted tool call runs the prompt (the model sees the FILLED
    /// body). Confirms under a plain ask; runs under `sudo ask`.
    #[test]
    fn ask_can_call_an_installed_prompt() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();

            // Install a parameterized prompt (reuse the fetch flow).
            let pkg = serde_json::json!({
                "name": "tldr",
                "description": "one-line summary",
                "arguments": [{"name":"file","required":true}],
                "body": "TL;DR of {{file}} please."
            });
            // No index route → the index lookup 404s → record-only install (these tests don't assert
            // on integrity; the verify path has its own dedicated tests).
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
            session.run_line("grease registry add https://reg.example").await;
            let inst = session.eval_line("sudo grease install tldr").await;
            assert_eq!(inst.exit_code, 0);

            // The model calls the prompt tool by its namespaced name with the required arg. The shared
            // FakeProvider serves three turns in order: (1) the outer ask's tool call, (2) the NESTED
            // prompt run's reply (the prompt tool re-enters the model), (3) the outer ask's final text.
            let ask_seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![
                    crate::ai::ask::AskResponse {
                        text: String::new(),
                        tool_calls: vec![crate::ai::ask::AskToolCall {
                            id: "p1".into(),
                            name: "prompt__tldr".into(),
                            arguments_json: serde_json::json!({ "file": "report.md" }).to_string(),
                        }],
                        finished_for_tools: true,
                        error: None,
                    },
                    crate::ai::ask::AskResponse::text("the one-line summary"), // nested prompt reply
                    crate::ai::ask::AskResponse::text("summarized it"),        // outer ask final text
                ],
                ask_seen.clone(),
            )));

            let result = session.eval_line(r#"sudo ask "summarize report.md with the tldr prompt""#).await;
            assert_eq!(result.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&result.stderr));
            assert_eq!(String::from_utf8(result.stdout).unwrap(), "summarized it");

            let turns = ask_seen.lock().unwrap();
            // The prompt tool was in the outer ask's tool surface AND listed in its system prompt.
            assert!(
                turns[0].tools.iter().any(|t| t.name == "prompt__tldr"),
                "the prompt should be an ask tool"
            );
            let system = turns[0].system.clone().unwrap_or_default();
            assert!(system.contains("prompt__tldr"), "system prompt should list the prompt tool");
            // The nested prompt run saw the FILLED body (turn 2 — the {{file}} was substituted).
            let saw_filled = turns
                .iter()
                .any(|t| t.user_content().contains("TL;DR of report.md please."));
            assert!(saw_filled, "the model should have seen the filled prompt body");
        });
    }

    /// Under a plain (non-sudo) ask, an installed-prompt tool call pauses for authorization (running a
    /// prompt is an outbound LLM call → Confirm).
    #[test]
    fn ask_prompt_tool_pauses_without_sudo() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();
            let pkg = serde_json::json!({
                "name": "greet", "description": "greet", "body": "Say hello."
            });
            // No index route → the index lookup 404s → record-only install (these tests don't assert
            // on integrity; the verify path has its own dedicated tests).
            session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
            session.run_line("grease registry add https://reg.example").await;
            session.eval_line("sudo grease install greet").await;

            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![crate::ai::ask::AskResponse {
                    text: String::new(),
                    tool_calls: vec![crate::ai::ask::AskToolCall {
                        id: "p1".into(),
                        name: "prompt__greet".into(),
                        arguments_json: "{}".into(),
                    }],
                    finished_for_tools: true,
                    error: None,
                }],
                std::sync::Arc::new(Mutex::new(Vec::new())),
            )));
            // Plain (non-sudo) ask: the prompt tool call pauses for authorization.
            let r = session.eval_line(r#"ask "greet the user""#).await;
            // The ask itself first confirms (outbound HTTP), then the tool call confirms — either way a
            // pause is surfaced.
            assert!(r.pending_prompt.is_some(), "a plain ask + prompt tool call should pause");
            session.answer_prompt(Some("no".into())).await; // drain
        });
    }

    /// A registry-name collision with a builtin is rejected at install.
    #[test]
    fn grease_install_rejects_builtin_collision() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();
            session.set_mcp_http(Box::new(FakeMcpHttp::new(vec![])));
            session.run_line("grease registry add https://reg.example").await;
            // `ask` is a builtin — installing a package named `ask` must fail before any fetch.
            let r = session.eval_line("sudo grease install ask").await;
            assert_eq!(r.exit_code, 2);
            assert!(String::from_utf8(r.stderr).unwrap().contains("collides with a built-in"));
        });
    }

    /// `grease install` (a Confirm subcommand) surfaces a confirmation without sudo; a non-http
    /// registry URL is rejected; install with no registry gives an honest error.
    #[test]
    fn grease_install_confirms_and_errors_without_registry() {
        on_rt(async {
            let _dirs = set_grease_dirs();
            let mut session = Session::new().await.unwrap();

            let bad = session.eval_line("grease registry add not-a-url").await;
            assert_eq!(bad.exit_code, 2);
            assert!(String::from_utf8(bad.stderr).unwrap().contains("not an http"));

            // `install` is Confirm — a bare invocation pauses.
            let confirm = session.eval_line("grease install summarize").await;
            assert!(confirm.pending_prompt.is_some(), "install should confirm without sudo");
            session.answer_prompt(Some("no".into())).await;

            // Under sudo it runs; with no registry configured it errors honestly (no panic).
            let inst = session.eval_line("sudo grease install summarize").await;
            assert_eq!(inst.exit_code, 1);
            assert!(String::from_utf8(inst.stderr).unwrap().contains("no registries configured"));
        });
    }

    /// reflect it. Uses a scripted fake transport.
    #[test]
    fn mcp_add_installs_and_surfaces_the_server() {
        on_rt(async {
            let dirs = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();
            // Put the mcp bin dir on PATH so `which` finds the stub.
            session.run_line(&format!("export PATH={}:$PATH", dirs.bin)).await;
            session.set_mcp_http(Box::new(FakeMcpHttp::new(mcp_install_script())));

            let add = session.eval_line("sudo mcp add demo https://x.example/mcp").await;
            assert_eq!(add.exit_code, 0, "add failed: {}", String::from_utf8_lossy(&add.stderr));
            assert!(String::from_utf8(add.stdout).unwrap().contains("1 tools"));

            let (list, _) = session.run_line("mcp list").await;
            let list = String::from_utf8(list).unwrap();
            assert!(list.contains("demo") && list.contains("1 tools"), "got: {list}");

            let (tools, _) = session.run_line("mcp tools demo").await;
            assert!(String::from_utf8(tools).unwrap().contains("echo"));

            // The /usr/lib/mcp/bin stub is a real file, so `which` finds it.
            let (which, _) = session.run_line("which demo").await;
            assert!(String::from_utf8(which).unwrap().contains("demo"), "which should find the stub");
        });
    }

    /// `mcp add` against an erroring transport keeps the config as "not installed" and exits 4.
    #[test]
    fn mcp_add_transport_failure_is_configured_not_installed() {
        on_rt(async {
            let _dirs = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();
            // initialize returns a 500.
            let bad = crate::mcp::client::HttpResponse { status: 500, headers: vec![], body: vec![] };
            session.set_mcp_http(Box::new(FakeMcpHttp::new(vec![bad])));

            let add = session.eval_line("sudo mcp add demo https://x.example/mcp").await;
            assert_eq!(add.exit_code, 4);
            let (list, _) = session.run_line("mcp list").await;
            assert!(String::from_utf8(list).unwrap().contains("not installed"));
        });
    }

    /// A server name colliding with a built-in command is rejected.
    #[test]
    fn mcp_add_rejects_a_builtin_name_collision() {
        on_rt(async {
            let _dirs = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();
            session.set_mcp_http(Box::new(FakeMcpHttp::new(vec![])));
            let add = session.eval_line("sudo mcp add grep https://x/mcp").await;
            assert_eq!(add.exit_code, 2);
            assert!(String::from_utf8(add.stderr).unwrap().contains("collides"));
        });
    }

    /// `mcp add` is `Confirm`-policy (outbound HTTP): a bare `mcp add` surfaces a confirmation, while
    /// `mcp list` (Allow subcommand) does not.
    #[test]
    fn mcp_add_confirms_but_list_does_not() {
        on_rt(async {
            let _dirs = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();
            session.set_mcp_http(Box::new(FakeMcpHttp::new(mcp_install_script())));

            // `mcp list` (subcommand Allow) runs without a confirm.
            let list = session.eval_line("mcp list").await;
            assert!(list.pending_prompt.is_none(), "mcp list should not confirm");

            // Bare `mcp add` (subcommand Confirm) surfaces a confirmation.
            let add = session.eval_line("mcp add demo https://x/mcp").await;
            assert!(add.pending_prompt.is_some(), "mcp add should confirm");
            // Approve → the install runs.
            let done = session.answer_prompt(Some("yes".to_string())).await;
            assert_eq!(done.exit_code, 0);
        });
    }

    /// A `tools/call` response echoing text content.
    fn mcp_call_response(text: &str) -> crate::mcp::client::HttpResponse {
        mcp_json(serde_json::json!({"jsonrpc":"2.0","id":9,"result":{
            "content":[{"type":"text","text":text}], "isError":false}}))
    }

    /// `<server> <tool> --param v` runs a tool call: args mapped from the schema; result text returned.
    #[test]
    fn mcp_tool_dispatch_maps_args_and_returns_text() {
        on_rt(async {
            let dirs = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();
            let mut script = mcp_install_script();
            script.push(mcp_call_response("echoed: hello"));
            session.set_mcp_http(Box::new(FakeMcpHttp::new(script)));
            session.eval_line("sudo mcp add demo https://x/mcp").await;

            // `sudo demo echo --text hello` runs the tool (sudo pre-authorizes the Confirm).
            let out = session.eval_line("sudo demo echo --text hello").await;
            assert_eq!(out.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&out.stderr));
            assert!(String::from_utf8(out.stdout).unwrap().contains("echoed: hello"));
            let _ = dirs;
        });
    }

    /// A missing required argument is a usage error (exit 2), no HTTP call.
    #[test]
    fn mcp_tool_missing_required_arg_errors() {
        on_rt(async {
            let _dirs = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();
            session.set_mcp_http(Box::new(FakeMcpHttp::new(mcp_install_script())));
            session.eval_line("sudo mcp add demo https://x/mcp").await;
            // `echo` requires `text`; omit it.
            let out = session.eval_line("sudo demo echo").await;
            assert_eq!(out.exit_code, 2);
            assert!(String::from_utf8(out.stderr).unwrap().contains("required"));
        });
    }

    /// A bare `<server> <tool>` (no sudo) surfaces a confirmation; approving runs it.
    #[test]
    fn mcp_tool_confirms_then_runs() {
        on_rt(async {
            let _dirs = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();
            let mut script = mcp_install_script();
            script.push(mcp_call_response("ran"));
            session.set_mcp_http(Box::new(FakeMcpHttp::new(script)));
            session.eval_line("sudo mcp add demo https://x/mcp").await;

            let first = session.eval_line("demo echo --text hi").await;
            assert!(first.pending_prompt.is_some(), "MCP tool call should confirm");
            let done = session.answer_prompt(Some("yes".to_string())).await;
            assert_eq!(done.exit_code, 0);
            assert!(String::from_utf8(done.stdout).unwrap().contains("ran"));
        });
    }

    /// `<server> --help` prints the server's tool list without confirming; `man <server>` too.
    #[test]
    fn mcp_server_help_and_man_surfaces() {
        on_rt(async {
            let _dirs = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();
            session.set_mcp_http(Box::new(FakeMcpHttp::new(mcp_install_script())));
            session.eval_line("sudo mcp add demo https://x/mcp").await;

            let help = session.eval_line("demo --help").await;
            assert!(help.pending_prompt.is_none(), "help must not confirm");
            assert!(String::from_utf8(help.stdout).unwrap().contains("echo"));

            let (man, _) = session.run_line("man demo").await;
            assert!(String::from_utf8(man).unwrap().contains("demo"), "man should resolve the server");
        });
    }

    /// The `--args '<json>'` escape hatch bypasses schema mapping.
    #[test]
    fn mcp_tool_raw_args_escape_hatch() {
        on_rt(async {
            let _dirs = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();
            let mut script = mcp_install_script();
            script.push(mcp_call_response("raw ok"));
            let http = FakeMcpHttp::new(script);
            let seen = http.seen.clone();
            session.set_mcp_http(Box::new(http));
            session.eval_line("sudo mcp add demo https://x/mcp").await;

            let out = session.eval_line(r#"sudo demo echo --args '{"text":"direct"}'"#).await;
            assert_eq!(out.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&out.stderr));
            // The tools/call body carried the raw args verbatim.
            let calls = seen.lock().unwrap();
            let last = calls.last().unwrap();
            assert!(last.1.contains("\"text\":\"direct\""), "tools/call body: {}", last.1);
        });
    }

    /// `mcp session open/list/info/close` lifecycle over a fake transport.
    #[test]
    fn mcp_session_lifecycle() {
        on_rt(async {
            let _dirs = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();
            // install script (init+initialized+tools/list) then a SECOND init for `session open`.
            let mut script = mcp_install_script();
            let mut open_init = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":3,"result":{
                "protocolVersion":"2025-03-26",
                "serverInfo":{"name":"demo","version":"1.0"},"capabilities":{"tools":{}}}}));
            open_init.headers.push(("mcp-session-id".into(), "srv-open".into()));
            script.push(open_init);
            script.push(mcp_json(serde_json::json!({}))); // initialized
            script.push(mcp_json(serde_json::json!({}))); // DELETE close (200)
            session.set_mcp_http(Box::new(FakeMcpHttp::new(script)));
            session.eval_line("sudo mcp add demo https://x/mcp").await;

            // No sessions yet.
            let (list0, _) = session.run_line("mcp session list").await;
            assert!(String::from_utf8(list0).unwrap().contains("no open MCP sessions"));

            // Open one.
            let open = session.eval_line("sudo mcp session open demo").await;
            assert_eq!(open.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&open.stderr));
            assert!(String::from_utf8(open.stdout).unwrap().contains("s1"));

            let (list1, _) = session.run_line("mcp session list").await;
            let list1 = String::from_utf8(list1).unwrap();
            assert!(list1.contains("s1") && list1.contains("srv-open"), "got: {list1}");

            let (info, _) = session.run_line("mcp session info s1").await;
            assert!(String::from_utf8(info).unwrap().contains("demo"));

            // Close it.
            let close = session.eval_line("sudo mcp session close s1").await;
            assert_eq!(close.exit_code, 0);
            let (list2, _) = session.run_line("mcp session list").await;
            assert!(String::from_utf8(list2).unwrap().contains("no open MCP sessions"));
        });
    }

    /// Closing a session the server refuses (HTTP 405) still removes it locally, with a clear message.
    #[test]
    fn mcp_session_close_405_removes_locally() {
        on_rt(async {
            let _dirs = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();
            let mut script = mcp_install_script();
            let mut open_init = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":3,"result":{
                "serverInfo":{"name":"demo","version":"1.0"},"capabilities":{}}}));
            open_init.headers.push(("mcp-session-id".into(), "srv-open".into()));
            script.push(open_init);
            script.push(mcp_json(serde_json::json!({}))); // initialized
            script.push(crate::mcp::client::HttpResponse { status: 405, headers: vec![], body: vec![] });
            session.set_mcp_http(Box::new(FakeMcpHttp::new(script)));
            session.eval_line("sudo mcp add demo https://x/mcp").await;
            session.eval_line("sudo mcp session open demo").await;

            let close = session.eval_line("sudo mcp session close s1").await;
            // Message names the 405 refusal; the local session is gone regardless.
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&close.stdout),
                String::from_utf8_lossy(&close.stderr)
            );
            assert!(combined.contains("405") || combined.contains("locally"), "got: {combined}");
            let (list, _) = session.run_line("mcp session list").await;
            assert!(String::from_utf8(list).unwrap().contains("no open MCP sessions"));
        });
    }

    /// C4: an installed MCP tool becomes an ask ToolDefinition. Under `sudo ask`, the model calling
    /// `mcp__demo__echo` runs the tool (blanket confirm-tier) and the FakeMcpHttp sees the tools/call.
    #[test]
    fn ask_can_call_an_mcp_tool() {
        on_rt(async {
            let _dirs = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();
            let mut script = mcp_install_script();
            script.push(mcp_call_response("echoed by mcp"));
            let http = FakeMcpHttp::new(script);
            let seen = http.seen.clone();
            session.set_mcp_http(Box::new(http));
            session.eval_line("sudo mcp add demo https://x/mcp").await;

            // The model calls the MCP tool by its namespaced name.
            let ask_seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![
                    crate::ai::ask::AskResponse {
                        text: String::new(),
                        tool_calls: vec![crate::ai::ask::AskToolCall {
                            id: "m1".into(),
                            name: "mcp__demo__echo".into(),
                            arguments_json: serde_json::json!({ "text": "hi mcp" }).to_string(),
                        }],
                        finished_for_tools: true,
                        error: None,
                    },
                    crate::ai::ask::AskResponse::text("done, the tool ran"),
                ],
                ask_seen.clone(),
            )));

            let result = session.eval_line(r#"sudo ask "use the demo echo tool""#).await;
            assert_eq!(result.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&result.stderr));
            assert_eq!(String::from_utf8(result.stdout).unwrap(), "done, the tool ran");
            // The MCP server saw a tools/call carrying the tool's arguments.
            let calls = seen.lock().unwrap();
            let tool_call = calls.iter().find(|(_url, body)| body.contains("tools/call"));
            assert!(tool_call.is_some(), "expected a tools/call, saw: {calls:?}");
            assert!(tool_call.unwrap().1.contains("hi mcp"), "args should reach the server");
            // The tool surface the model saw included the MCP tool definition.
            let ask_turns = ask_seen.lock().unwrap();
            assert!(
                ask_turns[0].tools.iter().any(|t| t.name == "mcp__demo__echo"),
                "the MCP tool should be in the ask tool surface"
            );
        });
    }

    /// Under a plain (non-sudo) ask, an MCP tool call pauses for authorization (MCP calls are Confirm).
    #[test]
    fn ask_mcp_tool_pauses_without_sudo() {
        on_rt(async {
            let _dirs = set_mcp_dirs();
            let mut session = Session::new().await.unwrap();
            let mut script = mcp_install_script();
            script.push(mcp_call_response("ran after approval"));
            session.set_mcp_http(Box::new(FakeMcpHttp::new(script)));
            session.eval_line("sudo mcp add demo https://x/mcp").await;

            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![
                    crate::ai::ask::AskResponse {
                        text: String::new(),
                        tool_calls: vec![crate::ai::ask::AskToolCall {
                            id: "m1".into(),
                            name: "mcp__demo__echo".into(),
                            arguments_json: serde_json::json!({ "text": "x" }).to_string(),
                        }],
                        finished_for_tools: true,
                        error: None,
                    },
                    crate::ai::ask::AskResponse::text("finished"),
                ],
                std::sync::Arc::new(Mutex::new(Vec::new())),
            )));

            // Plain ask: approve the ask, then the MCP tool call pauses for its own authz.
            session.eval_line(r#"ask "use the tool""#).await;
            let after_ask = session.answer_prompt(Some("yes".to_string())).await;
            assert!(after_ask.pending_prompt.is_some(), "MCP tool call should pause under plain ask");
            let done = session.answer_prompt(Some("yes".to_string())).await;
            assert_eq!(done.exit_code, 0);
            assert_eq!(String::from_utf8(done.stdout).unwrap(), "finished");
        });
    }

    /// `mcp watch` on a URI no installed server owns is an honest error (the bounded-poll happy path is
    /// covered by `mcp_watch_is_a_bounded_poll`).
    #[test]
    fn mcp_watch_unknown_uri_errors() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let result = session.eval_line("mcp watch some://uri").await;
            assert_eq!(result.exit_code, 1);
            assert!(String::from_utf8(result.stderr).unwrap().contains("no installed server owns"));
        });
    }

    /// With a provider installed, `ask` returns the model's reply on stdout (exit 0), and the request
    /// it assembled carries the current transcript as context (the README "transcript is the context").
    /// `ask` is `Confirm`-gated, so `sudo ask` is used here to skip the confirmation pause.
    #[test]
    fn ask_returns_reply_and_feeds_transcript_as_context() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply(
                "the answer is 42",
                seen.clone(),
            )));

            // Run a command first so there's transcript history to feed as context.
            session.run_line("echo marker_abc").await;

            let result = session.eval_line(r#"sudo ask "what did I just echo?""#).await;
            assert_eq!(result.exit_code, 0);
            assert!(result.pending_prompt.is_none(), "sudo ask must not confirm");
            assert_eq!(
                String::from_utf8(result.stdout).unwrap(),
                "the answer is 42"
            );

            // The provider saw one turn: the first user message carries the prompt and the transcript
            // (including the prior echo), with the default model.
            let turns = seen.lock().unwrap().clone();
            assert_eq!(turns.len(), 1, "one turn expected, got: {}", turns.len());
            let content = turns[0].user_content();
            assert!(
                content.contains("what did I just echo?"),
                "user content should carry the prompt, got: {content}"
            );
            assert_eq!(turns[0].model, crate::ai::ask::DEFAULT_MODEL);
            assert!(
                content.contains("marker_abc"),
                "transcript context should include the prior echo, got: {content}"
            );
        });
    }

    /// When recording a command evicts old entries to stay under budget, the leading count marker is
    /// upgraded into a model-generated summary block (the README's summarize-at-leading-edge compaction).
    #[test]
    fn auto_compaction_summarizes_the_dropped_span() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            // Several identical summaries: each eviction re-opens the pending span and re-summarizes,
            // so more than one summarize turn can fire across the run (the last one wins the marker).
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![crate::ai::ask::AskResponse::text("SUMMARY: earlier work happened"); 8],
                seen.clone(),
            )));

            // Shrink the window so the next few commands force an eviction.
            session.eval_line("context budget 4").await;
            session.run_line("echo marker_one").await;
            session.run_line("echo marker_two").await;
            session.run_line("echo marker_three").await;

            let shown = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
            // The leading marker is a summary block carrying the model's text, not a bare count.
            assert!(
                shown.contains("[summary of") && shown.contains("SUMMARY: earlier work happened"),
                "expected a summary block at the leading edge, got:\n{shown}"
            );
            assert!(!shown.contains("earlier entries dropped"), "count marker should be upgraded");

            // The provider was asked to summarize the DROPPED span (system = SUMMARIZE_SYSTEM_PROMPT),
            // not the whole transcript.
            let turns = seen.lock().unwrap().clone();
            assert!(
                turns.iter().any(|t| t.system.as_deref() == Some(crate::ai::ask::SUMMARIZE_SYSTEM_PROMPT)),
                "a summarize turn should have fired"
            );
        });
    }

    /// With no provider (native), auto-compaction leaves the bare `[N earlier entries dropped]` count
    /// marker — the decided fallback: eviction never blocks or fails on the summary being unavailable.
    #[test]
    fn auto_compaction_falls_back_to_count_marker_without_a_provider() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            // No ask_provider injected.
            session.eval_line("context budget 4").await;
            session.run_line("echo marker_one").await;
            session.run_line("echo marker_two").await;
            session.run_line("echo marker_three").await;

            let shown = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
            assert!(
                shown.contains("earlier entries dropped"),
                "without a provider the count marker stays, got:\n{shown}"
            );
            assert!(!shown.contains("[summary of"), "no summary block without a provider");
        });
    }

    /// Points `CLANK_LOG_DIR` at a fresh temp dir for a Session logging test, restoring the env on drop.
    /// Serializes via a process-wide lock (env is global). The default `DefaultLogSink` (installed by
    /// `eval_line`) then writes real files under this dir.
    struct LogCapture {
        _lock: std::sync::MutexGuard<'static, ()>,
        dir: std::path::PathBuf,
    }
    impl LogCapture {
        fn new(tag: &str) -> Self {
            let lock = crate::logging::test_env_lock();
            let dir = std::env::temp_dir().join(format!("clank-sesslog-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::env::set_var(crate::logging::LOG_DIR_ENV, &dir);
            Self { _lock: lock, dir }
        }
        fn read(&self, file: crate::logging::LogFile) -> String {
            std::fs::read_to_string(self.dir.join(file.filename())).unwrap_or_default()
        }
    }
    impl Drop for LogCapture {
        fn drop(&mut self) {
            std::env::remove_var(crate::logging::LOG_DIR_ENV);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A normal command writes start + end (with exit code) events to shell.log.
    #[test]
    fn shell_log_records_start_and_end() {
        on_rt(async {
            let cap = LogCapture::new("shell");
            let mut session = Session::new().await.unwrap();
            session.eval_line("echo hi").await;
            session.eval_line("false").await;
            let log = cap.read(crate::logging::LogFile::Shell);
            assert!(log.contains(r#"start line="echo hi""#), "got:\n{log}");
            assert!(log.contains(r#"end line="echo hi" exit=0"#), "got:\n{log}");
            assert!(log.contains("exit=1"), "the failing command's exit code is logged, got:\n{log}");
        });
    }

    /// A destructive (`sudo-only`) command is recorded in ops.log with its authorization outcome, even
    /// when denied (a bare `rm` is `sudo-only` → confirm-required without sudo).
    #[test]
    fn ops_log_records_destructive_ops() {
        on_rt(async {
            let cap = LogCapture::new("ops");
            let mut session = Session::new().await.unwrap();
            // A bare `rm` is the destructive tier; without sudo it needs confirmation.
            let r = session.eval_line("rm /tmp/whatever").await;
            assert!(r.pending_prompt.is_some(), "rm should confirm");
            session.answer_prompt(Some("no".into())).await;
            let log = cap.read(crate::logging::LogFile::Ops);
            assert!(log.contains("destructive"), "ops.log should record the destructive op, got:\n{log}");
            assert!(log.contains("cmd=rm"), "got:\n{log}");
            assert!(log.contains("confirm-required"), "got:\n{log}");
        });
    }

    /// An `ask` LLM turn is recorded in http.log (via the LoggingAskProvider wrapper).
    #[test]
    fn http_log_records_the_llm_turn() {
        on_rt(async {
            let cap = LogCapture::new("http");
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply("reply", seen)));
            session.eval_line(r#"sudo ask "hello""#).await;
            let log = cap.read(crate::logging::LogFile::Http);
            assert!(log.contains("kind=llm"), "http.log should record the LLM call, got:\n{log}");
            assert!(log.contains("status=ok"), "got:\n{log}");
        });
    }

    /// `--fresh` sends no transcript context; the prompt still reaches the provider.
    #[test]
    fn ask_fresh_sends_empty_transcript() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply("ok", seen.clone())));

            session.run_line("echo should_not_appear").await;
            let result = session.eval_line(r#"sudo ask --fresh "hi""#).await;
            assert_eq!(result.exit_code, 0);

            // --fresh sends no transcript: the user content is just the prompt, no marker.
            let turns = seen.lock().unwrap().clone();
            let content = turns[0].user_content();
            assert!(
                !content.contains("should_not_appear"),
                "fresh should omit the transcript, got: {content}"
            );
            assert_eq!(content, "hi");
        });
    }

    /// `ask --json` with a valid-JSON reply: the JSON is on stdout, exit 0, and the model saw the
    /// JSON-mode directive in its system prompt.
    #[test]
    fn ask_json_valid_reply_exits_zero() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply(
                r#"{"ok":true}"#,
                seen.clone(),
            )));

            let result = session.eval_line(r#"sudo ask --json --fresh "give me json""#).await;
            assert_eq!(result.exit_code, 0);
            assert_eq!(String::from_utf8(result.stdout).unwrap(), r#"{"ok":true}"#);

            // The system prompt carried the JSON-mode directive.
            let turns = seen.lock().unwrap().clone();
            let system = turns[0].system.clone().unwrap_or_default();
            assert!(
                system.contains("single valid JSON value"),
                "json mode should add the directive, got: {system}"
            );
        });
    }

    /// `ask --json` wrapping its JSON in a Markdown code fence still validates (the fence is stripped).
    #[test]
    fn ask_json_strips_code_fence() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.set_ask_provider(Box::new(FakeProvider::reply(
                "```json\n[1,2,3]\n```",
                std::sync::Arc::new(Mutex::new(Vec::new())),
            )));
            let result = session.eval_line(r#"sudo ask --json --fresh "list""#).await;
            assert_eq!(result.exit_code, 0);
            assert_eq!(String::from_utf8(result.stdout).unwrap(), "[1,2,3]");
        });
    }

    /// `ask --json` with a prose (non-JSON) reply exits 6 with the raw text on stderr and empty
    /// stdout — the README `--json` contract.
    #[test]
    fn ask_json_invalid_reply_exits_six() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.set_ask_provider(Box::new(FakeProvider::reply(
                "sorry, I cannot do that",
                std::sync::Arc::new(Mutex::new(Vec::new())),
            )));
            let result = session.eval_line(r#"sudo ask --json --fresh "give me json""#).await;
            assert_eq!(result.exit_code, 6);
            assert!(result.stdout.is_empty(), "no stdout on a --json failure");
            let stderr = String::from_utf8(result.stderr).unwrap();
            assert!(stderr.contains("did not return valid JSON"), "stderr: {stderr}");
            assert!(stderr.contains("sorry, I cannot do that"), "raw text preserved: {stderr}");
        });
    }

    /// `echo hi | sudo ask "q"` (Phase B): the upstream runs, its stdout is captured and fed to the
    /// model as a stdin block. `sudo` on the tail pre-authorizes (no confirmation).
    #[test]
    fn ask_pipe_feeds_upstream_stdout_as_stdin() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply("got it", seen.clone())));

            let result = session
                .eval_line(r#"echo piped_marker_xyz | sudo ask --fresh "what did I pipe?""#)
                .await;
            assert_eq!(result.exit_code, 0);
            assert_eq!(String::from_utf8(result.stdout).unwrap(), "got it");

            let turns = seen.lock().unwrap().clone();
            assert_eq!(turns.len(), 1);
            let content = turns[0].user_content();
            assert!(
                content.contains("# Piped input (stdin)"),
                "stdin block missing, got: {content}"
            );
            assert!(
                content.contains("piped_marker_xyz"),
                "captured upstream stdout should be in the stdin block, got: {content}"
            );
            // --fresh: the prompt is present, but no transcript context header.
            assert!(content.contains("what did I pipe?"));
        });
    }

    /// A bare (non-sudo) ask-tail pipeline surfaces the ask's confirmation, and the captured stdin
    /// survives the pause: after approval, the model sees the piped bytes.
    #[test]
    fn ask_pipe_confirmation_preserves_stdin() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply("answer", seen.clone())));

            // Bare ask ⇒ pauses for confirmation; the upstream was captured for the pause.
            let surface = session
                .eval_line(r#"echo survives_pause_marker | ask --fresh "q""#)
                .await;
            assert!(surface.pending_prompt.is_some(), "bare ask-pipe should confirm");
            assert!(seen.lock().unwrap().is_empty(), "model not called before approval");

            // Approve ⇒ the ask runs with the preserved stdin.
            let done = session.answer_prompt(Some("yes".into())).await;
            assert_eq!(done.exit_code, 0);
            let content = seen.lock().unwrap()[0].user_content();
            assert!(
                content.contains("survives_pause_marker"),
                "stdin should survive the pause, got: {content}"
            );
        });
    }

    /// A denied ask-tail pipeline exits 5 and does NOT leak the captured stdin into a later ask.
    #[test]
    fn ask_pipe_denied_exits_five_and_clears_stdin() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply("later", seen.clone())));

            let surface = session.eval_line(r#"echo leak_marker | ask --fresh "q""#).await;
            assert!(surface.pending_prompt.is_some());
            let denied = session.answer_prompt(Some("no".into())).await;
            assert_eq!(denied.exit_code, 5);
            assert!(seen.lock().unwrap().is_empty(), "denied ask never calls the model");

            // A subsequent unrelated sudo ask must not carry the earlier pipe's stdin.
            session.eval_line(r#"sudo ask --fresh "hello""#).await;
            let content = seen.lock().unwrap()[0].user_content();
            assert!(
                !content.contains("leak_marker"),
                "stale stdin leaked into a later ask, got: {content}"
            );
        });
    }

    /// `sudo context summarize` runs the LLM (no pause), prints the summary, and does NOT mutate or
    /// re-record the transcript (inspection only, like `context show`).
    #[test]
    fn context_summarize_returns_summary_without_mutating() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply(
                "You ran two echo commands.",
                seen.clone(),
            )));
            session.run_line("echo original_marker_one").await;
            session.run_line("echo original_marker_two").await;

            let result = session.eval_line("sudo context summarize").await;
            assert_eq!(result.exit_code, 0);
            assert_eq!(
                String::from_utf8(result.stdout).unwrap(),
                "You ran two echo commands.\n"
            );
            assert!(result.pending_prompt.is_none(), "sudo must not pause");
            // The provider saw the transcript (the two echoes) as its user content.
            let content = seen.lock().unwrap()[0].user_content();
            assert!(content.contains("original_marker_one") && content.contains("original_marker_two"));

            // The transcript is UNCHANGED: both echoes still there, the summary is NOT recorded.
            let shown = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
            assert!(shown.contains("original_marker_one") && shown.contains("original_marker_two"));
            assert!(!shown.contains("You ran two echo commands"), "summary must not be recorded");
        });
    }

    /// A bare (non-sudo) `context summarize` surfaces a Confirm pause (outbound LLM HTTP); deny → exit
    /// 5, approve → runs.
    #[test]
    fn context_summarize_confirms_then_runs() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply("a summary", seen.clone())));
            session.run_line("echo something").await;

            // Bare summarize pauses.
            let surface = session.eval_line("context summarize").await;
            assert!(surface.pending_prompt.is_some(), "bare summarize should confirm");
            assert!(seen.lock().unwrap().is_empty(), "model not called before approval");

            // Approve ⇒ runs.
            let done = session.answer_prompt(Some("yes".into())).await;
            assert_eq!(done.exit_code, 0);
            assert_eq!(String::from_utf8(done.stdout).unwrap(), "a summary\n");
            // The summary is still not recorded after the deferred run.
            let shown = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
            assert!(!shown.contains("a summary"), "deferred summary must not be recorded");
        });
    }

    /// A denied `context summarize` exits 5 and never calls the model.
    #[test]
    fn context_summarize_denied_exits_five() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply("nope", seen.clone())));
            session.run_line("echo x").await;
            let surface = session.eval_line("context summarize").await;
            assert!(surface.pending_prompt.is_some());
            let denied = session.answer_prompt(Some("no".into())).await;
            assert_eq!(denied.exit_code, 5);
            assert!(seen.lock().unwrap().is_empty(), "denied summarize never calls the model");
        });
    }

    /// `context summarize` with no provider (native) degrades to a clean exit-4 error.
    #[test]
    fn context_summarize_without_provider_errors() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.run_line("echo x").await;
            let result = session.eval_line("sudo context summarize").await;
            assert_eq!(result.exit_code, 4);
            assert!(String::from_utf8(result.stderr).unwrap().contains("no model provider"));
        });
    }

    /// `context summarize` inside `$(...)` stays with Brush and hits the honest error (it can't run
    /// the LLM in the nested runtime).
    #[test]
    fn context_summarize_in_substitution_is_honest() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.set_ask_provider(Box::new(FakeProvider::reply(
                "should not run",
                std::sync::Arc::new(Mutex::new(Vec::new())),
            )));
            session.run_line("echo x").await;
            let result = session.eval_line("echo $(context summarize)").await;
            // The nested summarize errors honestly; the outer echo still exits 0 with the error text
            // captured (Brush substitutes the stderr-less builtin output).
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(combined.contains("needs the model"), "combined: {combined}");
        });
    }

    /// `ask repl` on the durable-agent path (via `eval_line`) returns an honest not-here message
    /// (exit 2), never trying to run a blocking interactive loop.
    #[test]
    fn ask_repl_via_eval_line_is_honest_message() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let result = session.eval_line("ask repl").await;
            assert_eq!(result.exit_code, 2);
            let stderr = String::from_utf8(result.stderr).unwrap();
            assert!(stderr.contains("native-terminal feature"), "stderr: {stderr}");
            assert!(result.pending_prompt.is_none());
        });
    }

    /// A native REPL turn runs against the ISOLATED transcript: the model sees the REPL's own history,
    /// the main session transcript is untouched, and the exchange is recorded into the REPL transcript.
    #[test]
    fn repl_turn_uses_isolated_transcript() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![
                    crate::ai::ask::AskResponse::text("first reply"),
                    crate::ai::ask::AskResponse::text("second reply"),
                ],
                seen.clone(),
            )));

            // Put a marker in the MAIN transcript; the REPL (fresh) must NOT see it.
            session.run_line("echo main_transcript_marker").await;

            let args = crate::ai::ask::ReplArgs {
                model: None,
                seed: crate::ai::ask::ReplSeed::Fresh,
            };
            session.repl_start(&args).unwrap();

            let r1 = session.repl_turn("hello there").await;
            assert_eq!(r1, "first reply");
            // The first turn saw a fresh context (no main-transcript marker).
            let content1 = seen.lock().unwrap()[0].user_content();
            assert!(!content1.contains("main_transcript_marker"), "repl leaked main: {content1}");
            assert!(content1.contains("hello there"));

            // The second turn sees the FIRST exchange (isolated transcript grew).
            let _r2 = session.repl_turn("and again").await;
            let content2 = seen.lock().unwrap()[1].user_content();
            assert!(content2.contains("first reply"), "repl turn2 missing history: {content2}");
            assert!(content2.contains("hello there"), "repl turn2 missing prior prompt");

            // Exiting renders the REPL session; the main transcript still has only its own marker.
            let rendered = String::from_utf8(session.repl_end()).unwrap();
            assert!(rendered.contains("first reply") && rendered.contains("and again"));
            let main = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
            assert!(main.contains("main_transcript_marker"));
            assert!(!main.contains("first reply"), "REPL content leaked into main mid-session");
        });
    }

    /// `:model` switches the REPL's model; `:new-session` clears its transcript; `:exit` signals exit.
    #[test]
    fn repl_meta_commands() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.set_ask_provider(Box::new(FakeProvider::reply(
                "ok",
                std::sync::Arc::new(Mutex::new(Vec::new())),
            )));
            let args = crate::ai::ask::ReplArgs {
                model: None,
                seed: crate::ai::ask::ReplSeed::Fresh,
            };
            session.repl_start(&args).unwrap();
            assert_eq!(session.repl_model().as_deref(), Some(crate::ai::ask::DEFAULT_MODEL));

            // :model switches (anthropic/ prefix stripped).
            let (out, exit) = session.repl_meta(":model anthropic/claude-sonnet-5").unwrap();
            assert!(!exit);
            assert!(out.contains("claude-sonnet-5"));
            assert_eq!(session.repl_model().as_deref(), Some("claude-sonnet-5"));

            // A prompt grows the transcript; :new-session clears it.
            session.repl_turn("hi").await;
            let (_out, _exit) = session.repl_meta(":new-session").unwrap();
            // After clearing, the next turn sees no prior history.
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply("ok2", seen.clone())));
            session.repl_turn("fresh start").await;
            let content = seen.lock().unwrap()[0].user_content();
            assert!(!content.contains("hi"), "new-session should have cleared history: {content}");

            // :exit signals exit; a non-meta line returns None.
            assert_eq!(session.repl_meta(":exit").unwrap().1, true);
            assert!(session.repl_meta("just a prompt").is_none());
        });
    }

    /// `ask`'s reply is recorded into the transcript like any command output, so a follow-up `ask`
    /// (or `context show`) sees the prior exchange — the README "run a command, ask about it" loop.
    #[test]
    fn ask_reply_is_recorded_in_transcript() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.set_ask_provider(Box::new(FakeProvider::reply(
                "recorded_reply_xyz",
                std::sync::Arc::new(Mutex::new(Vec::new())),
            )));

            session.run_line(r#"sudo ask "q""#).await;
            let (transcript, _) = session.run_line("context show").await;
            let transcript = String::from_utf8(transcript).unwrap();
            assert!(
                transcript.contains("recorded_reply_xyz"),
                "the ask reply should be in the transcript, got: {transcript}"
            );
        });
    }

    /// Without a provider (the native default), `ask` degrades to a clean error (exit 4), not a panic.
    #[test]
    fn ask_without_provider_reports_not_configured() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let result = session.eval_line(r#"sudo ask "hi""#).await;
            assert_eq!(result.exit_code, 4);
            assert!(String::from_utf8(result.stderr)
                .unwrap()
                .contains("no model provider configured"));
        });
    }

    /// Bare `ask` (no sudo) surfaces the outbound-HTTP confirmation, like curl/wget — it does not
    /// call the provider until approved.
    #[test]
    fn ask_surfaces_a_confirmation() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply(
                "should not run yet",
                seen.clone(),
            )));

            let result = session.eval_line(r#"ask "hi""#).await;
            let pending = result.pending_prompt.expect("bare ask should surface a confirm");
            assert!(pending.question.to_lowercase().contains("ask"), "got: {}", pending.question);
            // The provider must NOT have run before approval.
            assert!(seen.lock().unwrap().is_empty(), "provider ran before approval");

            // Approving runs the deferred ask.
            let answered = session.answer_prompt(Some("yes".to_string())).await;
            assert_eq!(answered.exit_code, 0);
            assert_eq!(String::from_utf8(answered.stdout).unwrap(), "should not run yet");
        });
    }

    // ---- A2: the agentic shell-tool loop --------------------------------------------------------

    /// The model calls the `shell` tool once, the loop runs the command, feeds back the result, and
    /// the model answers. The tool result carries the command's stdout; the trace is on stderr.
    #[test]
    fn ask_shell_tool_runs_command_and_feeds_result_back() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![
                    shell_tool_call("c1", "echo MARK_42"),
                    crate::ai::ask::AskResponse::text("done: I saw MARK_42"),
                ],
                seen.clone(),
            )));

            let result = session.eval_line(r#"sudo ask "echo the marker""#).await;
            assert_eq!(result.exit_code, 0);
            assert_eq!(String::from_utf8(result.stdout).unwrap(), "done: I saw MARK_42");
            // Trace framing on stderr.
            let stderr = String::from_utf8(result.stderr).unwrap();
            assert!(stderr.contains("[tool] $ echo MARK_42"), "got: {stderr}");
            assert!(stderr.contains("[tool] exit 0"), "got: {stderr}");
            // The tool result fed back carried the command's stdout.
            let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
            let payload = tr.outcome.expect("shell tool succeeded");
            assert!(payload.contains("MARK_42"), "result payload: {payload}");
        });
    }

    /// Under a plain (approved, non-sudo) ask, a `confirm`-policy tool line (curl) PAUSES for
    /// authorization (A3); denying it feeds a "denied by user" result back and the loop continues.
    #[test]
    fn ask_confirm_tool_pauses_and_deny_continues() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![
                    shell_tool_call("c1", "curl https://example.com"),
                    crate::ai::ask::AskResponse::text("I could not fetch it"),
                ],
                seen.clone(),
            )));

            // Bare ask → surfaces the ask confirmation; approve it (blanket stays false).
            let first = session.eval_line(r#"ask "fetch it""#).await;
            assert!(first.pending_prompt.is_some(), "bare ask should confirm first");
            let second = session.answer_prompt(Some("yes".to_string())).await;
            // The curl tool call now surfaces its OWN authorization pause.
            let pending = second.pending_prompt.expect("curl tool should pause for authz");
            assert!(pending.question.to_lowercase().contains("permission"), "got: {}", pending.question);
            // Deny it → loop continues, model answers, ask exits 0.
            let result = session.answer_prompt(Some("no".to_string())).await;
            assert_eq!(result.exit_code, 0);
            assert_eq!(String::from_utf8(result.stdout).unwrap(), "I could not fetch it");

            let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
            let msg = tr.outcome.expect_err("denied curl is an error result");
            assert!(msg.contains("denied by user"), "got: {msg}");
        });
    }

    /// Even under `sudo ask`, a `sudo-only` tool line (rm) still PAUSES (blanket covers confirm-tier
    /// only); denying it leaves the file intact.
    #[test]
    fn ask_sudo_only_tool_pauses_even_under_sudo_ask() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            // `rm` is sudo-only in the default policy table; try to delete a marker file.
            let path = std::env::temp_dir().join("clank_ask_sudoonly_proof");
            std::fs::write(&path, b"keep").unwrap();
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![
                    shell_tool_call("c1", &format!("rm {}", path.display())),
                    crate::ai::ask::AskResponse::text("could not remove"),
                ],
                seen.clone(),
            )));

            let first = session.eval_line(&format!(r#"sudo ask "delete {}""#, path.display())).await;
            // Even under sudo ask, the sudo-only rm pauses (no "all" offered for this tier).
            let pending = first.pending_prompt.expect("sudo-only rm should pause under sudo ask");
            assert!(!pending.choices.clone().unwrap_or_default().contains(&"all".to_string()),
                "sudo-only pause must not offer 'all'");
            let result = session.answer_prompt(Some("no".to_string())).await;
            assert_eq!(result.exit_code, 0);
            let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
            assert!(tr.outcome.is_err(), "denied rm is an error result");
            assert!(path.exists(), "the file must survive the denied rm");
            std::fs::remove_file(&path).ok();
        });
    }

    /// `sudo ask` pre-authorizes confirm-tier up front: a curl tool call runs without any pause and
    /// its body comes back in the tool result.
    #[test]
    fn ask_sudo_pre_authorizes_confirm_tool() {
        on_rt(async {
            let url = http_mock("fetched-body");
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![
                    shell_tool_call("c1", &format!("curl {url}")),
                    crate::ai::ask::AskResponse::text("done"),
                ],
                seen.clone(),
            )));

            let first = session.eval_line(r#"sudo ask "fetch it""#).await;
            // sudo ask grants blanket confirm-tier up front → curl does not pause.
            assert!(first.pending_prompt.is_none(), "sudo ask pre-authorizes curl (no pause)");
            assert_eq!(first.exit_code, 0);
            let tr = last_tool_result(&seen, "c1").unwrap().outcome.unwrap();
            assert!(tr.contains("fetched-body"), "curl body should be in the tool result: {tr}");
        });
    }

    /// A `curl` under a plain approved ask pauses; answering "all" runs it and pre-authorizes a second
    /// confirm-tier call in a later turn (no second pause).
    #[test]
    fn ask_all_answer_upgrades_blanket_mid_loop() {
        on_rt(async {
            let url_a = http_mock("body-a");
            let url_b = http_mock("body-b");
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![
                    shell_tool_call("c1", &format!("curl {url_a}")),
                    shell_tool_call("c2", &format!("curl {url_b}")),
                    crate::ai::ask::AskResponse::text("done"),
                ],
                seen.clone(),
            )));

            // Plain ask (blanket false): approve the ask, then the first curl pauses.
            session.eval_line(r#"ask "fetch a then b""#).await;
            let after_ask = session.answer_prompt(Some("yes".to_string())).await;
            assert!(after_ask.pending_prompt.is_some(), "first curl should pause");
            // Answer "all" → runs c1 AND pre-authorizes c2 (no second pause) → loop completes.
            let done = session.answer_prompt(Some("all".to_string())).await;
            assert!(done.pending_prompt.is_none(), "all should carry through to c2");
            assert_eq!(done.exit_code, 0);
            assert_eq!(String::from_utf8(done.stdout).unwrap(), "done");
            // Both curls actually ran.
            assert!(last_tool_result(&seen, "c1").unwrap().outcome.unwrap().contains("body-a"));
            assert!(last_tool_result(&seen, "c2").unwrap().outcome.unwrap().contains("body-b"));
        });
    }

    /// The `prompt_user` tool pauses the loop with the model's question; the human's answer becomes the
    /// tool result and the loop continues.
    #[test]
    fn ask_prompt_user_tool_round_trips() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            let prompt_call = crate::ai::ask::AskResponse {
                text: String::new(),
                tool_calls: vec![crate::ai::ask::AskToolCall {
                    id: "p1".into(),
                    name: crate::ai::ask::PROMPT_USER_TOOL.into(),
                    arguments_json: serde_json::json!({ "question": "What port should I use?" })
                        .to_string(),
                }],
                finished_for_tools: true,
                error: None,
            };
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![prompt_call, crate::ai::ask::AskResponse::text("using port 8080")],
                seen.clone(),
            )));

            let first = session.eval_line(r#"sudo ask "set up the server""#).await;
            let pending = first.pending_prompt.expect("prompt_user should pause");
            assert!(pending.question.contains("port"), "got: {}", pending.question);
            let done = session.answer_prompt(Some("8080".to_string())).await;
            assert_eq!(done.exit_code, 0);
            assert_eq!(String::from_utf8(done.stdout).unwrap(), "using port 8080");
            // The answer reached the model as the tool result.
            let tr = last_tool_result(&seen, "p1").unwrap();
            assert!(tr.outcome.unwrap().contains("8080"), "answer should be the tool result");
        });
    }

    /// Killing the paused ask row (or Ctrl-C) aborts the whole ask: exit 130.
    #[test]
    fn ask_pause_kill_aborts_the_whole_ask() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            let prompt_call = crate::ai::ask::AskResponse {
                text: String::new(),
                tool_calls: vec![crate::ai::ask::AskToolCall {
                    id: "p1".into(),
                    name: crate::ai::ask::PROMPT_USER_TOOL.into(),
                    arguments_json: serde_json::json!({ "question": "continue?" }).to_string(),
                }],
                finished_for_tools: true,
                error: None,
            };
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![prompt_call, crate::ai::ask::AskResponse::text("should not reach")],
                seen.clone(),
            )));

            let first = session.eval_line(r#"sudo ask "do a thing""#).await;
            assert!(first.pending_prompt.is_some(), "prompt_user should pause");
            // Abort (as a kill of the paused row would, via answer_prompt(None)).
            let aborted = session.answer_prompt(None).await;
            assert_eq!(aborted.exit_code, 130);
            assert!(session.pending.is_none(), "no pending after abort");
        });
    }

    /// `model default X` (via the builtin) makes `ask` target X; an explicit `--model` overrides it.
    /// Exercises the full ask.toml resolution chain through a real Session.
    #[test]
    fn ask_uses_model_default_and_flag_overrides() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![
                    crate::ai::ask::AskResponse::text("a"),
                    crate::ai::ask::AskResponse::text("b"),
                ],
                seen.clone(),
            )));
            // Point HOME at a unique temp dir so ask.toml is hermetic (nanos avoids cross-test clash).
            let home = std::env::temp_dir().join(format!(
                "clank_ask_model_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&home).unwrap();
            session
                .run_line(&format!("export HOME={}", home.display()))
                .await;

            // Set the default, then a bare ask should use it (prefix stripped to the bare id).
            session
                .run_line("model default anthropic/claude-sonnet-4-5")
                .await;
            session.run_line(r#"sudo ask --fresh "hi""#).await;
            assert_eq!(seen.lock().unwrap().last().unwrap().model, "claude-sonnet-4-5");

            // An explicit --model overrides the default.
            session
                .run_line(r#"sudo ask --fresh --model claude-haiku-4-5 "hi""#)
                .await;
            assert_eq!(seen.lock().unwrap().last().unwrap().model, "claude-haiku-4-5");

            std::fs::remove_dir_all(&home).ok();
        });
    }

    /// An unknown provider prefix in `--model` fails before any model call (exit 2).
    #[test]
    fn ask_unknown_provider_prefix_errors() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::reply("x", seen.clone())));
            let result = session
                .eval_line(r#"sudo ask --model openai/gpt-4o --fresh "hi""#)
                .await;
            assert_eq!(result.exit_code, 2);
            assert!(String::from_utf8(result.stderr).unwrap().contains("unknown provider"));
            // The provider was never called.
            assert!(seen.lock().unwrap().is_empty());
        });
    }

    /// The model trying to call `ask` recursively via the shell tool is refused.
    #[test]
    fn ask_recursion_is_refused() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![
                    shell_tool_call("c1", "ask what is 2+2"),
                    crate::ai::ask::AskResponse::text("ok"),
                ],
                seen.clone(),
            )));

            session.eval_line(r#"sudo ask "recurse""#).await;
            let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
            let msg = tr.outcome.expect_err("ask recursion should be refused");
            assert!(msg.contains("itself"), "got: {msg}");
        });
    }

    /// A `shell`-internal command (`context`) is refused as a tool: it mutates state a tool can't
    /// reach. (Also guards `cd`/`export`/`kill` by the same scope check.)
    #[test]
    fn ask_shell_internal_command_is_refused() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![
                    shell_tool_call("c1", "context clear"),
                    crate::ai::ask::AskResponse::text("ok"),
                ],
                seen.clone(),
            )));

            session.eval_line(r#"sudo ask "clear it""#).await;
            let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
            let msg = tr.outcome.expect_err("context should be refused as a tool");
            assert!(msg.contains("shell-internal"), "got: {msg}");
        });
    }

    /// Malformed tool arguments produce an honest error result; the loop continues.
    #[test]
    fn ask_malformed_tool_args_error_and_continue() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            let bad = crate::ai::ask::AskResponse {
                text: String::new(),
                tool_calls: vec![crate::ai::ask::AskToolCall {
                    id: "c1".into(),
                    name: crate::ai::ask::SHELL_TOOL.into(),
                    arguments_json: "{".into(), // not valid JSON
                }],
                finished_for_tools: true,
                error: None,
            };
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![bad, crate::ai::ask::AskResponse::text("recovered")],
                seen.clone(),
            )));

            let result = session.eval_line(r#"sudo ask "do something""#).await;
            assert_eq!(String::from_utf8(result.stdout).unwrap(), "recovered");
            let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
            assert!(tr.outcome.unwrap_err().contains("malformed"), "expected a malformed-args error");
        });
    }

    /// The loop stops at the iteration cap when the model calls a tool every turn, exiting 0 with a
    /// stderr notice. The provider is called exactly `ASK_MAX_ITERATIONS` times.
    #[test]
    fn ask_loop_stops_at_the_iteration_cap() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
            // More tool-call responses than the cap: the loop must stop at the cap.
            let script: Vec<_> = (0..ASK_MAX_ITERATIONS + 5)
                .map(|i| shell_tool_call(&format!("c{i}"), "echo loop"))
                .collect();
            session.set_ask_provider(Box::new(FakeProvider::scripted(script, seen.clone())));

            let result = session.eval_line(r#"sudo ask "loop forever""#).await;
            assert_eq!(result.exit_code, 0);
            assert!(
                String::from_utf8(result.stderr).unwrap().contains("tool-call limit"),
                "expected a cap notice on stderr"
            );
            // Exactly cap turns were requested.
            assert_eq!(seen.lock().unwrap().len(), ASK_MAX_ITERATIONS);
        });
    }

    /// Two `ask` calls in a row both succeed — the provider is take()n and restored each time. A
    /// forgotten restore would make the second ask report "not configured".
    #[test]
    fn ask_provider_is_restored_between_calls() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.set_ask_provider(Box::new(FakeProvider::scripted(
                vec![
                    crate::ai::ask::AskResponse::text("first"),
                    crate::ai::ask::AskResponse::text("second"),
                ],
                std::sync::Arc::new(Mutex::new(Vec::new())),
            )));

            let a = session.eval_line(r#"sudo ask --fresh "one""#).await;
            assert_eq!(String::from_utf8(a.stdout).unwrap(), "first");
            let b = session.eval_line(r#"sudo ask --fresh "two""#).await;
            assert_eq!(b.exit_code, 0, "second ask must not report not-configured");
            assert_eq!(String::from_utf8(b.stdout).unwrap(), "second");
        });
    }

    /// `<cmd> --help` for an intercepted command prints its manifest help text and exits 0, through
    /// `eval_line`. These commands never reach Brush's dispatch, so this is the only place they get
    /// `--help`. Crucially, `curl --help` does NOT surface the outbound-HTTP confirmation — it's a
    /// help query, handled before the authz gate.
    #[test]
    fn help_flag_prints_help_for_intercepted_commands() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();

            let result = session.eval_line("curl --help").await;
            assert_eq!(result.exit_code, 0);
            assert!(result.pending_prompt.is_none(), "curl --help must not confirm");
            let out = String::from_utf8(result.stdout).unwrap();
            assert!(out.contains("fetch a URL over"), "got: {out}");

            let result = session.eval_line("prompt-user --help").await;
            assert_eq!(result.exit_code, 0);
            assert!(result.pending_prompt.is_none(), "help must not surface a prompt");
            assert!(String::from_utf8(result.stdout).unwrap().contains("pause the"));

            let result = session.eval_line("wget --help").await;
            assert_eq!(result.exit_code, 0);
            assert!(String::from_utf8(result.stdout).unwrap().contains("download a URL"));

            let result = session.eval_line("context --help").await;
            assert_eq!(result.exit_code, 0);
            assert!(String::from_utf8(result.stdout)
                .unwrap()
                .contains("session transcript"));
        });
    }

    /// A non-`--secret` `prompt-user` error is recorded in the transcript like any other command
    /// (only `--secret` *responses* are redacted, per the README — this line never reached the
    /// point of collecting a response).
    #[test]
    fn prompt_user_error_is_recorded_in_transcript() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.run_line("prompt-user --bogus").await;
            let (transcript, _) = session.run_line("context show").await;
            let transcript = String::from_utf8(transcript).unwrap();
            assert!(transcript.contains("prompt-user --bogus"));
            assert!(transcript.contains("unknown flag"));
        });
    }

    /// The registry carries a manifest for `prompt-user` even though it's never registered as a
    /// Brush `SimpleCommand` (it's intercepted before dispatch) — `type`/tool-surface consumers
    /// should still see it.
    #[test]
    fn prompt_user_has_a_registry_manifest() {
        on_rt(async {
            let session = Session::new().await.unwrap();
            let manifest = session
                .registry()
                .get("prompt-user")
                .expect("prompt-user should have a manifest");
            assert_eq!(
                manifest.execution_scope,
                crate::manifest::ExecutionScope::ShellInternal
            );
        });
    }

    /// The full two-step path: `prompt-user` surfaces the question (returns immediately with
    /// `pending_prompt` set, does NOT hang), then `answer_prompt` delivers the response to stdout
    /// with exit 0, recorded in the transcript (not `--secret`), and clears the pending state.
    #[test]
    fn prompt_user_surfaces_then_answer_resolves() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();

            // Step 1: surface. The question comes back and the prompt is pending — no hang.
            let surfaced = session.eval_line(r#"prompt-user "Which environment?""#).await;
            assert_eq!(surfaced.exit_code, 0);
            let pending = surfaced.pending_prompt.expect("should surface a pending prompt");
            assert_eq!(pending.question, "Which environment?");
            assert!(session.has_pending_prompt());

            // Step 2: answer. The response flows to stdout, pending clears.
            let answered = session.answer_prompt(Some("production".to_string())).await;
            assert_eq!(String::from_utf8(answered.stdout).unwrap(), "production\n");
            assert_eq!(answered.exit_code, 0);
            assert!(answered.pending_prompt.is_none());
            assert!(!session.has_pending_prompt());

            let (transcript, _) = session.run_line("context show").await;
            let transcript = String::from_utf8(transcript).unwrap();
            assert!(transcript.contains("production"), "got: {transcript}");
        });
    }

    /// `kill <pid>` of the P-state prompt-paused row is the one command allowed through while a
    /// prompt is pending: it aborts the prompt (exit 130, same as an explicit abort). Any other
    /// command — and a kill of a DIFFERENT pid — stays rejected.
    #[test]
    fn kill_of_pending_prompt_pid_aborts_it() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.eval_line(r#"prompt-user "q?""#).await;
            assert!(session.has_pending_prompt());
            let paused_pid = {
                let table = session.proc_table.lock().unwrap();
                let row = table
                    .rows()
                    .iter()
                    .find(|r| r.state == crate::proctable::ProcState::P)
                    .expect("a paused row");
                row.pid
            };

            // A kill of some other pid is rejected like any other command.
            let other = session.eval_line(&format!("kill {}", paused_pid + 100)).await;
            assert_eq!(other.exit_code, 1);
            assert!(session.has_pending_prompt());

            // Killing the paused pid aborts the prompt: exit 130, pending cleared, row reaped.
            let killed = session.eval_line(&format!("kill {paused_pid}")).await;
            assert_eq!(killed.exit_code, 130);
            assert!(!session.has_pending_prompt());
            let after = session.eval_line("echo ok").await;
            assert_eq!(String::from_utf8(after.stdout).unwrap(), "ok\n");
        });
    }

    /// An aborted answer (`None`) exits 130 with no stdout (README) and clears the pending prompt.
    #[test]
    fn prompt_user_abort_exits_130() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.eval_line(r#"prompt-user "q?""#).await;
            let result = session.answer_prompt(None).await;
            assert!(result.stdout.is_empty());
            assert_eq!(result.exit_code, 130);
            assert!(!session.has_pending_prompt());
        });
    }

    /// An answer outside the prompt's `--choices` errors and leaves the prompt pending to re-ask.
    #[test]
    fn prompt_user_invalid_choice_keeps_prompt_pending() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session
                .eval_line(r#"prompt-user "Approve?" --confirm"#)
                .await;
            let bad = session.answer_prompt(Some("maybe".to_string())).await;
            assert_eq!(bad.exit_code, 1);
            assert!(session.has_pending_prompt(), "prompt should stay pending");

            // A valid choice then resolves it.
            let ok = session.answer_prompt(Some("yes".to_string())).await;
            assert_eq!(String::from_utf8(ok.stdout).unwrap(), "yes\n");
            assert!(!session.has_pending_prompt());
        });
    }

    /// A `--secret` response is never entered into the transcript (README), though the command
    /// line itself is still recorded.
    #[test]
    fn prompt_user_secret_response_is_redacted_from_transcript() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session
                .eval_line(r#"prompt-user "Enter the API key:" --secret"#)
                .await;
            let result = session.answer_prompt(Some("s3cr3t-key".to_string())).await;
            // The caller still gets the response on stdout...
            assert_eq!(String::from_utf8(result.stdout).unwrap(), "s3cr3t-key\n");

            // ...but it must not appear in the transcript.
            let (transcript, _) = session.run_line("context show").await;
            let transcript = String::from_utf8(transcript).unwrap();
            assert!(
                !transcript.contains("s3cr3t-key"),
                "secret response leaked into transcript: {transcript}"
            );
            // The command line itself is still recorded.
            assert!(transcript.contains("prompt-user"));
        });
    }

    /// While a prompt is pending, an ordinary command is rejected — the caller must answer first.
    #[test]
    fn command_while_prompt_pending_is_rejected() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.eval_line(r#"prompt-user "q?""#).await;
            let blocked = session.eval_line("echo hi").await;
            assert_ne!(blocked.exit_code, 0);
            assert!(
                String::from_utf8(blocked.terminal_output())
                    .unwrap()
                    .contains("awaiting a response"),
                "expected a 'answer the prompt first' error"
            );
            // The prompt is still pending and still answerable.
            assert!(session.has_pending_prompt());
        });
    }

    /// `answer_prompt` with no prompt outstanding is a clean error, not a panic.
    #[test]
    fn answer_prompt_with_no_pending_is_an_error() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let result = session.answer_prompt(Some("x".to_string())).await;
            assert_ne!(result.exit_code, 0);
        });
    }

    /// A seeded temp file for `rm` tests: returns its path. Uses a unique name per test.
    fn seed_file(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("clank_authz_{tag}_{}", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        path
    }

    /// `rm` is `sudo-only`: without elevation it surfaces a confirmation instead of deleting, and
    /// the file survives. (The gate composes on the same pending-prompt pause as `prompt-user`.)
    #[test]
    fn sudo_only_rm_without_sudo_surfaces_confirmation() {
        on_rt(async {
            let path = seed_file("gate");
            let mut session = Session::new().await.unwrap();
            let result = session
                .eval_line(&format!("rm {}", path.display()))
                .await;
            // A confirmation was surfaced, not a deletion.
            assert!(result.pending_prompt.is_some(), "rm should surface a sudo confirmation");
            assert!(session.has_pending_prompt());
            assert!(path.exists(), "file must survive an unapproved sudo-only rm");
            let _ = std::fs::remove_file(&path);
        });
    }

    /// Denying (`no`) a `sudo-only` `rm` confirmation → exit 5, file survives, pending clears.
    #[test]
    fn sudo_only_rm_denied_returns_exit_5() {
        on_rt(async {
            let path = seed_file("deny");
            let mut session = Session::new().await.unwrap();
            session.eval_line(&format!("rm {}", path.display())).await;
            let denied = session.answer_prompt(Some("no".to_string())).await;
            assert_eq!(denied.exit_code, 5, "denial is exit 5");
            assert!(path.exists(), "denied rm must not delete");
            assert!(!session.has_pending_prompt());
            let _ = std::fs::remove_file(&path);
        });
    }

    /// Approving (`yes`) a `sudo-only` `rm` confirmation runs the deferred command — the file is
    /// deleted, exit 0.
    #[test]
    fn sudo_only_rm_approved_runs_the_command() {
        on_rt(async {
            let path = seed_file("approve");
            let mut session = Session::new().await.unwrap();
            session.eval_line(&format!("rm {}", path.display())).await;
            let approved = session.answer_prompt(Some("yes".to_string())).await;
            assert_eq!(approved.exit_code, 0, "approved rm succeeds");
            assert!(!path.exists(), "approved rm must delete the file");
            assert!(!session.has_pending_prompt());
        });
    }

    /// A `sudo rm` prefix pre-authorizes the sudo-only command — it runs immediately, no prompt.
    #[test]
    fn sudo_prefix_bypasses_the_gate() {
        on_rt(async {
            let path = seed_file("sudo");
            let mut session = Session::new().await.unwrap();
            let result = session
                .eval_line(&format!("sudo rm {}", path.display()))
                .await;
            assert!(result.pending_prompt.is_none(), "sudo rm should not prompt");
            assert_eq!(result.exit_code, 0);
            assert!(!path.exists(), "sudo rm deletes immediately");
        });
    }

    /// An `allow`-policy command (e.g. `echo`) is completely unaffected by the gate.
    #[test]
    fn allow_policy_command_is_ungated() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let result = session.eval_line("echo hi").await;
            assert!(result.pending_prompt.is_none());
            assert_eq!(String::from_utf8(result.stdout).unwrap(), "hi\n");
            assert_eq!(result.exit_code, 0);
        });
    }

    /// A one-shot localhost HTTP server (raw `std::net`, no dep) that replies `200 <body>` once.
    /// Hermetic — the `curl`/`wget` interception is exercised end-to-end without real internet.
    fn http_mock(body: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    /// `curl` is `confirm`-policy (outbound HTTP): it surfaces a confirmation before the request runs.
    #[test]
    fn curl_surfaces_a_confirmation() {
        on_rt(async {
            let url = http_mock("body");
            let mut session = Session::new().await.unwrap();
            let result = session.eval_line(&format!("curl {url}")).await;
            assert!(result.pending_prompt.is_some(), "curl should surface a confirm");
            assert!(session.has_pending_prompt());
            // No request ran yet: resolve the prompt so the mock thread can exit cleanly.
            session.answer_prompt(Some("no".to_string())).await;
        });
    }

    /// Approving a `curl` confirmation runs the request — the body comes back on stdout, exit 0.
    /// Proves the post-approval deferred path routes through the HTTP dispatch in `run_command`.
    #[test]
    fn curl_approved_runs_the_request() {
        on_rt(async {
            let url = http_mock("approved-body");
            let mut session = Session::new().await.unwrap();
            session.eval_line(&format!("curl {url}")).await;
            let out = session.answer_prompt(Some("yes".to_string())).await;
            assert_eq!(out.exit_code, 0);
            assert_eq!(String::from_utf8(out.stdout).unwrap(), "approved-body");
        });
    }

    /// Denying a `curl` confirmation → exit 5, no request.
    #[test]
    fn curl_denied_returns_exit_5() {
        on_rt(async {
            let url = http_mock("never");
            let mut session = Session::new().await.unwrap();
            session.eval_line(&format!("curl {url}")).await;
            let out = session.answer_prompt(Some("no".to_string())).await;
            assert_eq!(out.exit_code, 5);
        });
    }

    /// `sudo curl` pre-authorizes: the request runs immediately, no prompt. Proves the direct-allow
    /// path also routes through the HTTP dispatch (not Brush's `execute`).
    #[test]
    fn sudo_curl_bypasses_gate_and_fetches() {
        on_rt(async {
            let url = http_mock("sudo-body");
            let mut session = Session::new().await.unwrap();
            let out = session.eval_line(&format!("sudo curl {url}")).await;
            assert!(out.pending_prompt.is_none(), "sudo curl should not prompt");
            assert_eq!(out.exit_code, 0);
            assert_eq!(String::from_utf8(out.stdout).unwrap(), "sudo-body");
        });
    }

    /// `curl -o <file>` (approved) writes the body to a file, stdout empty.
    #[test]
    fn curl_o_writes_a_file() {
        on_rt(async {
            let url = http_mock("file-body");
            let path = std::env::temp_dir().join(format!("clank_curl_o_{}", std::process::id()));
            let mut session = Session::new().await.unwrap();
            session
                .eval_line(&format!("sudo curl -o {} {url}", path.display()))
                .await;
            // (sudo → no prompt; runs immediately)
            let out = std::fs::read_to_string(&path).unwrap();
            let _ = std::fs::remove_file(&path);
            assert_eq!(out, "file-body");
        });
    }

    /// `wget -O -` (approved) streams the body to stdout.
    #[test]
    fn wget_dash_o_to_stdout() {
        on_rt(async {
            let url = http_mock("wget-body");
            let mut session = Session::new().await.unwrap();
            let out = session.eval_line(&format!("sudo wget -O - {url}")).await;
            assert_eq!(out.exit_code, 0);
            assert_eq!(String::from_utf8(out.stdout).unwrap(), "wget-body");
        });
    }

    /// `curl`/`wget` carry `Subprocess`/`Confirm` manifests in the registry.
    #[test]
    fn http_commands_have_confirm_manifests() {
        on_rt(async {
            let session = Session::new().await.unwrap();
            for name in ["curl", "wget"] {
                let m = session.registry().get(name).expect("manifest");
                assert_eq!(m.execution_scope, crate::manifest::ExecutionScope::Subprocess);
                assert_eq!(
                    m.authorization_policy,
                    crate::manifest::AuthorizationPolicy::Confirm
                );
            }
        });
    }

    /// `$PATH` is set to clank's README default (the virtual package-resolution namespace).
    #[test]
    fn path_is_the_readme_default() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let (out, _) = session.run_line("echo $PATH").await;
            assert_eq!(String::from_utf8(out).unwrap(), format!("{DEFAULT_PATH}\n"));
        });
    }

    /// `which` finds nothing for a name with no file-backed form, and does NOT report a phantom
    /// path (the bug caught on the agent: Brush's wasm `executable()` returns true unconditionally,
    /// so `which` must verify existence itself). Chained with a marker to prove no wedge/error.
    #[test]
    fn which_finds_nothing_for_a_nonexistent_command() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let (out, _) = session
                .run_line("which clank-no-such-cmd-xyz; echo done")
                .await;
            let out = String::from_utf8(out).unwrap();
            assert!(out.trim_end().ends_with("done"), "got: {out}");
            assert!(
                !out.contains("clank-no-such-cmd-xyz"),
                "which must not report a phantom path for a missing command: {out}"
            );
        });
    }

    /// `which` finds a real executable file placed on `$PATH`.
    #[test]
    fn which_finds_a_real_path_file() {
        on_rt(async {
            let dir = std::env::temp_dir().join(format!("clank_which_bin_{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let exe = dir.join("clank-which-probe");
            std::fs::write(&exe, b"#!/bin/sh\ntrue\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
            }

            let mut session = Session::new().await.unwrap();
            // Prepend our dir to $PATH, then `which` should find the probe file.
            session
                .run_line(&format!("export PATH={}:$PATH", dir.display()))
                .await;
            let (out, _) = session.run_line("which clank-which-probe").await;
            let out = String::from_utf8(out).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            assert!(
                out.contains("clank-which-probe"),
                "which should find a real $PATH file, got: {out}"
            );
        });
    }

    /// Brush's own `type` still works (now with a clank manifest, but unchanged behavior) — it
    /// reports a clank builtin as a builtin. Guards the manifest-registration change against
    /// accidentally breaking Brush's builtin dispatch.
    #[test]
    fn type_reports_a_builtin() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let (out, _) = session.run_line("type ls").await;
            let out = String::from_utf8(out).unwrap();
            assert!(out.contains("ls"), "got: {out}");
            assert!(out.contains("builtin"), "type should call ls a builtin, got: {out}");
        });
    }

    /// `type`/`command`/`which` have registry manifests (the resolution surface sees them).
    #[test]
    fn resolution_commands_have_manifests() {
        on_rt(async {
            let session = Session::new().await.unwrap();
            for name in ["type", "command", "which"] {
                assert!(
                    session.registry().get(name).is_some(),
                    "{name} should have a manifest"
                );
            }
        });
    }

    #[test]
    fn cat_reads_virtual_proc_status() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let (out, _) = session.run_line("cat /proc/1/status").await;
            let out = String::from_utf8(out).unwrap();
            assert!(out.contains("Pid:"), "got: {out}");
            assert!(out.contains("State:"));
            assert!(out.contains("clank"));
        });
    }

    /// `ls /bin` enumerates every registered command name — intercepted (`curl`, `prompt-user`) and
    /// Brush-registered (`cat`) alike — so the AI can discover the full capability set. Virtual `/bin`.
    #[test]
    fn ls_bin_lists_all_commands() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let (out, _) = session.run_line("ls /bin").await;
            let out = String::from_utf8(out).unwrap();
            assert!(out.contains("curl"), "got: {out}");
            assert!(out.contains("prompt-user"));
            assert!(out.contains("cat"));
        });
    }

    /// `cat /bin/<name>` prints the command's help text — the virtual file is `cat`-able like a
    /// `/proc` file. Covers an intercepted command (`curl`, invisible to Brush's own resolution).
    #[test]
    fn cat_bin_curl_shows_help() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let (out, _) = session.run_line("cat /bin/curl").await;
            let out = String::from_utf8(out).unwrap();
            assert!(out.contains("fetch a URL over"), "got: {out}");
        });
    }

    /// `cat /bin/<unknown>` reports "No such file or directory" (like a real missing file), not a
    /// spurious success — the virtual namespace only serves registered names.
    #[test]
    fn cat_bin_unknown_is_not_found() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let (out, _) = session.run_line("cat /bin/does-not-exist").await;
            let out = String::from_utf8(out).unwrap();
            assert!(out.contains("No such file or directory"), "got: {out}");
        });
    }

    /// `grep` output is captured via `context.stdout()` (the `run_tool` path) — this is
    /// parallel-safe (no process-global fd swap) and verifies the wasm output-capture fix on the
    /// native side too.
    #[test]
    fn grep_captures_output() {
        on_rt(async {
            let dir = std::env::temp_dir();
            let path = dir.join(format!("clank_grep_test_{}", std::process::id()));
            std::fs::write(&path, b"alpha\nbeta\ngamma\n").unwrap();
            let mut session = Session::new().await.unwrap();
            let (out, _) = session
                .run_line(&format!("grep beta {}", path.display()))
                .await;
            let _ = std::fs::remove_file(&path);
            let out = String::from_utf8(out).unwrap();
            assert!(
                out.contains("beta"),
                "grep output should contain the match, got: {out}"
            );
            assert!(
                !out.contains("alpha"),
                "grep should not emit non-matching lines: {out}"
            );
        });
    }

    /// `grep` over a virtual `/proc` file works and its output is captured.
    #[test]
    fn grep_matches_virtual_proc_file() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let (out, _) = session.run_line("grep State /proc/1/status").await;
            let out = String::from_utf8(out).unwrap();
            assert!(out.contains("State"), "got: {out}");
        });
    }

    /// A real-file `cat` still works after `cat` became a `/proc`-aware shim (delegation intact).
    #[test]
    fn cat_still_reads_real_files() {
        on_rt(async {
            let dir = std::env::temp_dir();
            let path = dir.join(format!("clank_cat_test_{}", std::process::id()));
            std::fs::write(&path, b"real-file-contents\n").unwrap();
            let mut session = Session::new().await.unwrap();
            let (out, _) = session.run_line(&format!("cat {}", path.display())).await;
            let _ = std::fs::remove_file(&path);
            let out = String::from_utf8(out).unwrap();
            assert!(out.contains("real-file-contents"), "got: {out}");
        });
    }

    /// PIDs persist and keep climbing across `run_line` calls (the durable-agent property, tested
    /// locally): the second command gets a higher PID than the first.
    #[test]
    fn pids_are_monotonic_across_lines() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.run_line("echo one").await;
            session.run_line("echo two").await;
            let (ps_out, _) = session.run_line("ps").await;
            let ps_out = String::from_utf8(ps_out).unwrap();

            let pid_of = |needle: &str| -> u32 {
                ps_out
                    .lines()
                    .find(|l| l.contains(needle))
                    .and_then(|l| l.split_whitespace().next())
                    .and_then(|p| p.parse().ok())
                    .unwrap_or_else(|| panic!("no pid for {needle} in:\n{ps_out}"))
            };
            assert!(pid_of("echo two") > pid_of("echo one"));
        });
    }
}
