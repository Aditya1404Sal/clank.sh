//! The command registry: clank's inventory of every command it registers, keyed by name, each
//! carrying a [`Manifest`].
//!
//! This is a clank-owned side table that sits *beside* Brush's builtin `HashMap` rather than
//! replacing it — clank cannot change Brush's `Registration` struct (a pinned dependency), so the
//! typed manifest fields (`execution_scope`, `authorization_policy`, input schema) live here and
//! every future surface (`type`, `ps`, `ask` tool packaging, authorization) reads them by name.
//!
//! **Drift invariant:** each clank builtin must have exactly one manifest and vice versa. The two
//! lists are authored in the same modules ([`crate::coreutils`], [`crate::texttools`]) — a builtin
//! registration next to its manifest — and a test here cross-checks that the registry's names match
//! the builtins actually registered, mirroring the README's "a package that cannot provide a
//! manifest is rejected at install time."

use std::collections::HashMap;

use crate::manifest::{AuthorizationPolicy, ExecutionScope, Manifest};

/// Manifest names present in the registry that are **not** registered via clank's
/// `.builtins(...)` calls in `session::build_shell` — the drift-guard test below allowlists these
/// explicitly rather than requiring every manifest to map to a clank-authored `SimpleCommand`.
/// Each entry's reason:
///
/// - `prompt-user` — intercepted in `Session::eval_line` *before* Brush dispatch (its suspend
///   logic can't run inside a synchronous Brush builtin); it never reaches `shell.run_string`, so
///   it has no `SimpleCommand`/`.builtins()` registration to match against. See `promptuser`.
/// - `type`, `command` — Brush's own `BashMode` builtins (registered by `.default_builtins`, not by
///   clank). They work as-is (pure, wasm-safe path/builtin lookup); clank just adds their manifests
///   so the resolution/tool surfaces see them. This is the "later increment" the module doc predicted.
/// - `curl`, `wget` — HTTP commands intercepted in `Session::run_command` (their async HTTP can't run
///   inside a synchronous Brush builtin under the nested runtime); not `.builtins()`-registered. See
///   `httpcmd`.
/// - `context` — the clank-specific transcript builtin, intercepted in `Session::eval_line` via
///   `dispatch_context` *before* Brush dispatch (it operates on the session transcript, which a Brush
///   builtin can't reach); not `.builtins()`-registered. See `dispatch_context`.
/// - `ask` — the AI-native LLM command, intercepted in `Session::run_command` (its LLM call must run at
///   the Session layer where the Golem durable context is live, not inside a synchronous Brush builtin
///   under the nested runtime); not `.builtins()`-registered. See `askcmd`.
/// - `kill` — the synthetic kill, intercepted in `Session::run_command` (it mutates Session state:
///   the bg-job mapping, proc table, and pending prompt; Brush's own kill is nix/unix-gated). See
///   `killcmd`.
pub const MANUAL_MANIFESTS: &[&str] =
    &["prompt-user", "type", "command", "curl", "wget", "context", "ask", "kill"];

/// Hand-authored manifests for commands not backed by a clank `SimpleCommand` registration (see
/// [`MANUAL_MANIFESTS`]).
fn manual_manifests() -> Vec<Manifest> {
    vec![
        Manifest::builtin("prompt-user", "pause and collect input from the human user")
            .with_scope(ExecutionScope::ShellInternal)
            .with_policy(AuthorizationPolicy::Allow)
            .with_help(
                "prompt-user <question> [--choices a,b,...] [--confirm] [--secret] — pause the \
                 current process, present <question> to the human user, and return the response. \
                 Accepts markdown on stdin, rendered before the question. Exits 0 on response, \
                 130 on abort.",
            ),
        // Brush-native BashMode builtins — manifest only (they already dispatch through Brush). The
        // README makes `type` the authoritative resolver for all commands and groups it with the
        // shell-internal builtins.
        Manifest::builtin("type", "describe how a command name would be resolved")
            .with_scope(ExecutionScope::ShellInternal),
        Manifest::builtin("command", "run a command, bypassing functions/aliases; or look it up")
            .with_scope(ExecutionScope::ShellInternal),
        // `context` — the clank transcript builtin, intercepted in `eval_line`. Shell-internal (it
        // operates on the session transcript); `Allow` (reading/managing one's own transcript needs
        // no confirmation).
        Manifest::builtin("context", "show, clear, or budget the session transcript")
            .with_scope(ExecutionScope::ShellInternal)
            .with_help(
                "context show — print the session transcript\n\
                 context clear — discard the session transcript\n\
                 context budget [n] — show or set the transcript token budget",
            ),
    ]
}

/// clank's inventory of manifests, keyed by command name.
#[derive(Clone, Debug, Default)]
pub struct CommandRegistry {
    by_name: HashMap<String, Manifest>,
}

impl CommandRegistry {
    /// Look up a command's manifest by name.
    pub fn get(&self, name: &str) -> Option<&Manifest> {
        self.by_name.get(name)
    }

    /// Whether a command with this name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Iterate over all registered manifests.
    pub fn iter(&self) -> impl Iterator<Item = &Manifest> {
        self.by_name.values()
    }

    /// The set of registered command names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// Number of registered commands.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Insert a manifest, panicking on a duplicate name — two commands with the same name is a
    /// programming error (the same name can't resolve to two manifests).
    fn insert(&mut self, manifest: Manifest) {
        let name = manifest.name.clone();
        if self.by_name.insert(name.clone(), manifest).is_some() {
            panic!("duplicate manifest for command '{name}' in the clank registry");
        }
    }
}

/// Build the registry from the manifests authored alongside clank's builtins.
///
/// Only clank-authored builtins are covered. Brush's BashMode builtins (`cd`, `export`, `echo`,
/// `type`, …) are not clank code and get manifests in a later increment (they need the
/// special-builtin / `parent-shell` + `shell-internal` classification work).
pub fn build() -> CommandRegistry {
    let mut registry = CommandRegistry::default();
    for manifest in crate::coreutils::manifests() {
        registry.insert(manifest);
    }
    for manifest in crate::texttools::manifests() {
        registry.insert(manifest);
    }
    for manifest in crate::ps::manifests() {
        registry.insert(manifest);
    }
    for manifest in crate::which::manifests() {
        registry.insert(manifest);
    }
    for manifest in crate::mancmd::manifests() {
        registry.insert(manifest);
    }
    for manifest in crate::statcmd::manifests() {
        registry.insert(manifest);
    }
    for manifest in crate::findcmd::manifests() {
        registry.insert(manifest);
    }
    for manifest in crate::xargscmd::manifests() {
        registry.insert(manifest);
    }
    for manifest in crate::httpcmd::manifests() {
        registry.insert(manifest);
    }
    for manifest in crate::askcmd::manifests() {
        registry.insert(manifest);
    }
    for manifest in crate::killcmd::manifests() {
        registry.insert(manifest);
    }
    for manifest in manual_manifests() {
        registry.insert(manifest);
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is non-empty and every manifest is retrievable by its own name.
    #[test]
    fn build_indexes_every_manifest_by_name() {
        let registry = build();
        assert!(!registry.is_empty());
        for m in registry.iter() {
            assert_eq!(registry.get(&m.name).map(|g| &g.name), Some(&m.name));
        }
    }

    /// Drift guard: the registry's names are exactly the names of the builtins clank registers on
    /// the shell. If a builtin is added/removed without its manifest (or vice versa), this fails.
    #[test]
    fn registry_names_match_registered_builtins() {
        use std::collections::BTreeSet;

        // The names clank actually registers on the Brush shell, from the same producers
        // `build_shell` uses. Uses the DefaultShellExtensions so the generic `builtins()` resolves.
        type SE = brush_core::extensions::DefaultShellExtensions;
        let builtin_names: BTreeSet<String> = crate::coreutils::builtins::<SE>()
            .into_iter()
            .chain(crate::texttools::builtins::<SE>())
            .chain(crate::ps::builtins::<SE>())
            .chain(crate::which::builtins::<SE>())
            .chain(crate::mancmd::builtins::<SE>())
            .chain(crate::statcmd::builtins::<SE>())
            .chain(crate::findcmd::builtins::<SE>())
            .chain(crate::xargscmd::builtins::<SE>())
            .map(|(name, _reg)| name)
            // MANUAL_MANIFESTS covers commands with a manifest but no `.builtins()` registration
            // (see its doc comment for why each entry is legitimate) — union them into the
            // expected set rather than requiring a `SimpleCommand` for every manifest.
            .chain(MANUAL_MANIFESTS.iter().map(|s| s.to_string()))
            .collect();

        let registry = build();
        let manifest_names: BTreeSet<String> = registry.names().map(String::from).collect();

        assert_eq!(
            manifest_names, builtin_names,
            "clank builtins and their manifests have drifted: \
             every registered builtin must have exactly one manifest and vice versa"
        );
    }
}
