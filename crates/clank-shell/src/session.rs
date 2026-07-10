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
//! cannot substitute for that. On wasm, pipelines/subshells that reach `spawn_blocking` are a
//! known limitation (no threads); simple builtins and shell language work.

use crate::{dispatch_context, promptuser, typecmd, Flow, Transcript};
use brush_builtins::{BuiltinSet, ShellBuilderExt};
use brush_core::openfiles::{OpenFile, OpenFiles};
use brush_core::{ExecutionControlFlow, Shell, SourceInfo};

use std::sync::{Arc, Mutex};

use crate::authz::{self, AuthzState, Decision};
use crate::process::ProcessKind;
use crate::promptuser::{AnswerInput, PendingPrompt, Resolution};
use crate::proctable::ProcessTable;
use crate::registry::CommandRegistry;

type BoxError = Box<dyn std::error::Error>;

/// Why the shell is paused awaiting a response — set alongside the [`PendingPrompt`].
enum PendingKind {
    /// A `prompt-user` invocation: the answer is returned to the caller verbatim.
    UserPrompt,
    /// An authorization confirmation gating a command: on approval the stashed `command` runs; on
    /// denial the caller gets exit `5`. `all` (when offered) also sets the session `allow_all` grant.
    AuthConfirm { command: String, sudo_grant: bool },
}

/// The shell's paused state: the surfaced prompt, the process-table row it belongs to, and why.
struct Pending {
    prompt: PendingPrompt,
    pid: Option<u32>,
    kind: PendingKind,
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
    transcript: Transcript,
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
    /// The injected LLM provider for `ask`. Installed by the agent build (a durable Anthropic
    /// provider); `None` on native and until injected, in which case `ask` degrades to a clean
    /// "not configured" error. See [`crate::askcmd`].
    ask_provider: Option<Box<dyn crate::askcmd::AskProvider>>,
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
            Ok(Self {
                shell,
                transcript: Transcript::new(),
                registry: crate::registry::build(),
                proc_table: Arc::new(Mutex::new(ProcessTable::new())),
                pending: None,
                authz: AuthzState::default(),
                ask_provider: None,
                source: SourceInfo::default(),
                rt,
            })
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let shell = build_shell().await?;
            Ok(Self {
                shell,
                transcript: Transcript::new(),
                registry: crate::registry::build(),
                proc_table: Arc::new(Mutex::new(ProcessTable::new())),
                pending: None,
                authz: AuthzState::default(),
                ask_provider: None,
                source: SourceInfo::default(),
            })
        }
    }

    /// The command registry — clank's inventory of command manifests.
    pub fn registry(&self) -> &CommandRegistry {
        &self.registry
    }

    /// Install the LLM provider that backs `ask`. The agent build injects a durable Anthropic
    /// provider here after constructing the session; without one, `ask` reports "not configured".
    pub fn set_ask_provider(&mut self, provider: Box<dyn crate::askcmd::AskProvider>) {
        self.ask_provider = Some(provider);
    }

    /// Evaluate one input line: record it, serve the clank-specific `context` builtin, otherwise
    /// execute it through Brush.
    pub async fn eval_line(&mut self, line: &str) -> LineResult {
        // A prompt is already outstanding: the caller must answer it (via `answer_prompt`), not run
        // a new command. The shell never blocks, so it's the caller's job to notice `pending_prompt`
        // and respond. Reject the command with a clear message rather than silently interleaving.
        if self.pending.is_some() {
            return LineResult::stderr(
                "clank: a prompt-user question is awaiting a response; answer it first\n",
            );
        }

        self.transcript.record_command(line);

        // Install this session's process table as the active one for the duration of the line, so
        // the `ps` builtin (a Brush builtin, which can't reach `Session` directly) can read it.
        // The guard clears the slot on drop.
        let _install = crate::proctable::install(self.proc_table.clone());

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

        // `context show` output is intentionally not recorded back into the transcript.
        if let Some(bytes) = dispatch_context(&mut self.transcript, line) {
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

        // Authorization gate: enforce the leading command's `authorization-policy` (README). A
        // `confirm`/`sudo-only` command that isn't pre-authorized surfaces a confirmation pause
        // (reusing the `prompt-user` mechanism) and defers the command until approved. In every
        // path the command actually run is the line with any leading `sudo` token stripped — `sudo`
        // is a clank authorization marker, not a real executable to dispatch to Brush.
        let (policy, elevated, command) = authz::resolve(&self.registry, line);
        let effective = strip_sudo_prefix(line);
        match authz::decide(policy, elevated, self.authz.allow_all) {
            Decision::Allow => {}
            Decision::Deny => {
                return self.finish_intercepted(pid, LineResult::denied());
            }
            Decision::Confirm { sudo_grant } => {
                return self.surface_auth_confirm(command.as_deref(), effective, pid, sudo_grant);
            }
        }

        let result = self.run_command(&effective, pid).await;
        self.transcript.record_output(&result.terminal_output());
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
    async fn run_command(&mut self, line: &str, pid: Option<u32>) -> LineResult {
        let result = if let Some(args) = crate::askcmd::classify(line) {
            // `ask` dispatches to the injected LLM provider — same "async/sync at the Session layer,
            // never through `execute`'s nested runtime" rule as curl/wget. The provider's call is
            // synchronous (the durable Anthropic provider blocks internally via the Golem host), so
            // there's no `.await` here, but it MUST run on this agent invocation where the durable
            // context is live — which is exactly `run_command`. See `askcmd`.
            self.run_ask(args)
        } else {
            match crate::httpcmd::classify(line) {
                Some((crate::httpcmd::HttpCommand::Curl, args)) => {
                    let o = wcurl::run(&args).await;
                    LineResult::from_outcome(o.stdout, o.stderr, o.exit_code)
                }
                Some((crate::httpcmd::HttpCommand::Wget, args)) => {
                    let o = waget::run(&args).await;
                    LineResult::from_outcome(o.stdout, o.stderr, o.exit_code)
                }
                None => self.execute(line).await,
            }
        };
        if let Some(pid) = pid {
            self.proc_table.lock().unwrap().complete(pid);
        }
        result
    }

    /// Assemble the `ask` request (transcript window as context, unless `--fresh`) and call the
    /// injected provider. If no provider is installed (e.g. the native build), degrade to a clean
    /// "not configured" error (exit 4) rather than panicking — the README's "features that require
    /// Golem fail with informative errors."
    fn run_ask(&self, args: crate::askcmd::AskArgs) -> LineResult {
        let Some(provider) = self.ask_provider.as_ref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"ask: no model provider configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };
        // The base context is the same bytes `context show` renders — "the AI reads exactly what you
        // see." `--fresh` sends no transcript. stdin-after-transcript is a later increment.
        let transcript = if args.fresh {
            String::new()
        } else {
            String::from_utf8_lossy(&self.transcript.render()).into_owned()
        };
        let request = crate::askcmd::AskRequest {
            system: Some(crate::askcmd::CORE_SYSTEM_PROMPT.to_string()),
            transcript,
            stdin: None,
            prompt: args.prompt,
            model: args.model,
        };
        let o = provider.complete(request);
        LineResult::from_outcome(o.stdout, o.stderr, o.exit_code)
    }

    /// Complete an intercepted line's row and record its output (for intercepted paths that don't go
    /// through `run_command`, e.g. an authorization denial).
    fn finish_intercepted(&mut self, pid: Option<u32>, result: LineResult) -> LineResult {
        if let Some(pid) = pid {
            self.proc_table.lock().unwrap().complete(pid);
        }
        self.transcript.record_output(&result.terminal_output());
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
    ) -> LineResult {
        let name = command_name.unwrap_or("command");
        let synopsis = self
            .registry
            .get(name)
            .map(|m| m.synopsis.clone())
            .unwrap_or_else(|| "run this command".to_string());
        let prompt = PendingPrompt {
            question: authz::confirm_question(name, &synopsis, sudo_grant),
            choices: Some(authz::confirm_choices(sudo_grant)),
            secret: false,
        };
        self.surface_pending(
            prompt,
            pid,
            PendingKind::AuthConfirm {
                command: gated_command,
                sudo_grant,
            },
        )
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
        self.transcript.record_output(&stdout);

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
    pub async fn answer_prompt(&mut self, response: Option<String>) -> LineResult {
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
            } => {
                self.resolve_auth_confirm(resolution, &command, sudo_grant, pending.pid)
                    .await
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
            self.transcript.record_output(&stdout);
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
            // "no" or abort → denied (exit 5). Reap the row.
            if let Some(pid) = pid {
                self.proc_table.lock().unwrap().complete(pid);
            }
            let result = LineResult::denied();
            self.transcript.record_output(&result.terminal_output());
            return result;
        }

        if grant_all {
            self.authz.allow_all = true;
        }

        // Approved: run the gated command, reusing the row (still `R` after resume) and reaping it.
        let result = self.run_command(command, pid).await;
        self.transcript.record_output(&result.terminal_output());
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

/// The README's default `$PATH` — the resolution namespace clank's package layout installs into.
/// Nothing populates `/usr/lib/{mcp,agents,prompts}/bin` or the skills glob yet (that's `grease`,
/// future); these entries currently resolve to nothing, which is correct — `type`/`which` degrade
/// to "not found" rather than erroring on a missing directory.
const DEFAULT_PATH: &str =
    "/usr/local/bin:/usr/bin:/usr/lib/mcp/bin:/usr/lib/agents/bin:/usr/lib/prompts/bin:/usr/share/skills/*/bin";

async fn build_shell() -> Result<Shell, brush_core::Error> {
    // NB: clank's builtins are registered here AND their manifests in `registry::build()`; the two
    // must stay in lockstep (the registry drift-guard test enforces it). Adding a builtin via
    // `Shell::register_builtin` directly would bypass the manifest — don't.
    let mut shell = Shell::builder()
        .default_builtins(BuiltinSet::BashMode)
        .builtins(crate::coreutils::builtins())
        .builtins(crate::texttools::builtins())
        .builtins(crate::ps::builtins())
        .builtins(crate::which::builtins())
        .build()
        .await?;

    // Set clank's `$PATH` explicitly, overriding whatever Brush's init seeded (empty on the wasm
    // stub, the host's real PATH on native — both wrong for clank's virtual namespace). Read by
    // `$PATH` expansion and by `type`/`which` path resolution alike.
    shell.env_mut().set_global(
        "PATH",
        brush_core::variables::ShellVariable::new(DEFAULT_PATH),
    )?;

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

    /// A fake `AskProvider` for tests: returns a canned reply and records the last request it saw,
    /// so tests can assert what context `ask` assembled (transcript-as-context).
    #[derive(Clone, Default)]
    struct FakeProvider {
        reply: String,
        seen: std::sync::Arc<Mutex<Option<crate::askcmd::AskRequest>>>,
    }

    impl crate::askcmd::AskProvider for FakeProvider {
        fn complete(&self, request: crate::askcmd::AskRequest) -> crate::askcmd::AskOutcome {
            *self.seen.lock().unwrap() = Some(request);
            crate::askcmd::AskOutcome::reply(self.reply.clone())
        }
    }

    /// With a provider installed, `ask` returns the model's reply on stdout (exit 0), and the request
    /// it assembled carries the current transcript as context (the README "transcript is the context").
    /// `ask` is `Confirm`-gated, so `sudo ask` is used here to skip the confirmation pause.
    #[test]
    fn ask_returns_reply_and_feeds_transcript_as_context() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(None));
            session.set_ask_provider(Box::new(FakeProvider {
                reply: "the answer is 42".to_string(),
                seen: seen.clone(),
            }));

            // Run a command first so there's transcript history to feed as context.
            session.run_line("echo marker_abc").await;

            let result = session.eval_line(r#"sudo ask "what did I just echo?""#).await;
            assert_eq!(result.exit_code, 0);
            assert!(result.pending_prompt.is_none(), "sudo ask must not confirm");
            assert_eq!(
                String::from_utf8(result.stdout).unwrap(),
                "the answer is 42"
            );

            // The provider saw the prompt and the transcript (including the prior echo).
            let req = seen.lock().unwrap().clone().expect("provider should have run");
            assert_eq!(req.prompt, "what did I just echo?");
            assert_eq!(req.model, crate::askcmd::DEFAULT_MODEL);
            assert!(
                req.transcript.contains("marker_abc"),
                "transcript context should include the prior echo, got: {}",
                req.transcript
            );
        });
    }

    /// `--fresh` sends no transcript context; the prompt still reaches the provider.
    #[test]
    fn ask_fresh_sends_empty_transcript() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            let seen = std::sync::Arc::new(Mutex::new(None));
            session.set_ask_provider(Box::new(FakeProvider {
                reply: "ok".to_string(),
                seen: seen.clone(),
            }));

            session.run_line("echo should_not_appear").await;
            let result = session.eval_line(r#"sudo ask --fresh "hi""#).await;
            assert_eq!(result.exit_code, 0);

            let req = seen.lock().unwrap().clone().unwrap();
            assert!(req.transcript.is_empty(), "got: {}", req.transcript);
            assert_eq!(req.prompt, "hi");
        });
    }

    /// `ask`'s reply is recorded into the transcript like any command output, so a follow-up `ask`
    /// (or `context show`) sees the prior exchange — the README "run a command, ask about it" loop.
    #[test]
    fn ask_reply_is_recorded_in_transcript() {
        on_rt(async {
            let mut session = Session::new().await.unwrap();
            session.set_ask_provider(Box::new(FakeProvider {
                reply: "recorded_reply_xyz".to_string(),
                seen: std::sync::Arc::new(Mutex::new(None)),
            }));

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
            let seen = std::sync::Arc::new(Mutex::new(None));
            session.set_ask_provider(Box::new(FakeProvider {
                reply: "should not run yet".to_string(),
                seen: seen.clone(),
            }));

            let result = session.eval_line(r#"ask "hi""#).await;
            let pending = result.pending_prompt.expect("bare ask should surface a confirm");
            assert!(pending.question.to_lowercase().contains("ask"), "got: {}", pending.question);
            // The provider must NOT have run before approval.
            assert!(seen.lock().unwrap().is_none(), "provider ran before approval");

            // Approving runs the deferred ask.
            let answered = session.answer_prompt(Some("yes".to_string())).await;
            assert_eq!(answered.exit_code, 0);
            assert_eq!(String::from_utf8(answered.stdout).unwrap(), "should not run yet");
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
