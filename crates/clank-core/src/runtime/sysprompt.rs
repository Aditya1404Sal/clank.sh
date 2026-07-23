//! The live system-prompt slot: the fully-rendered agentic system prompt for the current line.
//!
//! `/proc/clank/system-prompt` is meant to show *what the model actually sees* — the command surface
//! plus installed MCP tools, grease prompts, and skills. But [`crate::runtime::procfs`]'s resolver is reached
//! from a synchronous Brush builtin (`cat`) with no access to the live `Session` (its `registry`/`mcp`/
//! `grease`). So the `Session` renders the full prompt once per `run_line` (via
//! [`crate::ai::ask::build_system_prompt_with_capabilities`] — the same call `run_ask` makes) and
//! installs the string here; the procfs resolver reads it. Mirrors the per-line thread-local install
//! pattern of [`crate::runtime::proctable`] / [`crate::runtime::mcpfs`] / [`crate::runtime::dynreg`] exactly.
//!
//! When nothing is installed (native off-session reads, tests), the resolver falls back to the static
//! base prompt — so `cat /proc/clank/system-prompt` is always answerable.

use std::cell::RefCell;
use std::sync::Arc;

thread_local! {
    /// The pre-rendered system prompt for the line executing on this thread. Populated by [`install`]
    /// for the duration of one `run_line` and read by `procfs` via [`active`]. Thread-local so parallel
    /// Sessions (native tests) don't collide.
    static ACTIVE: RefCell<Option<Arc<String>>> = const { RefCell::new(None) };
}

/// Install `prompt` as the active system prompt for the current line; the guard restores the previous
/// slot on drop.
#[must_use]
pub fn install(prompt: Arc<String>) -> InstallGuard {
    let previous = ACTIVE.with(|slot| slot.borrow_mut().replace(prompt));
    InstallGuard { previous }
}

/// The active system prompt, if a line is executing on this thread.
#[must_use]
pub fn active() -> Option<Arc<String>> {
    ACTIVE.with(|slot| slot.borrow().clone())
}

/// Restores the previous active-prompt slot when dropped.
pub struct InstallGuard {
    previous: Option<Arc<String>>,
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        ACTIVE.with(|slot| *slot.borrow_mut() = previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_and_active_round_trip_with_restore() {
        assert!(active().is_none());
        {
            let _g = install(Arc::new("outer".to_string()));
            assert_eq!(active().as_deref().map(String::as_str), Some("outer"));
            {
                let _g2 = install(Arc::new("inner".to_string()));
                assert_eq!(active().as_deref().map(String::as_str), Some("inner"));
            }
            // Inner guard dropped → outer restored.
            assert_eq!(active().as_deref().map(String::as_str), Some("outer"));
        }
        assert!(active().is_none());
    }
}
