//! Forensics for a panic that clank cannot catch.
//!
//! **Why a hook and not `catch_unwind`.** `wasm32-wasip2` is an `abort` panic target — verified,
//! not assumed: `rustc -Z unstable-options --print target-spec-json --target wasm32-wasip2` reports
//! `"panic-strategy": "abort"`. There is no unwinding on the agent, so a `catch_unwind` around the
//! component's exports would be dead code: the panic aborts the instance before any landing pad
//! runs, Golem retries the deterministic trap, and the worker parks in `Failed`.
//!
//! A panic **hook**, though, runs *before* the abort. That is the whole opportunity here: clank
//! cannot survive the panic, but it can say where it died and what it was doing — which is the
//! difference between "the agent is wedged, no idea why" and a `file:line` plus the offending
//! command line. On native (an unwind target) the hook runs too, and the REPL additionally survives.
//!
//! The report goes to `ops.log` through the installed [`crate::logging::LogSink`], so on the agent
//! it takes the replay-safe whole-file-rewrite path rather than a raw append.

use std::cell::RefCell;
use std::sync::Once;

thread_local! {
    /// The command line currently executing on this thread, so a panic report can name it.
    /// Thread-local, matching [`crate::runtime::sysprompt`] and friends, so parallel native
    /// Sessions in tests don't collide.
    static CURRENT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Record `line` as the command executing on this thread; the guard clears it on drop.
#[must_use]
pub fn executing(line: &str) -> ExecutingGuard {
    let previous = CURRENT.with(|slot| slot.borrow_mut().replace(line.to_string()));
    ExecutingGuard { previous }
}

/// Restores the previous command slot when dropped.
pub struct ExecutingGuard {
    previous: Option<String>,
}

impl Drop for ExecutingGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        // `try_borrow_mut`: a panic while the slot is borrowed would otherwise panic again here
        // during unwind, and a panic-in-drop-during-panic aborts.
        CURRENT.with(|slot| {
            if let Ok(mut s) = slot.try_borrow_mut() {
                *s = previous;
            }
        });
    }
}

/// The command executing on this thread, if any.
#[must_use]
pub fn current() -> Option<String> {
    CURRENT.with(|slot| slot.try_borrow().ok().and_then(|s| s.clone()))
}

/// Install the panic reporter. Idempotent — safe to call on every invocation.
///
/// Chains to the previous hook so the default message still reaches stderr on native.
pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Everything in here must be panic-free: a panic inside the hook aborts immediately and
            // destroys the report. `logging::append` uses `try_borrow` for the same reason.
            let location = info
                .location()
                .map_or_else(|| "unknown".to_string(), ToString::to_string);
            let mut record = crate::logging::Record::new("panic").field("at", &location);
            if let Some(line) = current() {
                record = record.field("line", &line);
            }
            record = record.field("message", payload_of(info));
            crate::logging::append(crate::logging::LogFile::Ops, &record.render());
            previous(info);
        }));
    });
}

/// Best-effort extraction of a panic payload as text.
fn payload_of(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_command_is_scoped_and_restored() {
        assert!(current().is_none());
        {
            let _outer = executing("echo outer");
            assert_eq!(current().as_deref(), Some("echo outer"));
            {
                let _inner = executing("echo inner");
                assert_eq!(current().as_deref(), Some("echo inner"));
            }
            assert_eq!(current().as_deref(), Some("echo outer"));
        }
        assert!(current().is_none());
    }

    #[test]
    fn installing_is_idempotent() {
        // Called on every agent invocation, so it must not stack hooks.
        install();
        install();
        install();
    }
}
