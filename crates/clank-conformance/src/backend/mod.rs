//! The two shell targets behind one synchronous trait.
//!
//! `eval`/`answer` return `Err` only for INFRASTRUCTURE failures (session construction,
//! invoke timeout, undecodable CLI output) — assertion mismatches are the matcher's job,
//! so a red trial always says which of the two it was.

pub mod golem;
pub mod native;

/// Which of the two conformance targets a trial runs against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// The in-process native [`Session`] target.
    Native,
    /// The deployed golem agent target.
    Golem,
}

impl BackendKind {
    /// The lowercase tier name (`"native"` / `"golem"`) used in reports and paths.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            BackendKind::Native => "native",
            BackendKind::Golem => "golem",
        }
    }
}

/// One step's observable result, identical across targets. Streams are lossy UTF-8 —
/// the same mapping the agent's wire type applies.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// The captured standard-output stream (lossy UTF-8).
    pub stdout: String,
    /// The captured standard-error stream (lossy UTF-8).
    pub stderr: String,
    /// The command's exit status.
    pub exit_code: u8,
    /// The pending prompt the step left open, if any.
    pub pending: Option<PendingView>,
}

/// The pending-prompt surface both targets share (`secret` is native-side only and
/// deliberately outside the conformance contract).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingView {
    /// The prompt's question text.
    pub question: String,
    /// The prompt's fixed choice list, if it offers one.
    pub choices: Option<Vec<String>>,
}

/// A conformance target that runs shell steps and resolves prompts.
pub trait ShellBackend {
    /// Run one command line (the `run` step).
    ///
    /// # Errors
    /// Returns `Err` on an infrastructure failure (session construction, invoke timeout,
    /// or undecodable CLI output), never on an assertion mismatch.
    fn eval(&mut self, line: &str) -> anyhow::Result<Outcome>;
    /// Resolve a pending prompt: `Some(text)` answers, `None` aborts.
    ///
    /// # Errors
    /// Returns `Err` on an infrastructure failure (see [`ShellBackend::eval`]).
    fn answer(&mut self, response: Option<&str>) -> anyhow::Result<Outcome>;
    /// The `${TMP}` substitution value — the scenario's sandbox directory.
    fn tmp(&self) -> &str;
    /// Tear down (remove the native sandbox, restore process env). Failures here fail
    /// the trial: a leaky scenario is a bug.
    ///
    /// # Errors
    /// Returns `Err` if teardown fails — e.g. the native sandbox cannot be removed.
    fn finish(self: Box<Self>) -> anyhow::Result<()>;
}
