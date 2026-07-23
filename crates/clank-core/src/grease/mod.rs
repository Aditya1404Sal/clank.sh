//! `grease`, the package manager: registry payload types + integrity ([`pkg`]), the installed-package
//! [`state`], on-disk store/paths ([`config`]), and the `grease` command grammar ([`cmd`]).

pub(crate) mod cmd;
pub mod config;
pub mod pkg;
pub mod state;
