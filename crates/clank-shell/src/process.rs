//! The internal process abstraction.
//!
//! The README's process model says everything that looks like a process — builtins, scripts,
//! prompts, Golem agent invocations — is a synthetic abstraction inside the single clank component
//! (no fork/exec, no OS processes), modeled as distinct implementations of one async trait. This
//! module defines the *shape* of that trait so those kinds have a common slot to fill.
//!
//! **Scope (increment 1): shape only.** The trait is defined and one synchronous kind is described,
//! but there is no process *table*, no PIDs, and no background/paused execution — those need a
//! between-invocation execution model that the one-line-synchronous, no-threads wasm agent does not
//! have yet (a later increment). A [`ClankProcess`] is a *façade over Brush's existing execution*
//! (`Shell::run_string`), not a parallel task runtime: when real background work lands it will reuse
//! Brush's `ExecutionSpawnResult::StartedTask` / `jobs::JobManager` `JobTask::Internal`, not a new
//! executor.
//!
//! A process is distinct from a [`Manifest`](crate::manifest::Manifest): the manifest is static
//! metadata about a *command*; a process is a running *invocation* that references the manifest of
//! the command it runs.

use brush_core::ExecutionResult;

/// The kind of thing a process is an invocation of. Maps to the README's process types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessKind {
    /// A shell builtin or core command (uutils/text-lib), or brush-language execution.
    Builtin,
    /// An installed shell script on `$PATH`. (Not yet implemented.)
    Script,
    /// A prompt executed via `ask`. (Not yet implemented.)
    Prompt,
    /// A method invocation on a remote Golem agent. (Not yet implemented.)
    AgentInvocation,
}

/// A synthetic unit of execution inside the clank shell — the common shape for every process kind.
///
/// `async fn run` is the single execution entry point. On wasm today it collapses to the existing
/// synchronous path (Brush driven under a nested `block_on`); the `async` signature exists so that
/// future kinds which genuinely suspend (agent invocations awaiting a remote reply, prompts awaiting
/// a model) fit the same trait without reshaping it.
///
/// Only [`ProcessKind::Builtin`] has a real implementation in this increment; the other kinds are
/// declared for the process table to slot into later.
#[allow(async_fn_in_trait)] // internal trait, not part of a public object-safe API surface
pub trait ClankProcess: Send {
    /// What kind of process this is (for `ps` / the process table's type tag).
    fn kind(&self) -> ProcessKind;

    /// The command line that started this process, as argv (for `/proc/<pid>/cmdline`, `ps`).
    fn cmdline(&self) -> &[String];

    /// Run the process to completion and yield its exit result.
    ///
    /// This is a façade over Brush's execution, not a new runtime — see the module docs.
    async fn run(&mut self) -> ExecutionResult;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_kind_is_copy_and_comparable() {
        let k = ProcessKind::Builtin;
        assert_eq!(k, ProcessKind::Builtin);
        assert_ne!(k, ProcessKind::Script);
        // Copy semantics.
        let _c = k;
        assert_eq!(k, ProcessKind::Builtin);
    }
}
