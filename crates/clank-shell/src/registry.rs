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

use crate::manifest::Manifest;

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
            .map(|(name, _reg)| name)
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
