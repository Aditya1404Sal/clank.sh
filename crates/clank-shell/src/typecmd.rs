//! `type` — clank owns the authoritative command resolver (for the commands Brush can't see).
//!
//! The README makes `type` the authoritative resolver for *all* commands so the AI can discover
//! every capability natively. Brush's own `type` only reads Brush's tables (its builtins, aliases,
//! functions, and `$PATH`), so it never sees clank's **intercepted** commands — `prompt-user`,
//! `curl`, `wget`, `context` — which are handled in [`crate::session::Session::eval_line`]/
//! `run_command` *before* Brush dispatch and are not registered as Brush builtins. `type curl` under
//! plain Brush therefore reports "not found", contradicting the README.
//!
//! This module closes that gap **without re-implementing Brush's `type`**. It short-circuits only the
//! handful of names Brush would miss (the [`INTERCEPTED`] set) and otherwise returns `None` so
//! `eval_line` falls through to Brush unchanged (which handles `cat`/`grep`, aliases, functions,
//! `$PATH`, and every `type` flag). The clank path fires only when **every** queried name is
//! intercepted — any Brush-known or mixed line defers wholesale to Brush (a documented scope cut;
//! single-name lookups, the common case and what tool packaging uses, are fully correct).

use brush_parser::{tokenize_str, unquote_str, Token};

use crate::registry::CommandRegistry;

/// The commands clank intercepts before Brush dispatch — precisely the names in the registry that
/// are **not** registered as Brush builtins, so Brush's `type`/`--help` never see them. clank's
/// `type` and `--help` handling own exactly this set; everything else defers to Brush.
pub const INTERCEPTED: &[&str] =
    &["prompt-user", "curl", "wget", "context", "ask", "kill", "mcp", "grease", "golem"];

/// Whether `name` is a clank-intercepted command (one Brush can't resolve).
pub fn is_intercepted(name: &str) -> bool {
    INTERCEPTED.contains(&name)
}

/// The dequoted `Word` tokens of `line` (quote-aware via Brush's tokenizer; operators dropped).
/// `None` if the line doesn't tokenize — it falls through to Brush, which reports its own error.
/// Mirrors `httpcmd::leading_words`.
fn words(line: &str) -> Option<Vec<String>> {
    let tokens = tokenize_str(line).ok()?;
    let words: Vec<String> = tokens
        .into_iter()
        .filter_map(|t| match t {
            Token::Word(s, _) => Some(unquote_str(&s)),
            Token::Operator(_, _) => None,
        })
        .collect();
    (!words.is_empty()).then_some(words)
}

/// A parsed `type` invocation: the `-t` flag and the queried names.
struct TypeInvocation {
    /// `-t`: print only the word `builtin`/`file`/… rather than the full description.
    type_only: bool,
    /// The command names being queried (in order).
    names: Vec<String>,
}

/// Parse a `type` line into its `-t` flag and queried names, or `None` if the leading word isn't
/// `type`. Only `-t` is recognized among flags; any other leading dash is ignored (left in `names`
/// won't happen because a real other-flag line falls through to Brush anyway — see [`dispatch`]).
fn parse(line: &str) -> Option<TypeInvocation> {
    let words = words(line)?;
    let (first, rest) = words.split_first()?;
    if first != "type" {
        return None;
    }
    let mut type_only = false;
    let mut names = Vec::new();
    let mut saw_unknown_flag = false;
    for w in rest {
        if w == "-t" {
            type_only = true;
        } else if w.starts_with('-') && w != "-" {
            // An unrecognized flag (`-a`, `-p`, `-P`, `-f`, …): clank doesn't model it. Mark it so
            // `dispatch` defers the whole line to Brush.
            saw_unknown_flag = true;
        } else {
            names.push(w.clone());
        }
    }
    if saw_unknown_flag {
        return None;
    }
    Some(TypeInvocation { type_only, names })
}

/// If `line` is a `type` invocation that queries **only** clank-intercepted commands, produce the
/// `type` output clank owns (each name → `"<name> is a shell builtin"`, or bare `"builtin"` under
/// `-t`) and exit 0. Returns `None` in every other case — no `type` line, an unrecognized flag, no
/// names, or **any** queried name Brush could resolve (or that isn't intercepted) — so the caller
/// falls through to Brush's `type`, which handles the full flag/name surface unchanged.
///
/// Output shape matches Brush's `type` wording exactly (`"<name> is a shell builtin"`; `-t` →
/// `"builtin"`) so the two resolvers are indistinguishable to a caller. Returns `(stdout, exit_code)`.
pub fn dispatch(line: &str, registry: &CommandRegistry) -> Option<(String, u8)> {
    let inv = parse(line)?;
    // No names (`type` alone / `type -t`) → let Brush handle its own usage/error.
    if inv.names.is_empty() {
        return None;
    }
    // Fire only when EVERY queried name is a clank-intercepted command Brush can't see. Any
    // Brush-known or non-intercepted name → defer the whole line to Brush (documented scope cut).
    if !inv.names.iter().all(|n| is_intercepted(n)) {
        return None;
    }
    // Every name is intercepted; each has a registry manifest (INTERCEPTED ⊆ registry). Report it.
    let mut out = String::new();
    for name in &inv.names {
        debug_assert!(
            registry.contains(name),
            "intercepted command '{name}' missing from the registry"
        );
        if inv.type_only {
            out.push_str("builtin\n");
        } else {
            out.push_str(&format!("{name} is a shell builtin\n"));
        }
    }
    Some((out, 0))
}

/// If `line` is `<cmd> --help` (or `<cmd> ... --help`) where `<cmd>` is a clank-intercepted command,
/// return that command's manifest help text (with a trailing newline). `None` otherwise — including
/// for Brush's own builtins, which answer `--help` through their `get_content`.
///
/// This is the one place clank serves `--help` for the intercepted commands (`prompt-user`, `curl`,
/// `wget`, `context`), which never reach Brush's dispatch and so would otherwise ignore `--help`.
pub fn help_for(line: &str, registry: &CommandRegistry) -> Option<String> {
    let words = words(line)?;
    let (first, rest) = words.split_first()?;
    if !is_intercepted(first) {
        return None;
    }
    if !rest.iter().any(|w| w == "--help") {
        return None;
    }
    let manifest = registry.get(first)?;
    Some(format!("{}\n", manifest.help_text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> CommandRegistry {
        crate::registry::build()
    }

    #[test]
    fn dispatches_each_intercepted_command_as_shell_builtin() {
        for name in INTERCEPTED {
            let (out, code) = dispatch(&format!("type {name}"), &reg()).unwrap();
            assert_eq!(out, format!("{name} is a shell builtin\n"));
            assert_eq!(code, 0);
        }
    }

    #[test]
    fn type_only_prints_bare_builtin() {
        let (out, code) = dispatch("type -t curl", &reg()).unwrap();
        assert_eq!(out, "builtin\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn all_intercepted_names_report_each() {
        let (out, _) = dispatch("type curl wget prompt-user context", &reg()).unwrap();
        assert_eq!(
            out,
            "curl is a shell builtin\n\
             wget is a shell builtin\n\
             prompt-user is a shell builtin\n\
             context is a shell builtin\n"
        );
    }

    #[test]
    fn brush_known_name_falls_through() {
        // `cat` is a Brush-registered builtin — clank must NOT handle it (defer to Brush).
        assert!(dispatch("type cat", &reg()).is_none());
        // A name Brush would find on $PATH / not-found is also Brush's job.
        assert!(dispatch("type nonexistent-xyz", &reg()).is_none());
    }

    #[test]
    fn mixed_line_falls_through() {
        // Any Brush-known name in the mix → defer the WHOLE line to Brush (scope cut).
        assert!(dispatch("type curl cat", &reg()).is_none());
        assert!(dispatch("type cat curl", &reg()).is_none());
    }

    #[test]
    fn unrecognized_flag_falls_through() {
        // `-a`/`-p`/`-P`/`-f` are Brush's to interpret.
        assert!(dispatch("type -a curl", &reg()).is_none());
        assert!(dispatch("type -p curl", &reg()).is_none());
    }

    #[test]
    fn bare_type_and_non_type_lines_are_none() {
        assert!(dispatch("type", &reg()).is_none());
        assert!(dispatch("type -t", &reg()).is_none());
        assert!(dispatch("echo hi", &reg()).is_none());
        assert!(dispatch("", &reg()).is_none());
    }

    #[test]
    fn help_for_intercepted_returns_manifest_help() {
        let help = help_for("curl --help", &reg()).unwrap();
        assert!(help.contains("fetch a URL over"));
        assert!(help.ends_with('\n'));

        let help = help_for("prompt-user --help", &reg()).unwrap();
        assert!(help.contains("pause the"));

        let help = help_for("context --help", &reg()).unwrap();
        assert!(help.contains("session transcript"));
    }

    #[test]
    fn help_for_non_intercepted_or_no_flag_is_none() {
        // Brush builtin → its own --help handles it.
        assert!(help_for("cat --help", &reg()).is_none());
        // Intercepted but no --help.
        assert!(help_for("curl https://example.com", &reg()).is_none());
        // Not a command at all.
        assert!(help_for("", &reg()).is_none());
    }
}
