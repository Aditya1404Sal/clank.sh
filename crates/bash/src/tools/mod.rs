//! The command-line utilities: uutils-backed coreutils ([`coreutils`]), the hand-rolled text tools
//! ([`texttools`]: grep/jq/sed/awk/diff/patch/file, with [`awk`] its own engine), and the standalone
//! [`find`], [`stat`], [`xargs`], [`man`], and [`which`] builtins.
//!
//! Public because the tools are part of the shell core's surface now that the command families live
//! in another crate: a plug-in renders its own listings through the same helper `ls` does
//! ([`coreutils::format_columns`]). The items inside stay crate-private unless a consumer names one,
//! so making the modules public exports nothing new on its own.

pub mod awk;
pub mod coreutils;
pub mod find;
pub mod man;
pub mod stat;
pub mod texttools;
pub mod which;
pub mod xargs;
