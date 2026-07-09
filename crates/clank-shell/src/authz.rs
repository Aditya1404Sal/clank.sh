//! Authorization enforcement: gating command execution by the `authorization-policy` recorded on
//! each command's [`Manifest`](crate::manifest::Manifest) (README "Authorization").
//!
//! The three policies (`allow` / `confirm` / `sudo-only`) are enforced in
//! [`Session::eval_line`](crate::session::Session::eval_line), *before* a command reaches Brush —
//! `ShellExtensions` offers no dispatch hook, so the clank layer is the enforcement point. A
//! `confirm`/`sudo-only` command that isn't pre-authorized surfaces the *same* pending-prompt pause
//! `prompt-user` uses (a yes/no/all confirmation), rather than a second mechanism; the gated command
//! runs only once the human approves.
//!
//! **Scope (this increment): the leading command per line only.** No `;`/`&&`/`||`/`|` splitting —
//! a compound line is gated on its first command word alone. Reliable splitting needs real
//! quote/subshell awareness and is future work.
//!
//! **`sudo` is human-authorization intent, not Unix credentials** (README: single user, no
//! `/etc/sudoers`, no uid 0). A leading `sudo` token marks the invocation *elevated* — pre-authorized
//! for `confirm` and `sudo-only` — and is stripped before the command is resolved and run.

use brush_parser::{tokenize_str, unquote_str, Token};

use crate::manifest::AuthorizationPolicy;
use crate::registry::CommandRegistry;

/// Session-scoped authorization state.
#[derive(Debug, Default)]
pub struct AuthzState {
    /// Set once the human answers "all" to a `confirm` prompt: subsequent `confirm` commands this
    /// session proceed without re-asking. Does **not** satisfy `sudo-only` (that tier always needs
    /// its own explicit grant). Matches the README's flat "(a)ll" option, not per-command.
    pub allow_all: bool,
}

/// The leading command of `line` and whether it is `sudo`-elevated. A leading `sudo` token is
/// stripped and the *next* word becomes the command (`sudo ask "..."` → command `ask`, elevated).
/// Returns `(None, elevated)` if the line has no command word (e.g. only `sudo`, or empty).
///
/// Quote-aware (via Brush's tokenizer) and pipe/operator-aware: only leading `Word` tokens are
/// considered, so `sudo` before an operator isn't mistaken for elevation of a later stage.
pub fn leading_command(line: &str) -> (Option<String>, bool) {
    let Ok(tokens) = tokenize_str(line) else {
        return (None, false);
    };
    let mut words = tokens.into_iter().filter_map(|t| match t {
        Token::Word(s, _) => Some(unquote_str(&s)),
        Token::Operator(_, _) => None,
    });

    match words.next() {
        Some(first) if first == "sudo" => (words.next(), true),
        other => (other, false),
    }
}

/// The authorization decision for a resolved command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Run the command immediately.
    Allow,
    /// Surface a confirmation prompt; run the command only on approval. `sudo_grant` is true when
    /// this is a `sudo-only` gate (so "all" from a prior `confirm` does not auto-satisfy it, and the
    /// prompt copy frames it as a sudo request).
    Confirm { sudo_grant: bool },
    /// Refuse outright (no prompt). Currently unused — every gate offers a confirmation — but kept
    /// for a future policy that denies without asking.
    Deny,
}

/// Decide how to handle a command with the given `policy`, given whether the invocation is
/// `elevated` (a `sudo` prefix) and the session's `allow_all` grant.
pub fn decide(policy: AuthorizationPolicy, elevated: bool, allow_all: bool) -> Decision {
    match policy {
        AuthorizationPolicy::Allow => Decision::Allow,
        AuthorizationPolicy::Confirm => {
            if elevated || allow_all {
                Decision::Allow
            } else {
                Decision::Confirm { sudo_grant: false }
            }
        }
        AuthorizationPolicy::SudoOnly => {
            if elevated {
                Decision::Allow
            } else {
                // A prior "all" grant does NOT satisfy sudo-only; each needs its own approval.
                Decision::Confirm { sudo_grant: true }
            }
        }
    }
}

/// Resolve the effective policy for the leading command of `line`, plus whether the line is
/// `sudo`-elevated and the command word. Commands with no manifest default to [`Allow`] (Brush's own
/// builtins — `cd`, `export`, … — aren't in the clank registry yet).
pub fn resolve(registry: &CommandRegistry, line: &str) -> (AuthorizationPolicy, bool, Option<String>) {
    let (command, elevated) = leading_command(line);
    let policy = command
        .as_deref()
        .and_then(|name| registry.get(name))
        .map(|m| m.authorization_policy)
        .unwrap_or(AuthorizationPolicy::Allow);
    (policy, elevated, command)
}

/// The confirmation prompt text for a gated command, matching the README's example phrasing
/// (`"<cmd> has requested permission to <synopsis>. (y)es, (n)o, (a)ll"`).
pub fn confirm_question(command: &str, synopsis: &str, sudo_grant: bool) -> String {
    if sudo_grant {
        format!("{command} requires sudo authorization to {synopsis}. (y)es, (n)o")
    } else {
        format!("{command} has requested permission to {synopsis}. (y)es, (n)o, (a)ll")
    }
}

/// The `--choices` a confirmation prompt offers: `yes,no,all` for a `confirm` gate, `yes,no` for a
/// `sudo-only` gate (no blanket "all" for the strongest tier).
pub fn confirm_choices(sudo_grant: bool) -> Vec<String> {
    if sudo_grant {
        vec!["yes".to_string(), "no".to_string()]
    } else {
        vec!["yes".to_string(), "no".to_string(), "all".to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_command_extracts_first_word() {
        assert_eq!(leading_command("rm -rf /tmp/x"), (Some("rm".to_string()), false));
        assert_eq!(leading_command("echo hi"), (Some("echo".to_string()), false));
    }

    #[test]
    fn leading_command_detects_and_strips_sudo() {
        assert_eq!(
            leading_command(r#"sudo ask "clean up""#),
            (Some("ask".to_string()), true)
        );
        // Bare `sudo` with nothing after → elevated but no command.
        assert_eq!(leading_command("sudo"), (None, true));
    }

    #[test]
    fn leading_command_is_quote_aware() {
        // A quoted first word is dequoted.
        assert_eq!(leading_command(r#""rm" x"#), (Some("rm".to_string()), false));
    }

    #[test]
    fn allow_policy_always_allows() {
        assert_eq!(decide(AuthorizationPolicy::Allow, false, false), Decision::Allow);
        assert_eq!(decide(AuthorizationPolicy::Allow, true, true), Decision::Allow);
    }

    #[test]
    fn confirm_is_satisfied_by_sudo_or_all() {
        assert_eq!(
            decide(AuthorizationPolicy::Confirm, false, false),
            Decision::Confirm { sudo_grant: false }
        );
        assert_eq!(decide(AuthorizationPolicy::Confirm, true, false), Decision::Allow);
        assert_eq!(decide(AuthorizationPolicy::Confirm, false, true), Decision::Allow);
    }

    #[test]
    fn sudo_only_needs_elevation_not_all() {
        assert_eq!(decide(AuthorizationPolicy::SudoOnly, true, false), Decision::Allow);
        // "all" from a prior confirm does NOT satisfy sudo-only.
        assert_eq!(
            decide(AuthorizationPolicy::SudoOnly, false, true),
            Decision::Confirm { sudo_grant: true }
        );
    }

    #[test]
    fn confirm_choices_differ_by_tier() {
        assert_eq!(confirm_choices(false), vec!["yes", "no", "all"]);
        assert_eq!(confirm_choices(true), vec!["yes", "no"]);
    }
}
