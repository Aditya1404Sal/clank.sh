//! Shell-native builtins + command classifiers that aren't a utility or a capability subsystem: the
//! `context` transcript command ([`context`]), [`promptuser`] (human-in-the-loop), `kill` ([`kill`]),
//! `export --secret` ([`secretenv`]), curl/wget dispatch ([`http`]), `type` resolution ([`typecmd`] —
//! `type` is a Rust keyword), and the nested-context honest-error stubs ([`interceptstub`]).

pub(crate) mod context;
pub(crate) mod http;
pub(crate) mod interceptstub;
pub(crate) mod kill;
pub mod promptuser;
pub mod secretenv;
pub mod typecmd;
