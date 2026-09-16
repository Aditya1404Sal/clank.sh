//! [`SessionCtx`]: the part of a [`Session`] a plug-in may reach while it runs — command execution,
//! the transcript, the process table, authorization and the registry. A plug-in never holds the
//! `Session` itself, which is what lets the shell core move to its own crate without the families.

use super::{LineResult, Session};
use crate::manifest::Manifest;
use crate::registry::CommandRegistry;
use crate::runtime::proctable::{AgentMeta, ProcessKind, ProcessTable};

/// A plug-in's handle on the shell for the duration of one call.
pub struct SessionCtx<'a> {
    session: &'a mut Session,
}

impl<'a> SessionCtx<'a> {
    #[allow(dead_code, reason = "used from Task 4 onward")]
    pub(crate) fn new(session: &'a mut Session) -> Self {
        Self { session }
    }

    /// Run `line` through Brush and capture its output (no authorization, no plug-in routing).
    pub async fn execute(&mut self, line: &str) -> LineResult {
        self.session.execute(line).await
    }

    /// [`execute`](Self::execute) with `stdin` presented as fd 0 to the whole program.
    pub async fn execute_with_stdin(&mut self, line: &str, stdin: &[u8]) -> LineResult {
        self.session.execute_with_stdin(line, stdin).await
    }

    /// Complete `pid`'s row and record `result` in the transcript.
    pub fn finish(&mut self, pid: Option<u32>, result: LineResult) -> LineResult {
        self.session.finish_intercepted(pid, result)
    }

    /// Append output to the transcript.
    pub fn record_output(&mut self, bytes: &[u8]) {
        self.with_transcript(|t| t.record_output(bytes));
    }

    /// Run `f` with the transcript locked. Never hold the lock across an await: render first.
    pub fn with_transcript<R>(&self, f: impl FnOnce(&mut crate::Transcript) -> R) -> R {
        f(&mut self
            .session
            .transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    fn with_proc<R>(&self, f: impl FnOnce(&mut ProcessTable) -> R) -> R {
        f(&mut self
            .session
            .proc_table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// Mark `pid` finished (`→ Z`).
    pub fn proc_complete(&mut self, pid: u32) {
        self.with_proc(|t| t.complete(pid));
    }

    /// Mark `pid` paused for a human (`→ P`).
    pub fn proc_pause(&mut self, pid: u32) {
        self.with_proc(|t| t.pause(pid));
    }

    /// Resume a paused `pid` (`P → R`).
    pub fn proc_resume(&mut self, pid: u32) {
        self.with_proc(|t| t.resume(pid));
    }

    /// Spawn a background (`S`) row and return its PID.
    pub fn proc_spawn_bg(&mut self, kind: ProcessKind, argv: Vec<String>, ppid: u32) -> u32 {
        self.with_proc(|t| t.spawn_bg(kind, argv, ppid))
    }

    /// Attach Golem agent identity to `pid`'s row (renders in `/proc/<pid>/status`).
    pub fn proc_set_agent_meta(&mut self, pid: u32, meta: AgentMeta) {
        self.with_proc(|t| t.set_agent_meta(pid, meta));
    }

    /// Whether the session-wide "all" confirmation grant is set.
    #[must_use]
    pub fn allow_all(&self) -> bool {
        self.session.authz.allow_all
    }

    /// Set or clear the session-wide "all" confirmation grant.
    pub fn set_allow_all(&mut self, allow: bool) {
        self.session.authz.allow_all = allow;
    }

    /// The static manifest for `name`, if the registry has one.
    #[must_use]
    pub fn manifest(&self, name: &str) -> Option<&Manifest> {
        self.session.registry.get(name)
    }

    /// The whole command registry.
    #[must_use]
    pub fn registry(&self) -> &CommandRegistry {
        &self.session.registry
    }

    /// The terminal width, or `None` when non-interactive.
    #[must_use]
    pub fn columns(&self) -> Option<usize> {
        self.session.columns()
    }

    /// The shell's `$HOME`.
    #[must_use]
    pub fn home(&self) -> String {
        self.session.shell_home()
    }
}
