//! `grease`, the package manager: registry payload types + integrity ([`pkg`]), the installed-package
//! [`state`], on-disk store/paths ([`config`]), and the `grease` command grammar ([`cmd`]).
//!
//! The payload types and their sha256/ed25519/RFC-6962-transparency-log integrity machinery live in
//! the standalone [`grease_pkg`] crate — re-exported here as [`pkg`] (plus [`Error`]/[`error`]) so
//! every existing `crate::grease::pkg::…`/`crate::grease::Error` path in this crate keeps working
//! unchanged. `grease-pkg` is dependency-free of `clank-core` (that's the point: `dev-tools/grease-tool`
//! depends on it directly instead of on all of clank-core), so it cannot know about this crate's
//! manifest types — see [`param_specs_of`] for the one adapter that gap requires.

pub(crate) mod cmd;
pub mod config;
pub mod state;

pub use grease_pkg as pkg;
pub use grease_pkg::error;
pub use grease_pkg::Error;

/// Adapts a grease package's declared `{{var}}` arguments ([`grease_pkg::PackageArg`]) into the
/// manifest's [`ParamSpec`](crate::manifest::ParamSpec) shape (drives `--help`/completion for prompt
/// and script packages). All params come out `String`-typed (free text); `required`/`default` carry
/// through unchanged.
///
/// This is a package → manifest adapter, and it lives here rather than as a method on
/// `grease_pkg::PromptPackage`/`ScriptPackage` (which is where it used to live, before the
/// `grease-pkg` extraction) because `ParamSpec`/`ParamType` are shared manifest types —
/// [`crate::mcp::state`] builds them too, for MCP tool schemas — so they belong in `crate::manifest`,
/// not folded into the leaf `grease-pkg` crate. Moving the adapter into `grease-pkg` instead would
/// make that crate depend on `crate::manifest`, i.e. on clank-core — inverting the exact dependency
/// direction the extraction exists to cut. Rust also has no inherent-impl escape hatch here (both
/// `PackageArg` and `ParamSpec` are foreign to whichever crate doesn't define them), so a free
/// function on the clank-core side is the natural seam.
#[must_use]
pub fn param_specs_of(args: &[grease_pkg::PackageArg]) -> Vec<crate::manifest::ParamSpec> {
    args.iter()
        .map(|a| crate::manifest::ParamSpec {
            name: a.name.clone(),
            ty: crate::manifest::ParamType::String,
            required: a.required,
            default: a.default.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_specs_of_maps_arguments() {
        let args = vec![
            grease_pkg::PackageArg {
                name: "a".into(),
                description: String::new(),
                required: true,
                default: None,
            },
            grease_pkg::PackageArg {
                name: "b".into(),
                description: String::new(),
                required: false,
                default: Some("z".into()),
            },
        ];
        let specs = param_specs_of(&args);
        assert_eq!(specs.len(), 2);
        let a = specs.iter().find(|s| s.name == "a").unwrap();
        assert!(a.required && matches!(a.ty, crate::manifest::ParamType::String));
        let b = specs.iter().find(|s| s.name == "b").unwrap();
        assert!(!b.required && b.default.as_deref() == Some("z"));
    }
}
