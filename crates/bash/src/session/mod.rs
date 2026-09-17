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
//! That nested runtime is also why async work is dispatched from `run_command` rather than from a
//! Brush builtin — see `docs/architecture/wall-c.md`. The virtual `/bin`, `/proc` and `/mnt/mcp`
//! namespaces this dispatch resolves against are `docs/architecture/resolution-surface.md`.

use crate::builtins::{promptuser, typecmd};
use crate::{dispatch_context, Flow, Transcript};
use brush_builtins::{BuiltinSet, ShellBuilderExt};
use brush_core::openfiles::{OpenFile, OpenFiles};
use brush_core::{ExecutionControlFlow, Shell, SourceInfo};

use std::sync::{Arc, Mutex};

use crate::authz::{self, AuthzState, Decision};
use crate::builtins::promptuser::{AnswerInput, PendingPrompt, Resolution};
use crate::registry::CommandRegistry;
use crate::runtime::proctable::ProcessKind;
use crate::runtime::proctable::ProcessTable;

type BoxError = Box<dyn std::error::Error>;

mod ctx;
// `pub` for `env::effective_path`: the `$PATH` a session installs is now built in two halves (the
// core's, plus the installed plug-in's `path_dirs`), and the drift guard pinning the two against
// the README default lives with the plug-in that supplies the second half — in another crate.
pub mod env;
mod prompt;
mod stateless;
mod streams;
mod tool_commands;

pub use ctx::SessionCtx;
pub use tool_commands::ShellContinuation;

use crate::plugin::Plugin;

// What the eval pipeline still reaches for after the relocation. Everything else that used to live
// here moved to the module that owns it AND is now only called from there — the short length of this
// list is the evidence that the split fell along a real seam rather than an arbitrary one.
use env::{build_shell, ensure_fs_layout};
use streams::finish;
// The wasm capture adapters; the code that constructs them is `cfg`-gated the same way.
#[cfg(target_arch = "wasm32")]
use streams::{BufSink, BufSource};

/// Why the shell is paused awaiting a response — set alongside the [`PendingPrompt`].
enum PendingKind {
    /// A `prompt-user` invocation: the answer is returned to the caller verbatim.
    UserPrompt,
    /// An authorization confirmation gating a command: on approval the stashed `command` runs; on
    /// denial the caller gets exit `5`. `all` (when offered) also sets the session `allow_all` grant.
    /// `rerun_stdin` carries a pre-captured pipeline stdin payload for a deferred `cat x | ask` tail, so
    /// the piped context survives the pause/resume (restored into `rerun_stdin` before re-running).
    AuthConfirm {
        command: String,
        sudo_grant: bool,
        rerun_stdin: Option<String>,
    },
    /// A plug-in paused mid-flight and owns the continuation (an `ask` agentic loop whose tool call
    /// needs the human, say). The shell carries the opaque payload and hands it straight back to
    /// [`crate::plugin::Plugin::resume`] when the human answers. Replay-safe on the durable agent —
    /// Golem rebuilds it by deterministic replay.
    Plugin(crate::plugin::PluginPending),
}

/// The shell's paused state: the surfaced prompt, the process-table row it belongs to, and why.
struct Pending {
    prompt: PendingPrompt,
    pid: Option<u32>,
    kind: PendingKind,
}

/// A per-line snapshot of the installed capability surface, keyed by the plug-in's version.
/// Rebuilt only when that key changes; otherwise the same `Arc`s are re-installed each line (a cheap
/// clone) instead of re-rendering the manifests / resource index / system prompt every command.
struct CapabilityView {
    version: u64,
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

/// The result of evaluating one shell line.
pub struct LineResult {
    /// The line's captured standard output.
    pub stdout: Vec<u8>,
    /// The line's captured standard error.
    pub stderr: Vec<u8>,
    /// The line's exit status (`0` on success; see the README exit-code table).
    pub exit_code: u8,
    /// Whether the shell should continue or exit after this line.
    pub flow: Flow,
    /// Set when this line surfaced a `prompt-user` question the shell is now awaiting a response
    /// to. The caller must collect a human answer and deliver it via [`Session::answer_prompt`]
    /// (the shell does not block). `None` for every ordinary line.
    pub pending_prompt: Option<PendingPrompt>,
}

impl LineResult {
    /// Bytes as a terminal would display them through the legacy `run_line` API.
    #[must_use]
    pub fn terminal_output(&self) -> Vec<u8> {
        let mut output = self.stdout.clone();
        output.extend_from_slice(&self.stderr);
        output
    }

    /// A successful line: `stdout`, no stderr, exit `0`, keep looping.
    #[must_use]
    pub fn continue_with_stdout(stdout: Vec<u8>) -> Self {
        Self {
            stdout,
            stderr: Vec::new(),
            exit_code: 0,
            flow: Flow::Continue,
            pending_prompt: None,
        }
    }

    /// Override the exit code, keeping the rest of the result.
    ///
    /// For commands that aggregate sub-results into one stdout blob and must still report the worst
    /// outcome — the exit code is the only machine-readable channel a non-interactive driver has, so
    /// "printed some failures, returned 0" is a lie to it.
    #[must_use]
    pub fn with_exit_code(mut self, exit_code: u8) -> Self {
        self.exit_code = exit_code;
        self
    }

    /// A failed line: `message` on stderr, nothing on stdout, exit `1`.
    #[must_use]
    pub fn stderr(message: impl Into<Vec<u8>>) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: message.into(),
            exit_code: 1,
            flow: Flow::Continue,
            pending_prompt: None,
        }
    }

    /// An authorization failure: exit `5` (README) with a stderr message.
    #[must_use]
    pub fn denied() -> Self {
        Self {
            stdout: Vec::new(),
            stderr: b"clank: authorization denied\n".to_vec(),
            exit_code: 5,
            flow: Flow::Continue,
            pending_prompt: None,
        }
    }

    /// Build a result from an HTTP command's outcome (`wcurl`/`waget` return the same shape).
    #[must_use]
    pub fn from_outcome(stdout: Vec<u8>, stderr: Vec<u8>, exit_code: u8) -> Self {
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
    tools: Option<Arc<crate::agent_tools::ToolRuntime>>,
    stateless: bool,
    /// The session transcript. Shared behind `Arc<Mutex>` (like `proc_table`) so each executed
    /// line can install it into the thread-local slot the Brush-registered `context` builtin
    /// reads — that's how `$(context show)` and `context show | head` reach it.
    transcript: Arc<Mutex<Transcript>>,
    /// The clank-owned inventory of command manifests (sits beside `transcript` as a shell-owned
    /// state object): the core surface plus any installed plug-in's. Drives command metadata
    /// surfaces. Behind an `Arc` (like `transcript` and `proc_table`) so each executed line can
    /// install it into the thread-local slot the `/bin`-reading builtins (`cat`/`ls`/`man`/`stat`)
    /// resolve against — that's how `ls /bin` lists a plug-in's commands too.
    registry: Arc<CommandRegistry>,
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
    /// process table (the `JoinHandles` themselves are rebuilt by re-execution).
    bg_jobs: Vec<BgJob>,
    /// The installed plug-in (clank's command families), if any. Taken out for the duration of a
    /// line by `eval_line`/`answer_prompt` and passed down dispatch, so it can re-enter through
    /// `SessionCtx::run_command` by handing itself back.
    plugin: Option<Box<dyn crate::plugin::Plugin>>,
    /// Stdin captured for a confirmed line to replay when it re-runs (`cat x | ask` confirmed later).
    rerun_stdin: Option<String>,
    /// The per-line capability view, rebuilt when the plug-in's version changes.
    capabilities: Option<CapabilityView>,
    /// The log sink installed per-line (the `/var/log` observability layer). Defaults to the direct
    /// append sink (correct on native); the agent injects a whole-file-rewrite sink whose writes are
    /// idempotent under oplog replay, avoiding line duplication (see `logging` + `log_sink`).
    log_sink: std::sync::Arc<dyn crate::logging::LogSink>,
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

/// What [`Session::classify_line`] decided a line is: which rung of the historical interception
/// ladder it matches, resolved AFTER the guard clauses in `eval_line_inner` that must run
/// unconditionally (or mutate a `Session` field directly, which a `&self` classifier can't do) —
/// see that function's doc for the split.
///
/// **This order is a behavioural contract, not a style choice.** `classify_line` tests these in the
/// exact order the ladder always has, and `eval_line_inner`'s `match` on the result must keep that
/// order — reordering either changes what the shell does. Concretely: `Help` must resolve before
/// `Plugin`/`Brush` (the authz gate) so `<cmd> --help` never triggers a confirmation; the
/// `BeforeContext` `Plugin` ask must resolve before `ContextDispatch` so `context summarize` routes
/// to the model instead of `context`'s "unknown subcommand"; every variant here must resolve before
/// `Brush` so inspecting/help-ing a command never confirms it.
enum LineRoute {
    /// Syntactically incomplete input (an unterminated heredoc/quote/substitution).
    IncompleteInput,
    /// `<cmd> --help` for a clank-intercepted command; carries the rendered help text.
    Help(String),
    /// A line the installed plug-in claimed — its own help text, or a route it will run itself. The
    /// plug-in is asked twice (see [`crate::plugin::LinePhase`]), so this variant stands in at BOTH
    /// of the positions its family checks used to occupy.
    Plugin(crate::plugin::LineAction),
    /// `context show`/`clear`/`trim` (not `summarize`); carries the already-rendered output. The
    /// one variant here that mutates as part of deciding (`clear`/`trim` change the transcript) —
    /// still safe in a `&self` classifier because the mutation goes through `self.transcript`'s
    /// `Mutex`, not a `Session` field, so nothing here needs `&mut Session` the way
    /// `run_secret_export`/`surface_prompt` do. Kept as one call into `dispatch_context` (rather
    /// than a separate pure pre-check duplicating its operator-scan) so there is exactly one place
    /// that knows what counts as a `context` line.
    ContextDispatch(Vec<u8>),
    /// `prompt-user ...`.
    PromptUser,
    /// `export --secret NAME=VALUE` as a standalone command.
    SecretExport,
    /// `export --secret ...` carrying shell operators — refused outright rather than falling
    /// through to Brush's own unredacted `export`.
    SecretExportRefused,
    /// A `type` line resolved entirely against clank-intercepted names; carries the rendered output
    /// and exit code.
    TypeDispatch(String, u8),
    /// None of the above: falls through to the authorization gate, then `run_command`.
    Brush,
}

/// What [`Session::classify_command`] decided a line is, once `eval_line_inner`'s authorization
/// gate has already resolved. Same order-is-behaviour contract as [`LineRoute`] — see that enum's
/// doc and `classify_command`'s.
enum CommandRoute {
    /// `kill ...`, parsed (or a parse error to report).
    Kill(crate::error::Result<crate::builtins::kill::KillArgs>),
    /// A line the installed plug-in claimed; carries its own opaque route back to it.
    Plugin(crate::plugin::Route),
    /// A curl/wget-headed pipeline (`curl … | rest…`).
    HttpPipe(crate::builtins::http::HttpHeadPipe),
    /// A bare (unpiped) curl/wget invocation; carries its argv tail.
    HttpDirect(crate::builtins::http::HttpCommand, Vec<String>),
    /// None of the above: an ordinary line to run through Brush's `execute`.
    Execute,
}

impl Session {
    /// Build a non-interactive shell with the full bash-compatible builtin set.
    ///
    /// No plug-in is installed: the shell core cannot name a command family. An embedder that wants
    /// one calls [`set_plugin`](Self::set_plugin) — for clank's families, `ClankSessionExt`'s
    /// `install_clank`.
    ///
    /// # Errors
    /// Returns `Err` if the Brush shell fails to build or its `$PATH`/`$HOME` seeding fails (and,
    /// on wasm, if the current-thread tokio runtime cannot be constructed).
    // wasm builds the shell via `rt.block_on` (no `.await`), so clippy sees an unused `async`; the
    // native arm awaits and the signature must stay async for both. Suppress on wasm only
    // (clippy 1.98 reports the same condition under `unused_async_trait_impl` as well).
    #[cfg_attr(
        target_arch = "wasm32",
        allow(clippy::unused_async, clippy::unused_async_trait_impl)
    )]
    pub async fn new() -> Result<Self, BoxError> {
        // The core namespace only; an installed plug-in's own dirs are created by `set_plugin`.
        ensure_fs_layout(&[]);
        #[cfg(target_arch = "wasm32")]
        {
            // wasip2 has no threads: a current-thread runtime drives Brush's async.
            let rt = tokio::runtime::Builder::new_current_thread().build()?;
            let shell = rt.block_on(build_shell())?;
            let session = Self {
                shell,
                tools: None,
                stateless: false,
                transcript: Arc::new(Mutex::new(Transcript::with_cap(
                    crate::configured_context_cap(),
                ))),
                registry: Arc::new(crate::registry::build()),
                proc_table: Arc::new(Mutex::new(ProcessTable::new())),
                pending: None,
                authz: AuthzState::default(),
                bg_jobs: Vec::new(),
                plugin: None,
                rerun_stdin: None,
                capabilities: None,
                log_sink: std::sync::Arc::new(crate::logging::DefaultLogSink),
                source: SourceInfo::default(),
                secret_env: std::collections::BTreeMap::new(),
                rt,
            };
            Ok(session)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let shell = build_shell().await?;
            let session = Self {
                shell,
                tools: None,
                stateless: false,
                transcript: Arc::new(Mutex::new(Transcript::with_cap(
                    crate::configured_context_cap(),
                ))),
                registry: Arc::new(crate::registry::build()),
                proc_table: Arc::new(Mutex::new(ProcessTable::new())),
                pending: None,
                authz: AuthzState::default(),
                bg_jobs: Vec::new(),
                plugin: None,
                rerun_stdin: None,
                capabilities: None,
                log_sink: std::sync::Arc::new(crate::logging::DefaultLogSink),
                source: SourceInfo::default(),
                secret_env: std::collections::BTreeMap::new(),
            };
            Ok(session)
        }
    }

    /// Test-only: set the transcript safety cap at runtime to force eviction. There is no user
    /// command for this — production sets the cap once at construction from [`crate::configured_context_cap`].
    ///
    /// Behind the `test-support` feature so a consumer crate's suite (the plug-in's `ask` tests,
    /// which drive auto-compaction) can force the same eviction without a second copy of the knob.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_context_cap(&self, cap_tokens: usize) {
        self.transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_cap(cap_tokens);
    }

    /// The command registry — clank's inventory of command manifests.
    #[must_use]
    pub fn registry(&self) -> &CommandRegistry {
        &self.registry
    }

    /// Install `plugin`: its layout dirs are created, its builtins join the shell, its manifests
    /// join the registry, its `path_dirs` become `$PATH`'s second half, and its `on_start` runs.
    /// Genuinely replaces any previous plug-in: the old one's builtins are disabled and its
    /// manifests and `$PATH` entries are dropped before the new plug-in's are installed, so calling
    /// `set_plugin` a second time (an embedder swapping configurations) does not hit
    /// [`CommandRegistry::insert`]'s duplicate-name assert and does not leave `$PATH` carrying a mix
    /// of the old and new plug-ins' directories.
    ///
    /// The filesystem layout comes first: the plug-in's `on_start` may materialize files into its
    /// own dirs, and the `$PATH` it extends has to resolve them.
    pub fn set_plugin(&mut self, mut plugin: Box<dyn crate::plugin::Plugin>) {
        // Undo the previous plug-in's effect on the shell first, so installing this one is a
        // genuine replace rather than an accumulation.
        if let Some(old) = self.plugin.take() {
            self.disable_plugin_builtins(old.as_ref());
        }

        ensure_fs_layout(&plugin.layout_dirs());
        for (name, registration) in plugin.builtins() {
            self.shell.register_builtin(name, registration);
        }
        // Rebuilt from the core surface rather than mutated in place: a previous plug-in's
        // manifests must not survive into this registry (see the doc above).
        let mut registry = crate::registry::build();
        for manifest in plugin.manifests() {
            registry.insert(manifest);
        }
        self.registry = Arc::new(registry);

        // Rebuild, rather than extend, `$PATH`: recomputed from the core half plus THIS plug-in's
        // own `path_dirs` every time (not read back from whatever the live `$PATH` currently holds),
        // so a replaced plug-in's directories never linger and re-installing the same plug-in never
        // appends them a second time. Unconditional — even an empty `path_dirs` must still clear a
        // predecessor's.
        let path = env::effective_path(&plugin.path_dirs());
        let _ = self
            .shell
            .env_mut()
            .set_global("PATH", brush_core::variables::ShellVariable::new(path));

        plugin.on_start();
        self.capabilities = None;
        self.plugin = Some(plugin);
        self.refresh_tools();
    }

    /// Disable `plugin`'s builtins on the shell (Brush has no unregister; a disabled registration
    /// falls through to ordinary command resolution — exactly what "not installed" means). Shared by
    /// [`set_plugin`](Self::set_plugin) (replacing a previous plug-in) and
    /// [`clear_plugin_for_test`](Self::clear_plugin_for_test) (removing one outright).
    fn disable_plugin_builtins(&mut self, plugin: &dyn crate::plugin::Plugin) {
        for (name, _registration) in plugin.builtins() {
            if let Some(registration) = self.shell.builtin_mut(&name) {
                registration.disabled = true;
            }
        }
    }

    /// The installed plug-in as `T`, if it is one.
    #[must_use]
    pub fn plugin_ref<T: crate::plugin::Plugin>(&self) -> Option<&T> {
        self.plugin.as_deref()?.as_any().downcast_ref::<T>()
    }

    /// The installed plug-in as mutable `T`, if it is one.
    pub fn plugin_mut<T: crate::plugin::Plugin>(&mut self) -> Option<&mut T> {
        self.plugin.as_deref_mut()?.as_any_mut().downcast_mut::<T>()
    }

    /// Run `f` with the installed plug-in taken out and a context on this session, then put it
    /// back. `None` when no plug-in of type `T` is installed.
    ///
    /// The plug-in must leave the slot for the call: `f` gets `&mut T` and a [`SessionCtx`] on the
    /// same `Session` at once, which is the shape a plug-in's own entry points need (an `ask repl`
    /// turn runs commands back through the shell while holding its REPL state).
    pub async fn with_plugin<T: crate::plugin::Plugin, R>(
        &mut self,
        f: impl AsyncFnOnce(&mut T, &mut SessionCtx<'_>) -> R,
    ) -> Option<R> {
        let mut plugin = self.plugin.take()?;
        let result = match plugin.as_any_mut().downcast_mut::<T>() {
            Some(typed) => Some(f(typed, &mut SessionCtx::new(self)).await),
            None => None,
        };
        self.plugin = Some(plugin);
        result
    }

    /// [`with_plugin`](Self::with_plugin) for a synchronous plug-in entry point.
    pub fn with_plugin_sync<T: crate::plugin::Plugin, R>(
        &mut self,
        f: impl FnOnce(&mut T, &mut SessionCtx<'_>) -> R,
    ) -> Option<R> {
        let mut plugin = self.plugin.take()?;
        let result = plugin
            .as_any_mut()
            .downcast_mut::<T>()
            .map(|typed| f(typed, &mut SessionCtx::new(self)));
        self.plugin = Some(plugin);
        result
    }

    /// Test-only: uninstall the plug-in, so the contract tests can see what the bare shell core
    /// does. There is no user command for this — a session starts with no plug-in and an embedder
    /// installs one with [`set_plugin`](Self::set_plugin).
    ///
    /// Undoes what `set_plugin` did to the shell, so "no plug-in" means it everywhere: the builtins
    /// it registered are disabled (Brush has no unregister, and a disabled registration falls
    /// through to ordinary command resolution — which is exactly what "not installed" means), and
    /// its manifests go by rebuilding the core registry. Its `$PATH` entries stay; they name
    /// directories, and an empty directory on `$PATH` resolves nothing.
    #[cfg(test)]
    pub(crate) fn clear_plugin_for_test(&mut self) {
        if let Some(plugin) = self.plugin.take() {
            self.disable_plugin_builtins(plugin.as_ref());
        }
        self.registry = Arc::new(crate::registry::build());
        self.capabilities = None;
    }

    /// Install the `/var/log` log sink. The agent injects a whole-file-rewrite sink (idempotent under
    /// oplog replay, so no duplicated lines); native keeps the default direct-append sink.
    pub fn set_log_sink(&mut self, sink: std::sync::Arc<dyn crate::logging::LogSink>) {
        self.log_sink = sink;
    }

    /// The shell's current working directory — Brush's tracked `working_dir`, which `cd` updates
    /// (see the `ShellCwd` guard; clank never moves the *process* cwd). Surfaced so an interactive
    /// caller (e.g. `golem agent shell`) can show the cwd in its prompt and reflect `cd` live.
    #[must_use]
    pub fn cwd(&self) -> &std::path::Path {
        self.shell.working_dir()
    }

    /// Set the shell's `COLUMNS` to the terminal width, so a terminal-style `ls` (and other columnar
    /// output) lays its columns out to the real window. The native REPL calls this per prompt from
    /// `crossterm::terminal::size()`; in `agent shell` the client sends `export COLUMNS=<w>` instead.
    /// A no-op if the value can't be stored (`COLUMNS` is a plain shell var, so this never fails in
    /// practice).
    pub fn set_columns(&mut self, cols: u16) {
        let mut var = brush_core::variables::ShellVariable::new(cols.to_string());
        var.export();
        let _ = self.shell.env_mut().set_global("COLUMNS", var);
    }

    /// The terminal width from the shell's `COLUMNS` (see [`set_columns`](Self::set_columns)), or
    /// `None` when unset — i.e. non-interactive (a script, `agent invoke`, the conformance harness),
    /// where clank's listings stay one-per-line. `Some` marks an interactive terminal, so columnar
    /// output is safe.
    pub(crate) fn columns(&self) -> Option<usize> {
        self.shell
            .env()
            .get("COLUMNS")
            .and_then(|(_, var)| {
                var.value()
                    .to_cow_str(&self.shell)
                    .trim()
                    .parse::<usize>()
                    .ok()
            })
            .filter(|w| *w > 0)
    }

    /// The shell's `$HOME` (seeded to `/home/user` on the agent), for locating `~/.config/ask/ask.toml`.
    pub(super) fn shell_home(&self) -> String {
        self.shell
            .env()
            .get_str("HOME", &self.shell)
            .map_or_else(|| DEFAULT_HOME.to_string(), std::borrow::Cow::into_owned)
    }

    /// Evaluate one input line: record it, serve the clank-specific `context` builtin, otherwise
    /// execute it through Brush.
    /// Evaluate one command line, logging its lifecycle to `shell.log`: a `start` event as the line
    /// begins and an `end` (with exit code) when it finishes — or a `pause` when it stops for a
    /// `prompt-user`/authorization question (the eventual `end` is logged when `answer_prompt` resolves
    /// it). The actual command dispatch lives in [`eval_line_inner`](Self::eval_line_inner).
    pub async fn eval_line(&mut self, line: &str) -> LineResult {
        // Record whether this line is a plain single command, so a bare `ls` may render like a
        // terminal (columns + colour) while a piped/redirected `ls` stays one-per-line. See
        // `note_simple_line`.
        crate::tools::coreutils::note_simple_line(line);
        // Install this session's log sink for the whole line so every logging call site (shell/http/mcp/
        // ops, deep in run_command / McpClient::call / coreutils) routes through it.
        let _log = crate::logging::install(self.log_sink.clone());
        // Name this line in `ops.log` if it panics. On wasm a panic ABORTS (wasm32-wasip2 is an
        // abort target), so there is nothing to catch — the hook running before the abort is the
        // only chance to record where the instance died and what it was running. Redacted the same
        // way the shell.log events are.
        crate::runtime::panicreport::install();
        let _panic_ctx = crate::runtime::panicreport::executing(log_safe_line(line).as_ref());
        if !line.trim().is_empty() {
            crate::logging::Record::new("start")
                .field("line", log_safe_line(line).as_ref())
                .emit(crate::logging::LogFile::Shell);
        }
        // The plug-in rides out of the slot for the whole line and back in at the end, so dispatch
        // can hand it a `&mut Session` (as a `SessionCtx`) and the plug-in itself at the same time —
        // which is what lets an `ask` tool call re-enter `run_command` through the plug-in.
        let mut plugin = self.plugin.take();
        let result = self.eval_line_inner(plugin.as_deref_mut(), line).await;
        self.plugin = plugin;
        self.log_line_outcome(line, &result);
        result
    }

    /// Emit the shell.log terminal event for a finished (or paused) line.
    // A method for call-site symmetry with `eval_line`; pairs with the per-`self` log-sink install.
    #[allow(clippy::unused_self)]
    fn log_line_outcome(&self, line: &str, result: &LineResult) {
        if line.trim().is_empty() {
            return;
        }
        let event = if result.pending_prompt.is_some() {
            "pause"
        } else {
            "end"
        };
        // Carry the shell's PID on every terminal event. `logging`'s module doc advertises
        // "PID/PPID-addressable audit events", but the ordinary start/end pair carried only `line`
        // — so two interleaved lines could not be told apart in the log, which is precisely when a
        // reader needs to. Only the answer_prompt path stamped a pid.
        let mut rec = crate::logging::Record::new(event)
            .field("pid", crate::runtime::proctable::SHELL_ROOT_PID.to_string())
            .field("line", log_safe_line(line).as_ref());
        if result.pending_prompt.is_none() {
            rec = rec.field("exit", result.exit_code.to_string());
        }
        rec.emit(crate::logging::LogFile::Shell);
    }

    // The single per-line dispatch pipeline: pending-prompt guard, secret-env install, transcript
    // record, then `classify_line`'s ordered intercept ladder ending at the authz gate — one linear
    // read. The guard clauses below all either mutate `Session` state directly or must run
    // unconditionally before any classification, which is why they stay here rather than moving
    // into `classify_line` — see that function's doc for the rule. Still over clippy's 100-line
    // default after extracting `classify_line`/`dispatch_via_authz_gate`: the ~130 mandatory setup
    // lines above the `match` are a hard floor.
    #[allow(clippy::too_many_lines)]
    async fn eval_line_inner(&mut self, plugin: Option<&mut dyn Plugin>, line: &str) -> LineResult {
        if self.stateless {
            if let Err(error) = stateless::validate(line) {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("bash: {error}\n").into_bytes(),
                    2,
                );
            }
        }
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
                self.transcript
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .record_command(line);
                // The plug-in is already out of the slot for this line, so hand it straight to the
                // resolver rather than going through `answer_prompt` (which would find an empty
                // slot and fail to resume a plug-in-owned pause).
                return self.answer_prompt_with_plugin(plugin, None).await;
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
                let redacted = line.replace(&secret.value, crate::runtime::secretenv::REDACTED);
                self.transcript
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .record_command(&redacted);
            }
            _ => {
                self.transcript
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .record_command(line);
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
        // Same pattern for the registry: the `/bin`-reading builtins (`cat /bin/<name>`, `ls /bin`,
        // `man`, `stat`) are Brush builtins with no path to `Session`, and the registry is
        // per-session (core + whatever plug-in is installed), so it rides the thread-local slot too.
        let _install_registry = crate::runtime::binfs::install(self.registry.clone());
        // Build (or reuse a cached) view of the installed capabilities, keyed by the plug-in's own
        // version — so the dynamic manifests (`man`/`type` resolution), the MCP resource index (`ls
        // /mnt/mcp/...`), and the live system prompt (`cat /proc/clank/system-prompt`) are re-rendered
        // only when a package/server was installed or removed, not on every command line.
        if let Some(p) = plugin.as_deref() {
            let version = p.version();
            if self.capabilities.as_ref().map(|c| c.version) != Some(version) {
                let caps = p.capabilities(&self.registry);
                self.capabilities = Some(CapabilityView {
                    version,
                    dynreg: std::sync::Arc::new(std::sync::Mutex::new(caps.manifests)),
                    mcpfs: std::sync::Arc::new(caps.resources),
                    sysprompt: std::sync::Arc::new(caps.system_prompt.unwrap_or_default()),
                });
            }
        }
        // Clone the cached `Arc`s into the per-line thread-local slots (cheap ref-count bumps); the
        // guards clear the slots on drop. The manifests/index/prompt are read-only surfaces, so sharing
        // one `Arc` across lines is safe.
        let (dynreg, mcpfs, sysprompt) = match self.capabilities.as_ref() {
            Some(c) => (c.dynreg.clone(), c.mcpfs.clone(), c.sysprompt.clone()),
            None => Default::default(),
        };
        let _install_dynreg = crate::runtime::dynreg::install(dynreg);
        let _install_mcpfs = crate::runtime::mcpfs::install(mcpfs);
        let _install_sysprompt =
            (!sysprompt.is_empty()).then(|| crate::runtime::sysprompt::install(sysprompt));

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
                Some(
                    self.proc_table
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .spawn(kind, argv),
                )
            }
        };

        // Classify the line, then dispatch on the result — see `classify_line`/`LineRoute` for the
        // order contract. Every arm below is a straight relocation of what used to be this
        // function's own sequential ladder; only the CONDITION moved to `classify_line`.
        let route = self.classify_line(plugin.as_deref(), line);
        match route {
            LineRoute::IncompleteInput => {
                // clank's Brush shell is non-interactive, so an unterminated heredoc/quote/
                // substitution would otherwise hit Brush's fatal-parse path (`finish` turns it into
                // `Flow::Exit`, ending the whole session). Answer honestly instead.
                let result = LineResult::from_outcome(
                    Vec::new(),
                    b"clank: incomplete input (a heredoc, quote, or substitution is missing its \
                      terminator); provide the full construct in one eval\n"
                        .to_vec(),
                    2,
                );
                self.finish_intercepted(pid, result)
            }
            LineRoute::Help(help) | LineRoute::Plugin(crate::plugin::LineAction::Help(help)) => {
                let result = LineResult::from_outcome(help.into_bytes(), Vec::new(), 0);
                self.finish_intercepted(pid, result)
            }
            LineRoute::Plugin(crate::plugin::LineAction::Intercept(route)) => match plugin {
                Some(p) => {
                    p.run(route, line, pid, false, &mut SessionCtx::new(self))
                        .await
                }
                None => self.finish_intercepted(
                    pid,
                    LineResult::stderr("clank: internal error: plug-in route with no plug-in\n"),
                ),
            },
            LineRoute::ContextDispatch(bytes) => {
                // `context show` output is intentionally not recorded back into the transcript.
                if let Some(pid) = pid {
                    self.proc_table
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .complete(pid);
                }
                LineResult::continue_with_stdout(bytes)
            }
            // `prompt-user` does NOT block: it records a durable pending prompt, leaves the row in
            // `P`, and returns immediately — surfacing the question to the caller, who answers via
            // `answer_prompt`. See the `promptuser` module docs.
            LineRoute::PromptUser => self.surface_prompt(line, pid),
            LineRoute::SecretExport => self.run_secret_export(line, pid),
            LineRoute::SecretExportRefused => {
                let result = LineResult::from_outcome(
                    Vec::new(),
                    b"export --secret: must be a standalone command (no pipes, `&&`, `;`, or \
                      substitutions) so the value never reaches an unredacted surface\n"
                        .to_vec(),
                    2,
                );
                self.finish_intercepted(pid, result)
            }
            LineRoute::TypeDispatch(stdout, exit_code) => {
                let result = LineResult::from_outcome(stdout.into_bytes(), Vec::new(), exit_code);
                self.finish_intercepted(pid, result)
            }
            LineRoute::Brush => self.dispatch_via_authz_gate(plugin, line, pid).await,
        }
    }

    /// The tail of the classic ladder: every line `classify_line` didn't recognize falls through to
    /// here — the authorization gate, then `run_command` (or a paused confirmation). Split out of
    /// `eval_line_inner`'s match for its size, not for any reuse.
    ///
    /// Authorization gate: enforce the leading command's `authorization-policy` (README). A
    /// `confirm`/`sudo-only` command that isn't pre-authorized surfaces a confirmation pause
    /// (reusing the `prompt-user` mechanism) and defers the command until approved. In every path
    /// the command actually run is the line with any leading `sudo` token stripped — `sudo` is a
    /// clank authorization marker, not a real executable to dispatch to Brush.
    ///
    /// Resolution consults the static registry AND the dynamic MCP manifests (an installed server
    /// name resolves to its Confirm-policy manifest — MCP tool calls are outbound HTTP). It also
    /// covers EVERY top-level command segment of a compound line (`echo ok && rm -rf /x` gates on
    /// `rm`, not the harmless leading `echo`) and returns the strictest segment's tuple plus the
    /// full list of gated commands.
    async fn dispatch_via_authz_gate(
        &mut self,
        mut plugin: Option<&mut dyn Plugin>,
        line: &str,
        pid: Option<u32>,
    ) -> LineResult {
        let (policy, elevated, command, gated) =
            self.resolve_authz_strictest(plugin.as_deref(), line, self.authz.allow_all);
        let effective = strip_sudo_prefix(line);
        let decision = authz::decide(policy, elevated, self.authz.allow_all);
        // ops.log: a `sudo-only` command is the destructive tier (rm / overwrite). Log the attempt with
        // its authorization outcome — recorded even when denied, so a blocked destructive op still
        // shows. Fires whenever the STRICTEST segment is sudo-only (any destructive command in the
        // line), so a destructive op buried behind an operator is audited too.
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
                // More than one gated command ⇒ name them all in the prompt (approving runs the whole
                // line). A single gated command uses the existing per-command synopsis text.
                let multi_summary =
                    (gated.len() > 1).then(|| authz::gated_commands_summary(&gated));
                return self.surface_auth_confirm(
                    plugin.as_deref(),
                    command.as_deref(),
                    effective,
                    pid,
                    sudo_grant,
                    None,
                    multi_summary,
                );
            }
        }

        // `blanket_authorized` = whether an `ask` dispatched from this line runs its tool calls with
        // blanket confirm-tier authorization. Only a literal `sudo ask` (elevated here) or a
        // session-wide "all" grant qualifies — approving a bare `ask` later does NOT (see
        // `resolve_auth_confirm`, which passes `false`).
        let blanket = elevated || self.authz.allow_all;
        let result = self
            .run_command(plugin.as_deref_mut(), &effective, pid, blanket)
            .await;
        self.transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_output(&result.terminal_output());
        // Tell the plug-in the line's output has been recorded — recording may just have evicted old
        // entries to stay under the cap, which is clank's cue to auto-compact them.
        if let Some(p) = plugin {
            p.after_record(&mut SessionCtx::new(self)).await;
        }
        result
    }

    /// Classify `line` into a [`LineRoute`] — the pure "what kind of line is this" decision, run
    /// after the guard clauses in `eval_line_inner`. See [`LineRoute`]'s doc for why the order below
    /// can't change.
    fn classify_line(&self, plugin: Option<&dyn Plugin>, line: &str) -> LineRoute {
        // Syntactically incomplete input must not reach Brush's fatal-parse path (which would end
        // the whole session over an unterminated heredoc/quote/substitution). Checked first so no
        // classifier below ever sees a half-construct.
        if self.line_is_incomplete(line) {
            return LineRoute::IncompleteInput;
        }
        // `<cmd> --help` for a clank-intercepted command: these commands never reach Brush's
        // dispatch, so they'd otherwise ignore `--help`. Checked first among the interceptions so no
        // intercepted command's own handling swallows it: `context --help` would otherwise be an
        // "unknown subcommand", `prompt-user --help` would be parsed as a prompt, `curl --help`
        // would surface an outbound-HTTP confirmation. Brush's own builtins (cat/grep) answer
        // `--help` through their `get_content`; not here.
        if let Some(help) = typecmd::help_for(line, &self.registry, plugin_intercepted(plugin)) {
            return LineRoute::Help(help);
        }
        // The plug-in's first look, at the rung its `context summarize` check used to occupy: after
        // core `--help`, before the generic `context` dispatch below.
        if let Some(action) =
            plugin.and_then(|p| p.classify_line(line, crate::plugin::LinePhase::BeforeContext))
        {
            return LineRoute::Plugin(action);
        }
        // `context show`/`clear`/`trim`.
        if let Some(bytes) = dispatch_context(
            &mut self
                .transcript
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            line,
        ) {
            return LineRoute::ContextDispatch(bytes);
        }
        // `prompt-user` is intercepted before Brush dispatch (like `context` above).
        if promptuser::is_prompt_user(line) {
            return LineRoute::PromptUser;
        }
        // `export --secret NAME=VALUE` (README "Sensitive environment variables"). Intercepted
        // before Brush so clank owns the secret table + std::env write and the value never enters
        // any rendered surface. Only a plain top-level line is handled; a `--secret` export carrying
        // operators (`&&`, `|`, `;`, `$()`) is REFUSED rather than silently falling through to
        // Brush — Brush's export would set the value with no redaction, which is exactly the surface
        // the flag exists to prevent (observed live: `export --secret K=v && echo set` concluded the
        // feature was inert).
        if crate::builtins::secretenv::is_secret_export(line) {
            return if is_plain_line(line) {
                LineRoute::SecretExport
            } else {
                LineRoute::SecretExportRefused
            };
        }
        // `type` for clank's intercepted commands (`prompt-user`/`curl`/`wget`/`context`): Brush's
        // own `type` can't see them (they aren't Brush builtins), so clank answers for lines that
        // query ONLY intercepted names, matching Brush's wording. Any other `type` line (a
        // Brush-known name, a mix, an unrecognized flag) returns `None` here and falls through to
        // Brush's `type` unchanged. Read-only meta command — resolved before the authz gate.
        if let Some(tools) = &self.tools {
            let words = crate::agent_tools::words(line);
            if words.first().is_some_and(|s| s == "type")
                && words.len() > 1
                && words[1..]
                    .iter()
                    .all(|name| tools.definitions.contains_key(name))
            {
                let stdout = words[1..]
                    .iter()
                    .map(|name| {
                        if tools.shadowed.contains(name) {
                            format!("{name} is a compiled command; its bound tool is shadowed\n")
                        } else {
                            format!("{name} is a bound agent tool (shell builtin)\n")
                        }
                    })
                    .collect::<String>();
                return LineRoute::TypeDispatch(stdout, 0);
            }
        }
        if let Some((stdout, exit_code)) =
            typecmd::dispatch(line, &self.registry, plugin_intercepted(plugin))
        {
            return LineRoute::TypeDispatch(stdout, exit_code);
        }
        // The plug-in's second look, at the rung its MCP-help / package-help / `ask repl` / ask-pipe
        // checks used to occupy: after `type`, as the last thing before the authorization gate (so
        // a plug-in's help never confirms, and its own gating runs instead of the core one).
        if let Some(action) =
            plugin.and_then(|p| p.classify_line(line, crate::plugin::LinePhase::BeforeGate))
        {
            return LineRoute::Plugin(action);
        }
        LineRoute::Brush
    }

    /// Run an authorized command line and reap its process row (`R → Z`). The shared execution choke
    /// point, reached both by `eval_line` (a directly-allowed command) and by `answer_prompt` (an
    /// approved gated command). Does not record the transcript — the caller decides.
    ///
    /// `curl`/`wget` are dispatched here to their async HTTP crates, NOT through `execute`. This is
    /// load-bearing: `execute` runs Brush on clank's nested `rt.block_on`, and a WASI-HTTP future
    /// polled by that tokio runtime never gets woken — nothing there performs the component-model
    /// wait (the "Wall C" shape). Awaiting `wcurl::run`/`waget::run` directly here keeps the HTTP
    /// one level under the SDK's own executor. Both call paths funnel through here, so the direct-allow and
    /// post-approval-deferred routes both reach the HTTP correctly. See `httpcmd`.
    async fn run_command(
        &mut self,
        mut plugin: Option<&mut dyn Plugin>,
        line: &str,
        pid: Option<u32>,
        blanket_authorized: bool,
    ) -> LineResult {
        // Confirmed commands can arrive from caller-carried continuation state.
        if self.stateless {
            if let Err(error) = stateless::validate(line) {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("bash: {error}\n").into_bytes(),
                    2,
                );
            }
        }
        let route = self.classify_command(plugin.as_deref(), line);
        let result = match route {
            // `kill` is Session-owned (it mutates the job table + proc table + pending state) and
            // MUST be tick-free: driving the runtime here could first-poll another parked
            // background job and wedge the invocation on its synchronous body.
            CommandRoute::Kill(Ok(args)) => self.run_kill(plugin.as_deref_mut(), &args),
            CommandRoute::Kill(Err(e)) => {
                LineResult::from_outcome(Vec::new(), format!("kill: {e}\n").into_bytes(), 2)
            }
            // A line the plug-in claimed: it runs it itself, with a `SessionCtx` for the shell
            // surface and itself in hand, so a plug-in command reached from inside it still routes.
            CommandRoute::Plugin(route) => match plugin {
                Some(p) => {
                    p.run(
                        route,
                        line,
                        pid,
                        blanket_authorized,
                        &mut SessionCtx::new(self),
                    )
                    .await
                }
                None => {
                    LineResult::stderr("clank: internal error: plug-in route with no plug-in\n")
                }
            },
            // A curl/wget-HEADED pipeline: the head's HTTP runs here at the Session layer (Wall C),
            // and the downstream runs through Brush with the response bytes as stdin. Reached only
            // from run_command, i.e. AFTER the line's authorization resolved (the gate reads the
            // pipeline's leading word, so `curl … | jq` confirmed as curl; `sudo` pre-authorized) —
            // which is also why this lives here and not in eval_line: the post-approval path
            // re-runs the raw line through run_command, and an eval_line-only intercept would miss
            // it.
            CommandRoute::HttpPipe(pipe) => self.run_http_pipe(pipe).await,
            CommandRoute::HttpDirect(crate::builtins::http::HttpCommand::Curl, args) => {
                let o = wcurl::run(&args).await;
                log_http_tool("curl", &args, o.exit_code);
                LineResult::from_outcome(o.stdout, o.stderr, o.exit_code)
            }
            CommandRoute::HttpDirect(crate::builtins::http::HttpCommand::Wget, args) => {
                let o = waget::run(&args).await;
                log_http_tool("wget", &args, o.exit_code);
                LineResult::from_outcome(o.stdout, o.stderr, o.exit_code)
            }
            CommandRoute::Execute => {
                let mut result = self.execute(line).await;
                self.adopt_new_jobs(pid, &mut result);
                result
            }
        };
        if let Some(pid) = pid {
            self.proc_table
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .complete(pid);
        }
        result
    }

    /// Classify `line` into a [`CommandRoute`] — the pure "what kind of line is this" decision at
    /// the heart of `run_command`'s dispatch, run only after `eval_line_inner`'s authorization gate
    /// has already resolved. Same behavioural-order contract as [`LineRoute`]/`classify_line`: this
    /// is tested in the exact order the historical ladder always has, and reordering it changes
    /// what the shell does (e.g. a curl/wget-headed PIPELINE must be recognized before a bare
    /// curl/wget invocation, since the pipeline form is also a superset-ish shape).
    // Kept a method for symmetry with `classify_line`, which does read `Session` state — the two are
    // one ladder and read as one. Nothing left in this body needs `Session` now that the family
    // checks belong to the plug-in.
    #[allow(clippy::unused_self)]
    fn classify_command(&self, plugin: Option<&dyn Plugin>, line: &str) -> CommandRoute {
        if let Some(parsed) = crate::builtins::kill::classify(line) {
            return CommandRoute::Kill(parsed);
        }
        // The plug-in's look, at the rung its eleven family checks used to occupy. (`context
        // summarize` moves from before `kill` to after it; no line parses as both.)
        if let Some(route) = plugin.and_then(|p| p.classify_command(line)) {
            return CommandRoute::Plugin(route);
        }
        if let Some(pipe) = crate::builtins::http::split_http_head(line) {
            return CommandRoute::HttpPipe(pipe);
        }
        match crate::builtins::http::classify(line) {
            Some((cmd, args)) => CommandRoute::HttpDirect(cmd, args),
            None => CommandRoute::Execute,
        }
    }

    /// Run a curl/wget-headed pipeline (`curl … | rest…`): the head's HTTP at the Session layer
    /// (Wall C — the SDK's executor must be the running one), then the downstream program
    /// through Brush with the response bytes as its stdin. POSIX pipe semantics: the downstream
    /// always runs, fed whatever the head produced (possibly nothing); the line's exit code is the
    /// downstream's; both stages' stderr concatenate in order.
    async fn run_http_pipe(&mut self, pipe: crate::builtins::http::HttpHeadPipe) -> LineResult {
        // wcurl and waget each have their own Outcome type; flatten to a shared triple.
        let (name, head_stdout, head_stderr, head_exit) = match pipe.cmd {
            crate::builtins::http::HttpCommand::Curl => {
                let o = wcurl::run(&pipe.args).await;
                ("curl", o.stdout, o.stderr, o.exit_code)
            }
            crate::builtins::http::HttpCommand::Wget => {
                let o = waget::run(&pipe.args).await;
                ("wget", o.stdout, o.stderr, o.exit_code)
            }
        };
        log_http_tool(name, &pipe.args, head_exit);
        let mut result = self
            .execute_with_stdin(&pipe.downstream, &head_stdout)
            .await;
        if !head_stderr.is_empty() {
            let mut stderr = head_stderr;
            stderr.extend_from_slice(&result.stderr);
            result.stderr = stderr;
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
                self.proc_table
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .complete(bg.pid);
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
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .spawn_bg(crate::runtime::proctable::ProcessKind::Builtin, argv, ppid);
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
    fn run_kill(
        &mut self,
        mut plugin: Option<&mut dyn Plugin>,
        args: &crate::builtins::kill::KillArgs,
    ) -> LineResult {
        use crate::builtins::kill::Target;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut any_missed = false;

        for target in &args.targets {
            let job_id = match target {
                Target::Job(spec) => {
                    if let Some(id) = self.shell.jobs_mut().resolve_job_spec(spec).map(|j| j.id) {
                        id
                    } else {
                        stderr.extend_from_slice(format!("kill: {spec}: no such job\n").as_bytes());
                        any_missed = true;
                        continue;
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
                    // A pending (triggered/scheduled) agent invocation: the plug-in cancels it.
                    let cancelled = plugin
                        .as_deref_mut()
                        .and_then(|p| p.cancel(*pid, &mut SessionCtx::new(self)));
                    if let Some(message) = cancelled {
                        stdout.extend_from_slice(message.as_bytes());
                        continue;
                    }
                    if let Some(bg) = self.bg_jobs.iter().find(|b| b.pid == *pid) {
                        bg.job_id
                    } else {
                        stderr.extend_from_slice(
                            format!("kill: ({pid}) - No such process\n").as_bytes(),
                        );
                        any_missed = true;
                        continue;
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
                self.proc_table
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .complete(killed_pid);
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

    /// Resolve a line's authorization policy, consulting the static registry AND — for a leading
    /// command the registry doesn't recognize — the installed plug-in's own run-time manifest for
    /// that name, if it supplies one. Mirrors [`authz::resolve`] but adds that plug-in layer: the
    /// shell core has no notion of which family (if any) installed a given command, only that a
    /// non-core command's line still needs the right policy, and the plug-in is the one place that
    /// can supply it. Returns `(policy, elevated, command)`.
    fn resolve_authz(
        &self,
        plugin: Option<&dyn Plugin>,
        line: &str,
    ) -> (crate::manifest::AuthorizationPolicy, bool, Option<String>) {
        let (command, elevated) = authz::leading_command(line);
        if let Some(name) = command.as_deref() {
            if let Some(tools) = &self.tools {
                let mut words = crate::agent_tools::words(line);
                if words.first().is_some_and(|s| s == "sudo") {
                    words.remove(0);
                }
                if let Some(policy) =
                    tools.policy(name, words.get(1..).unwrap_or_default(), |key| {
                        self.shell
                            .env()
                            .get(key)
                            .map(|v| v.1.value().to_cow_str(&self.shell).to_string())
                    })
                {
                    return (policy, elevated, command);
                }
            }
            if self.registry.get(name).is_none() {
                // Not a core command: defer to whatever manifest the installed plug-in supplies for
                // it (e.g. a command whose invocation is an outbound network/LLM call still needs to
                // confirm, even though the shell core can't see why).
                if let Some(m) = plugin.and_then(|p| p.authz_manifest(name)) {
                    return (m.authorization_policy, elevated, command);
                }
            }
        }
        authz::resolve(&self.registry, line)
    }

    /// Resolve authorization for a whole line by its STRICTEST top-level command segment (README
    /// "Authorization" — a `confirm`/`sudo-only` command hidden behind `&&`/`;`/`|`/`&` after a
    /// harmless leading command must still gate). Splits the line with [`authz::split_segments`],
    /// resolves each segment with the plug-in-aware [`Self::resolve_authz`], `decide`s each against
    /// `allow_all`, and returns the most-restrictive segment's `(policy, elevated, command)` — so the
    /// caller's existing `authz::decide(policy, elevated, allow_all)` reproduces the strictest
    /// `Decision` unchanged. Also returns every non-`Allow` command with its tier, so the confirmation
    /// can name all of them (approving the strictest runs the whole line).
    ///
    /// A single-segment line short-circuits to [`Self::resolve_authz`] — identical behavior, no
    /// overhead — so nothing changes for the overwhelmingly common plain command.
    pub(super) fn resolve_authz_strictest(
        &self,
        plugin: Option<&dyn Plugin>,
        line: &str,
        allow_all: bool,
    ) -> (
        crate::manifest::AuthorizationPolicy,
        bool,
        Option<String>,
        Vec<(String, crate::manifest::AuthorizationPolicy)>,
    ) {
        let segments = authz::split_segments(line);
        if segments.len() <= 1 {
            let (policy, elevated, command) = self.resolve_authz(plugin, line);
            let gated = match authz::decide(policy, elevated, allow_all) {
                Decision::Allow => Vec::new(),
                _ => command
                    .clone()
                    .map(|c| vec![(c, policy)])
                    .unwrap_or_default(),
            };
            return (policy, elevated, command, gated);
        }

        // Multi-segment: resolve+decide each, track the strictest, and collect every gated command.
        let mut strictest: Option<(
            u8,
            crate::manifest::AuthorizationPolicy,
            bool,
            Option<String>,
        )> = None;
        let mut gated: Vec<(String, crate::manifest::AuthorizationPolicy)> = Vec::new();
        for seg in segments {
            let (policy, elevated, command) = self.resolve_authz(plugin, seg);
            let decision = authz::decide(policy, elevated, allow_all);
            if !matches!(decision, Decision::Allow) {
                if let Some(name) = command.clone() {
                    gated.push((name, policy));
                }
            }
            let rank = authz::decision_rank(decision);
            if strictest.as_ref().is_none_or(|(r, ..)| rank > *r) {
                strictest = Some((rank, policy, elevated, command));
            }
        }
        // Invariant: `split_segments` never returns empty, so the loop above set `strictest` at
        // least once.
        #[allow(clippy::expect_used)]
        let (_, policy, elevated, command) = strictest.expect("split_segments never returns empty");
        (policy, elevated, command, gated)
    }

    /// Complete an intercepted line's row and record its output (for intercepted paths that don't go
    /// through `run_command`, e.g. an authorization denial).
    fn finish_intercepted(&mut self, pid: Option<u32>, result: LineResult) -> LineResult {
        if let Some(pid) = pid {
            self.proc_table
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .complete(pid);
        }
        self.transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_output(&result.terminal_output());
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
            return self
                .finish_intercepted(pid, LineResult::from_outcome(Vec::new(), Vec::new(), 0));
        };

        // Mark it exported in Brush's variable table so `$NAME` expands in scripts and it's part of
        // the exported set. `set_global` replaces any prior value (whole-value → replay-safe).
        let mut var = brush_core::variables::ShellVariable::new(secret.value.clone());
        var.export();
        if let Err(e) = self.shell.env_mut().set_global(&secret.name, var) {
            let msg = format!("export: {e}\n");
            return self.finish_intercepted(
                pid,
                LineResult::from_outcome(Vec::new(), msg.into_bytes(), 1),
            );
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
    #[must_use]
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
    #[must_use]
    pub fn line_is_incomplete(&self, line: &str) -> bool {
        match self.shell.parse_string(line) {
            Err(brush_parser::ParseError::Tokenizing { ref inner, .. })
                if inner.is_incomplete() =>
            {
                true
            }
            Err(brush_parser::ParseError::ParsingAtEndOfInput) => true,
            _ => false,
        }
    }

    /// Native execution: capture Brush's stdout and stderr into anonymous temp files.
    #[cfg(not(target_arch = "wasm32"))]
    async fn execute(&mut self, line: &str) -> LineResult {
        self.execute_impl(line, None).await
    }

    /// Like [`execute`](Self::execute), but with `stdin_bytes` presented as fd 0 to the whole
    /// program — how a Session-layer pipeline head (curl/wget, see `run_http_pipe`) hands its
    /// output to a Brush-run downstream.
    #[cfg(not(target_arch = "wasm32"))]
    async fn execute_with_stdin(&mut self, line: &str, stdin_bytes: &[u8]) -> LineResult {
        self.execute_impl(line, Some(stdin_bytes)).await
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn execute_impl(&mut self, line: &str, stdin_bytes: Option<&[u8]>) -> LineResult {
        use std::io::{Read, Seek, SeekFrom, Write};
        let _tools = self.tools.as_ref().map(|t| {
            crate::agent_tools::install(t.clone(), line, |key| {
                self.shell
                    .env()
                    .get(key)
                    .map(|v| v.1.value().to_cow_str(&self.shell).to_string())
            })
        });

        let stdout_capture = match tempfile::tempfile() {
            Ok(f) => f,
            Err(e) => return LineResult::stderr(format!("clank: {e}\n")),
        };
        let stderr_capture = match tempfile::tempfile() {
            Ok(f) => f,
            Err(e) => return LineResult::stderr(format!("clank: {e}\n")),
        };
        let (Ok(out_fd), Ok(err_fd)) = (stdout_capture.try_clone(), stderr_capture.try_clone())
        else {
            return LineResult::stderr(b"clank: failed to set up output capture\n".to_vec());
        };

        let mut params = self.shell.default_exec_params();
        params.set_fd(OpenFiles::STDOUT_FD, OpenFile::File(out_fd.into()));
        params.set_fd(OpenFiles::STDERR_FD, OpenFile::File(err_fd.into()));
        if let Some(bytes) = stdin_bytes {
            // Stage the bytes in an unlinked temp file and hand it to Brush as fd 0.
            let mut staged = match tempfile::tempfile() {
                Ok(f) => f,
                Err(e) => return LineResult::stderr(format!("clank: {e}\n")),
            };
            if staged.write_all(bytes).is_err() || staged.seek(SeekFrom::Start(0)).is_err() {
                return LineResult::stderr(b"clank: failed to stage pipeline stdin\n".to_vec());
            }
            params.set_fd(OpenFiles::STDIN_FD, OpenFile::File(staged.into()));
        }

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
        self.execute_impl(line, None).await
    }

    /// Like [`execute`](Self::execute), but with `stdin_bytes` presented as fd 0 to the whole
    /// program — how a Session-layer pipeline head (curl/wget, see `run_http_pipe`) hands its
    /// output to a Brush-run downstream.
    #[cfg(target_arch = "wasm32")]
    async fn execute_with_stdin(&mut self, line: &str, stdin_bytes: &[u8]) -> LineResult {
        self.execute_impl(line, Some(stdin_bytes)).await
    }

    #[cfg(target_arch = "wasm32")]
    // Drives Brush inline (block_on at the call boundary); the mirror native `execute_impl` awaits, so
    // this stays async for parity even though its wasm body doesn't `.await`.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn execute_impl(&mut self, line: &str, stdin_bytes: Option<&[u8]>) -> LineResult {
        let _tools = self.tools.as_ref().map(|t| {
            crate::agent_tools::install(t.clone(), line, |key| {
                self.shell
                    .env()
                    .get(key)
                    .map(|v| v.1.value().to_cow_str(&self.shell).to_string())
            })
        });
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
        if let Some(bytes) = stdin_bytes {
            params.set_fd(
                OpenFiles::STDIN_FD,
                OpenFile::Stream(Box::new(BufSource(std::io::Cursor::new(bytes.to_vec())))),
            );
        }

        let fut = self
            .shell
            .run_string(line.to_string(), &self.source, &params);
        let result = self.rt.block_on(fut);
        drop(params);

        let stdout = std::mem::take(
            &mut *stdout_buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let stderr = std::mem::take(
            &mut *stderr_buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        finish(result, stdout, stderr)
    }
}

/// Classify a command line into a process kind for the process table. Everything is a `Builtin`
/// this increment; this is where Script/Prompt/AgentInvocation classification lands once those
/// command kinds exist (they'll be resolved from `$PATH` / the registry).
fn classify(_line: &str) -> ProcessKind {
    ProcessKind::Builtin
}

/// The command names the installed plug-in intercepts (empty with no plug-in) — the set `type` and
/// `--help` own on top of [`typecmd::CORE_INTERCEPTED`].
fn plugin_intercepted(plugin: Option<&dyn Plugin>) -> &'static [&'static str] {
    plugin.map_or(&[][..], Plugin::intercepted)
}

/// A shell.log-safe rendering of `line`: strips the VALUE of an `export --secret NAME=VALUE` AND the
/// argument of any credential-bearing flag (`model add … --key <KEY>`, cluster `--token`, …) to
/// `<redacted>` (README "never written to logs"). These declaration lines are the cases the
/// `mask_values` log filter can't cover — the secret isn't in the active set until the line runs, and
/// the start/end events bracket that — so they are redacted structurally here. Every other line passes
/// through untouched (registered secret *values* that appear in later log lines are masked centrally
/// in `logging::append`).
fn log_safe_line(line: &str) -> std::borrow::Cow<'_, str> {
    // `export --secret` first (its NAME=VALUE shape), then any `--key`/`--token`-style flag argument.
    let export = crate::builtins::secretenv::redact_export_line(line);
    let base = export.as_deref().unwrap_or(line);
    match crate::builtins::secretenv::redact_secret_flag_args(base) {
        Some(flagged) => std::borrow::Cow::Owned(flagged),
        None => match export {
            Some(e) => std::borrow::Cow::Owned(e),
            None => std::borrow::Cow::Borrowed(line),
        },
    }
}

/// Strip a leading `sudo ` token from a line (the command to actually run once `sudo`-elevated
/// authorization is satisfied). Whitespace-based — sufficient for the leading-word scope of this
/// increment. If the line isn't `sudo`-prefixed, it's returned unchanged.
#[must_use]
pub fn strip_sudo_prefix(line: &str) -> String {
    let trimmed = line.trim_start();
    match trimmed.strip_prefix("sudo") {
        // Only a `sudo` token followed by whitespace (not `sudoedit`, etc.).
        Some(rest) if rest.starts_with(char::is_whitespace) => rest.trim_start().to_string(),
        _ => line.to_string(),
    }
}

/// Log a curl/wget invocation to http.log: the tool, its target URL (the first non-flag argument), and
/// the exit code. curl/wget bypass the `McpHttp` seam (their own `whttp` fetch), so they're
/// logged here at the dispatch site rather than by the `LoggingMcpHttp` decorator.
fn log_http_tool(tool: &str, args: &[String], exit_code: u8) {
    let url = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map_or("", String::as_str);
    crate::logging::Record::new("http")
        .field("tool", tool)
        .field("url", crate::logging::redact_url(url))
        .field("exit", exit_code.to_string())
        .emit(crate::logging::LogFile::Http);
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

use crate::config::vfs::HOME as DEFAULT_HOME;

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
