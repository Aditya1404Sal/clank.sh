//! The synthetic process + virtual-filesystem substrate the rest of the shell rides on:
//! the process model ([`process`]) and table ([`proctable`], `ps` at [`ps`]), the `/proc` virtual fs
//! ([`procfs`]), the `/bin` namespace ([`binfs`]), the `/mnt/mcp` resource fs ([`mcpfs`]), the dynamic
//! command-manifest registration slot ([`dynreg`]), and the live `/proc/clank/system-prompt` provider
//! ([`sysprompt`]).

pub mod binfs;
pub mod dynreg;
pub mod mcpfs;
pub mod process;
pub mod procfs;
pub mod proctable;
pub(crate) mod ps;
pub mod sysprompt;
