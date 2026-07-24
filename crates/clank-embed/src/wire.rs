//! The shell surface's wire types — the single agent-side source of truth.
//!
//! **Field order is a wire contract.** golem's value model is positional: a record's field names
//! live in the type graph, not the value, and every decoder (`#[derive(FromSchema)]` in golem-cli's
//! `agent shell`, out-of-band JSON readers like clank's golem-e2e) matches fields **by declaration
//! order**. Reordering fields here silently mis-assigns values in every consumer — it is not a
//! compile error anywhere. See clank's DEV_SDK_CHANGES.md ("the value-model break reaches the test
//! harness") for the incident that taught this.

use golem_rust::Schema;
use serde::{Deserialize, Serialize};

/// The result of one shell-surface call (`eval` / `answer_prompt` / `abort_prompt`).
///
/// Positional wire order: `stdout`, `stderr`, `exit_code`, `pending_prompt`, `cwd` — do not reorder.
/// `cwd` is LAST so it stays an *optional trailing* field: `agent shell` decodes the record
/// positionally and reads it only if present, so a consumer built against the four-field shape
/// still decodes cleanly (see golem-cli's `extract_cwd`).
#[derive(Clone, Debug, Schema, Serialize, Deserialize)]
pub struct EvalResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u8,
    /// Set when this call surfaced a `prompt-user` question the shell is now awaiting a response
    /// to. The caller must collect a human answer and deliver it via `answer_prompt` — the shell
    /// never blocks. `None` for every ordinary command.
    pub pending_prompt: Option<PendingPromptView>,
    /// The shell's working directory after this call (the agent's absolute path), so an interactive
    /// caller can show it in its prompt and reflect a `cd` live. Kept last — see the type doc.
    pub cwd: String,
}

/// The wire view of a pending `prompt-user` question surfaced to the caller.
///
/// Positional wire order: `question`, `choices` — do not reorder.
#[derive(Clone, Debug, Schema, Serialize, Deserialize)]
pub struct PendingPromptView {
    /// The question (with any piped markdown prepended) to present to the human.
    pub question: String,
    /// If present, the response must be one of these values.
    pub choices: Option<Vec<String>>,
}
