//! The command-line utilities: uutils-backed coreutils ([`coreutils`]), the hand-rolled text tools
//! ([`texttools`]: grep/jq/sed/awk/diff/patch/file, with [`awk`] its own engine), and the standalone
//! [`find`], [`stat`], [`xargs`], [`man`], and [`which`] builtins.

pub(crate) mod awk;
pub(crate) mod coreutils;
pub(crate) mod find;
pub(crate) mod man;
pub(crate) mod stat;
pub(crate) mod texttools;
pub(crate) mod which;
pub(crate) mod xargs;
