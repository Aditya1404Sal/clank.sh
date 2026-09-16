//! Shell-native builtins + command classifiers that aren't a utility or a capability subsystem: the
//! `context` transcript command ([`context`]), [`promptuser`] (human-in-the-loop), `kill` ([`kill`]),
//! `export --secret` ([`secretenv`]), curl/wget dispatch ([`http`]), `type` resolution ([`typecmd`] —
//! `type` is a Rust keyword), and the nested-context honest-error stubs ([`interceptstub`]).
//!
//! The `--help` shim for hand-rolled `SimpleCommand`s (generic Brush registration plumbing, not a
//! builtin) lives at the crate top level as `crate::helpshim`, not here — it's consumed from four
//! other concern directories (`tools`, `runtime`, `ai`) besides this one.

pub(crate) mod context;
pub(crate) mod http;
// `pub` for [`interceptstub::session_stub`]: a plug-in crate registers the same honest-error stub
// for its own session-layer commands, and it now lives on the other side of a crate boundary.
pub mod interceptstub;
pub(crate) mod kill;
pub mod probe;
pub mod promptuser;
pub mod secretenv;
pub mod typecmd;
