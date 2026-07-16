//! The two shell targets behind one synchronous trait.
//!
//! `eval`/`answer` return `Err` only for INFRASTRUCTURE failures (session construction,
//! invoke timeout, undecodable CLI output) — assertion mismatches are the matcher's job,
//! so a red trial always says which of the two it was.

pub mod golem;
pub mod native;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Native,
    Golem,
}

impl BackendKind {
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
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u8,
    pub pending: Option<PendingView>,
}

/// The pending-prompt surface both targets share (`secret` is native-side only and
/// deliberately outside the conformance contract).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingView {
    pub question: String,
    pub choices: Option<Vec<String>>,
}

pub trait ShellBackend {
    /// Run one command line (the `run` step).
    fn eval(&mut self, line: &str) -> anyhow::Result<Outcome>;
    /// Resolve a pending prompt: `Some(text)` answers, `None` aborts.
    fn answer(&mut self, response: Option<&str>) -> anyhow::Result<Outcome>;
    /// The `${TMP}` substitution value — the scenario's sandbox directory.
    fn tmp(&self) -> &str;
    /// Tear down (remove the native sandbox, restore process env). Failures here fail
    /// the trial: a leaky scenario is a bug.
    fn finish(self: Box<Self>) -> anyhow::Result<()>;
}
