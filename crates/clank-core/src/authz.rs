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
//! **Scope: every top-level command in a line.** The line is split on the control operators
//! `;` / `&&` / `||` / `|` / `&` / newline (quote- and subshell-aware, via Brush's tokenizer — see
//! [`split_segments`]) and each segment is gated on its own policy; the line is gated on the
//! STRICTEST segment ([`decision_rank`]). A subshell body `(rm x)` is covered (its leading word still
//! resolves). A command nested inside a command substitution `$(...)` or backticks is part of a single
//! word token and is NOT yet split out — recursive substitution parsing is future work, so such a
//! command is currently gated only by the policy of the word that contains it.
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
#[must_use]
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
#[must_use]
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
///
/// **Subcommand-aware:** if the resolved manifest has subcommands and the line's *second* word names
/// one, that subcommand's policy wins (so `mcp list` is `Allow` while `mcp add` is `Confirm`). This
/// keeps a coarse top-level policy from over-gating read-only subcommands.
#[must_use]
pub fn resolve(registry: &CommandRegistry, line: &str) -> (AuthorizationPolicy, bool, Option<String>) {
    let words = command_words(line);
    let elevated = words.first().is_some_and(|w| w == "sudo");
    let rest = if elevated { &words[1..] } else { &words[..] };
    let command = rest.first().cloned();
    let subword = rest.get(1);

    let policy = command
        .as_deref()
        .and_then(|name| registry.get(name))
        .map_or(AuthorizationPolicy::Allow, |m| {
            // Prefer a matching subcommand's policy, else the command's own.
            subword
                .and_then(|sub| m.subcommands.iter().find(|s| &s.name == sub))
                .map_or(m.authorization_policy, |s| s.authorization_policy)
        });
    (policy, elevated, command)
}

/// The dequoted `Word` tokens of `line` (operators dropped). Shared by [`resolve`] and
/// [`leading_command`] for subcommand-aware policy resolution.
fn command_words(line: &str) -> Vec<String> {
    let Ok(tokens) = tokenize_str(line) else {
        return Vec::new();
    };
    tokens
        .into_iter()
        .filter_map(|t| match t {
            Token::Word(s, _) => Some(unquote_str(&s)),
            Token::Operator(_, _) => None,
        })
        .collect()
}

/// The control operators that separate one command from the next at the top level. A `Token::Operator`
/// whose text is one of these ends the current command segment; everything else — redirects
/// (`>`/`<`/`>>`/`2>`), subshell parens (`(`/`)`), the `;;` case terminator — stays WITHIN a segment so
/// the segment's leading command still resolves.
const SEGMENT_SEPARATORS: &[&str] = &["|", "|&", "||", "&", "&&", ";", "\n"];

/// Split `line` into its top-level command segments as byte-exact substrings, cutting at each control
/// operator (`;` `&&` `||` `|` `&` newline). Byte-exact via the tokenizer's source spans (the same
/// technique as [`crate::builtins::http::split_http_head`]), so quoting inside a segment survives and a
/// quoted operator (`echo "a && b"`) is a single `Word`, never a split point. Empty / whitespace-only
/// segments (e.g. from a trailing `;`) are dropped.
///
/// A command inside `$(...)` / backticks is part of a single `Word` token and is therefore NOT split
/// out (documented gap — see the module doc). A subshell `(rm x)` IS covered: `(` is an operator, so
/// the segment's first `Word` is still `rm`.
///
/// A line that doesn't tokenize (or contains only separators) yields the whole line as one segment, so
/// the caller always resolves *something* rather than silently skipping the gate.
#[must_use]
pub fn split_segments(line: &str) -> Vec<&str> {
    let Ok(tokens) = tokenize_str(line) else {
        return vec![line];
    };
    let mut segments = Vec::new();
    let mut seg_start = 0usize;
    for t in &tokens {
        if let Token::Operator(op, span) = t {
            if SEGMENT_SEPARATORS.contains(&op.as_str()) {
                let (start, end) = (span.start.index, span.end.index);
                if start <= line.len() && line.is_char_boundary(start) {
                    let seg = line[seg_start..start].trim();
                    if !seg.is_empty() {
                        segments.push(seg);
                    }
                }
                if end <= line.len() && line.is_char_boundary(end) {
                    seg_start = end;
                }
            }
        }
    }
    if seg_start <= line.len() {
        let seg = line[seg_start..].trim();
        if !seg.is_empty() {
            segments.push(seg);
        }
    }
    // A line of only separators (or empty) yields no segments; fall back to the whole line so the
    // caller resolves an (all-`Allow`) default rather than skipping the gate entirely.
    if segments.is_empty() {
        segments.push(line.trim());
    }
    segments
}

/// A total order on [`Decision`] strictness, for aggregating a compound line onto its most-restrictive
/// segment: `Allow` < `Confirm{sudo_grant:false}` < `Confirm{sudo_grant:true}` < `Deny`.
#[must_use]
pub fn decision_rank(d: Decision) -> u8 {
    match d {
        Decision::Allow => 0,
        Decision::Confirm { sudo_grant: false } => 1,
        Decision::Confirm { sudo_grant: true } => 2,
        Decision::Deny => 3,
    }
}

/// Render the "names every gated command with its tier" clause for a compound-line confirmation, e.g.
/// `rm [sudo-only], curl [confirm]`. Shown when a line has more than one gated command so the human
/// sees everything approving will run, not just the strictest.
#[must_use]
pub fn gated_commands_summary(gated: &[(String, AuthorizationPolicy)]) -> String {
    gated
        .iter()
        .map(|(name, policy)| {
            let tier = match policy {
                AuthorizationPolicy::SudoOnly => "sudo-only",
                AuthorizationPolicy::Confirm => "confirm",
                AuthorizationPolicy::Allow => "allow",
            };
            format!("{name} [{tier}]")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The confirmation prompt for a compound line with more than one gated command: names them all (via
/// [`gated_commands_summary`]) so approval is informed. Choices still follow the strictest tier (via
/// [`confirm_choices`]); `sudo_grant` picks the tail.
#[must_use]
pub fn confirm_question_multi(summary: &str, sudo_grant: bool) -> String {
    if sudo_grant {
        format!("this line runs {summary}; it requires sudo authorization. (y)es, (n)o")
    } else {
        format!("this line runs {summary}; approve? (y)es, (n)o, (a)ll")
    }
}

/// The confirmation prompt text for a gated command, matching the README's example phrasing
/// (`"<cmd> has requested permission to <synopsis>. (y)es, (n)o, (a)ll"`).
#[must_use]
pub fn confirm_question(command: &str, synopsis: &str, sudo_grant: bool) -> String {
    if sudo_grant {
        format!("{command} requires sudo authorization to {synopsis}. (y)es, (n)o")
    } else {
        format!("{command} has requested permission to {synopsis}. (y)es, (n)o, (a)ll")
    }
}

/// The `--choices` a confirmation prompt offers: `yes,no,all` for a `confirm` gate, `yes,no` for a
/// `sudo-only` gate (no blanket "all" for the strongest tier).
#[must_use]
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

    #[test]
    fn split_segments_splits_on_control_operators() {
        assert_eq!(split_segments("echo hi && rm -rf /x"), vec!["echo hi", "rm -rf /x"]);
        assert_eq!(split_segments("a | b | c"), vec!["a", "b", "c"]);
        assert_eq!(split_segments("a ; b ; c"), vec!["a", "b", "c"]);
        assert_eq!(split_segments("a || b"), vec!["a", "b"]);
    }

    #[test]
    fn split_segments_drops_empty_trailing_segments() {
        // A trailing `;` or `&` leaves no empty segment.
        assert_eq!(split_segments("echo a ;"), vec!["echo a"]);
        assert_eq!(split_segments("rm x &"), vec!["rm x"]);
    }

    #[test]
    fn split_segments_does_not_split_quoted_operators() {
        // A quoted `&&` is a single Word — not a split point.
        assert_eq!(split_segments(r#"echo "a && b""#), vec![r#"echo "a && b""#]);
        assert_eq!(split_segments("echo 'a ; b'"), vec!["echo 'a ; b'"]);
    }

    #[test]
    fn split_segments_does_not_split_redirects() {
        // `>` is a redirect operator, not a command separator.
        assert_eq!(split_segments("echo x > f"), vec!["echo x > f"]);
    }

    #[test]
    fn split_segments_leaves_command_substitution_as_one_segment() {
        // Documented gap: a command inside `$(...)` is part of one word, not split out. The whole
        // line is a single segment, so `rm` here is gated only by `echo`'s policy.
        assert_eq!(split_segments("echo $(rm x)"), vec!["echo $(rm x)"]);
    }

    #[test]
    fn split_segments_subshell_leading_command_resolves() {
        // A subshell IS covered: the segment's leading command is `rm` (parens are operators).
        let segs = split_segments("echo ok && (rm x)");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0], "echo ok");
        // The second segment resolves to `rm` (operators filtered, first Word wins).
        assert_eq!(leading_command(segs[1]).0, Some("rm".to_string()));
    }

    #[test]
    fn split_segments_single_command_is_one_segment() {
        assert_eq!(split_segments("rm -rf /x"), vec!["rm -rf /x"]);
        assert_eq!(split_segments("sudo ask \"q\""), vec!["sudo ask \"q\""]);
    }

    #[test]
    fn decision_rank_orders_by_strictness() {
        assert!(decision_rank(Decision::Allow) < decision_rank(Decision::Confirm { sudo_grant: false }));
        assert!(
            decision_rank(Decision::Confirm { sudo_grant: false })
                < decision_rank(Decision::Confirm { sudo_grant: true })
        );
        assert!(decision_rank(Decision::Confirm { sudo_grant: true }) < decision_rank(Decision::Deny));
    }

    #[test]
    fn gated_summary_labels_each_command_with_its_tier() {
        let gated = vec![
            ("rm".to_string(), AuthorizationPolicy::SudoOnly),
            ("curl".to_string(), AuthorizationPolicy::Confirm),
        ];
        assert_eq!(gated_commands_summary(&gated), "rm [sudo-only], curl [confirm]");
    }
}
