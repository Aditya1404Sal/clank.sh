//! The internal process abstraction.
//!
//! The README's process model says everything that looks like a process — builtins, scripts,
//! prompts, Golem agent invocations — is a synthetic abstraction inside the single clank component
//! (no fork/exec, no OS processes), modeled as distinct implementations of one async trait. This
//! module defines the *shape* of that trait so those kinds have a common slot to fill.
//!
//! The live process *table* + PIDs + R/S/T/Z/P states are in [`proctable`](crate::runtime::proctable); the
//! `ProcessKind` tag below is what those rows carry. The [`ClankProcess`] trait is the original
//! execution-shape sketch and has no implementors — the running path drives Brush directly (a façade
//! over `Shell::run_string`, not a parallel task runtime) and records rows in `proctable`.
//!
//! A process is distinct from a [`Manifest`](crate::manifest::Manifest): the manifest is static
//! metadata about a *command*; a process is a running *invocation* of it.

use brush_core::ExecutionResult;

/// The type tag a process-table row carries. `classify` currently tags command lines as `Builtin`;
/// `AgentInvocation` is set on remote agent-invocation rows (see session.rs). `Script`/`Prompt` are
/// declared for future proc-table tagging (their features run through the `run_command` ladder today).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessKind {
    /// A shell builtin or core command (uutils/text-lib), or brush-language execution.
    Builtin,
    /// An installed shell script on `$PATH`.
    Script,
    /// A prompt executed via `ask`.
    Prompt,
    /// A method invocation on a remote Golem agent.
    AgentInvocation,
}

/// A synthetic unit of execution inside the clank shell — the common shape for every process kind. The
/// `async fn run` signature exists so kinds that genuinely suspend (agent invocations, model calls) fit
/// without reshaping the trait. Currently unimplemented — the live path drives Brush directly (see the
/// module docs); kept as the design shape.
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
