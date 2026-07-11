//! A per-line thread-local slot for **dynamic** command manifests (installed MCP servers) so
//! Brush-builtin surfaces that only see the static [`binfs::registry()`](crate::binfs::registry) —
//! notably `man` — can also resolve runtime-registered names.
//!
//! Mirrors [`proctable::install`](crate::proctable::install): the `Session` installs the current MCP
//! manifests for the duration of one line (an RAII guard clears the slot on drop), and `man` consults
//! [`active`] after the static registry. Thread-local so parallel Sessions (native tests) don't
//! collide; on the single-threaded agent every stage sees the slot.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use crate::manifest::Manifest;

thread_local! {
    static ACTIVE: RefCell<Option<Arc<Mutex<Vec<Manifest>>>>> = const { RefCell::new(None) };
}

/// Install `manifests` as the active dynamic registry for the current line. Returns an RAII guard that
/// restores the previous slot value on drop.
#[must_use]
pub fn install(manifests: Arc<Mutex<Vec<Manifest>>>) -> InstallGuard {
    let previous = ACTIVE.with(|slot| slot.borrow_mut().replace(manifests));
    InstallGuard { previous }
}

/// The dynamic manifest for `name`, if a line is executing and the name is a dynamic (MCP) command.
pub fn lookup(name: &str) -> Option<Manifest> {
    ACTIVE.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|arc| arc.lock().unwrap().iter().find(|m| m.name == name).cloned())
    })
}

/// Restores the previous slot when dropped.
pub struct InstallGuard {
    previous: Option<Arc<Mutex<Vec<Manifest>>>>,
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        ACTIVE.with(|slot| *slot.borrow_mut() = previous);
    }
}
