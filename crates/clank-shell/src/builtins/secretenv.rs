//! `export --secret KEY=value`: mark an environment variable as sensitive (README "Sensitive
//! environment variables"). The variable is available to agents via the environment — `$KEY` expands
//! in scripts and real subprocesses inherit it — but its value is **redacted everywhere the shell
//! renders session state**: never echoed by `env`, never written to the logs, never shown in `ps`,
//! and never entered into the transcript.
//!
//! This module owns only the *detection and parsing* of a `export --secret …` line. The runtime
//! effects (set the var in Brush's table + `std::env`, record it in the session's secret table, and
//! redact the recorded command line) live in [`crate::session::Session`]; the redaction *filter* that
//! every render path consults lives in [`crate::runtime::secretenv`].
//!
//! Only the `export --secret NAME=VALUE` form is intercepted. Plain `export` (no `--secret`) falls
//! through to Brush unchanged, as do `export --secret NAME` (no value — nothing sensitive to hide)
//! and any malformed form (Brush produces its own diagnostic).

use brush_parser::{tokenize_str, unquote_str, Token};

/// A parsed `export --secret NAME=VALUE` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretExport {
    pub name: String,
    pub value: String,
}

/// If `line` is an `export --secret NAME=VALUE` invocation, parse it into its `(name, value)`.
/// Returns `None` for every other line — including a bare `export`, `export` without `--secret`, and
/// `export --secret NAME` with no `=value` (which marks nothing sensitive, so it's left to Brush).
///
/// Quote-aware via Brush's own tokenizer (the value is routinely a quoted secret), matching
/// [`crate::builtins::promptuser::parse`]. Multiple assignments on one line are not supported — the
/// first `--secret NAME=VALUE` is taken and the rest ignored; the common case is one secret per line.
pub fn parse(line: &str) -> Option<SecretExport> {
    let tokens = tokenize_str(line).ok()?;
    let words: Vec<String> = tokens
        .into_iter()
        .filter_map(|t| match t {
            Token::Word(s, _) => Some(unquote_str(&s)),
            Token::Operator(_, _) => None,
        })
        .collect();

    // Leading word must be exactly `export`, and `--secret` must appear as a flag.
    let mut iter = words.iter();
    if iter.next().map(String::as_str) != Some("export") {
        return None;
    }
    if !words.iter().any(|w| w == "--secret") {
        return None;
    }

    // The first non-flag `NAME=VALUE` word is the secret assignment.
    for word in words.iter().skip(1) {
        if word == "--secret" || word.starts_with('-') {
            continue;
        }
        if let Some((name, value)) = word.split_once('=') {
            if !name.is_empty() {
                return Some(SecretExport {
                    name: name.to_string(),
                    value: value.to_string(),
                });
            }
        }
    }
    None
}

/// Whether `line` is an `export --secret NAME=VALUE` invocation this module handles.
pub fn is_secret_export(line: &str) -> bool {
    parse(line).is_some()
}

/// A log-safe rendering of a secret-export line: the value is replaced with the redaction placeholder
/// so it never reaches `shell.log`. `None` if `line` is not a secret export (the caller logs it
/// unchanged). This declaration line is the one place `mask_values` can't help — the secret isn't
/// registered in the active set until the line actually runs, and the shell.log start/end events fire
/// around that — so the value is stripped structurally here instead.
pub fn redact_export_line(line: &str) -> Option<String> {
    let export = parse(line)?;
    Some(format!(
        "export --secret {}={}",
        export.name,
        crate::runtime::secretenv::REDACTED
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_secret_export() {
        let s = parse("export --secret API_KEY=sk-abc123").unwrap();
        assert_eq!(s.name, "API_KEY");
        assert_eq!(s.value, "sk-abc123");
    }

    #[test]
    fn parses_quoted_value_with_spaces() {
        let s = parse(r#"export --secret TOKEN="a b c""#).unwrap();
        assert_eq!(s.name, "TOKEN");
        assert_eq!(s.value, "a b c");
    }

    #[test]
    fn secret_flag_after_assignment_still_parses() {
        let s = parse("export KEY=val --secret").unwrap();
        assert_eq!(s.name, "KEY");
        assert_eq!(s.value, "val");
    }

    #[test]
    fn empty_value_is_still_a_secret_assignment() {
        // `export --secret KEY=` marks KEY sensitive with an empty value — a valid (if unusual) form.
        let s = parse("export --secret KEY=").unwrap();
        assert_eq!(s.name, "KEY");
        assert_eq!(s.value, "");
    }

    #[test]
    fn plain_export_is_not_a_secret_export() {
        assert!(parse("export PATH=/usr/bin").is_none());
        assert!(parse("export FOO=bar").is_none());
    }

    #[test]
    fn secret_flag_without_assignment_falls_through() {
        // Nothing sensitive to hide (marks an already-set var); leave it to Brush.
        assert!(parse("export --secret KEY").is_none());
    }

    #[test]
    fn non_export_lines_are_none() {
        assert!(parse("echo --secret KEY=val").is_none());
        assert!(parse("KEY=val").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn is_secret_export_matches_parse() {
        assert!(is_secret_export("export --secret K=v"));
        assert!(!is_secret_export("export K=v"));
    }

    #[test]
    fn quoted_value_with_shell_metachars_parses_as_one_value() {
        // A pipe/redirect INSIDE quotes is part of the value, not a real operator — the tokenizer
        // classifies it as a `Word`. This is what lets `is_plain_line` (in the session layer) accept
        // `export --secret K="a|b"` as a plain top-level line rather than mistaking it for a pipeline.
        let s = parse(r#"export --secret K="a|b>c""#).unwrap();
        assert_eq!(s.name, "K");
        assert_eq!(s.value, "a|b>c");
    }
}
