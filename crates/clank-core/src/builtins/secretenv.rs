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

/// Flags whose FOLLOWING argument (or `=`-joined value) is a credential that must never reach the
/// logs — `model add <provider> --key <KEY>`, cluster `--token`, and the like. (`export --secret`'s
/// `NAME=VALUE` shape is handled separately by [`redact_export_line`].)
///
/// Note: `grease registry add … --key <PUBKEY>` carries a *public* key, so redacting it here is a
/// harmless false positive — we deliberately over-redact a public value rather than risk leaking a
/// private one, since `--key` is overloaded across commands.
const SECRET_FLAGS: &[&str] = &["--key", "--token", "--password", "--api-key", "--auth-token"];

/// Redact the argument of any credential-bearing flag (see [`SECRET_FLAGS`]) in `line`, covering both
/// `--key VALUE` and `--key=VALUE`. Byte-exact via the tokenizer's source spans (like
/// [`redact_export_line`] / [`crate::builtins::http::split_http_head`]), so the rest of the line —
/// spacing, quoting, other args — is preserved. Returns `Some(redacted)` if anything was redacted,
/// `None` if the line carries no secret flag.
///
/// This is the log-safety guard for commands like `model add anthropic --key <KEY>`, whose shell.log
/// start/end events would otherwise record the credential verbatim — `mask_values` can't help there,
/// because the value isn't a registered secret when those events fire.
pub fn redact_secret_flag_args(line: &str) -> Option<String> {
    let tokens = tokenize_str(line).ok()?;
    // (unquoted value, raw-span start, raw-span end) for each Word, in order.
    let words: Vec<(String, usize, usize)> = tokens
        .iter()
        .filter_map(|t| match t {
            Token::Word(s, span) => Some((unquote_str(s), span.start.index, span.end.index)),
            Token::Operator(_, _) => None,
        })
        .collect();

    let redacted = crate::runtime::secretenv::REDACTED;
    // Byte ranges over the ORIGINAL line to overwrite, with their replacement text.
    let mut ranges: Vec<(usize, usize, String)> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let (w, start, end) = &words[i];
        // `--flag=value` form: redact only the value, keep the flag.
        if let Some(eq) = w.find('=') {
            let flag = &w[..eq];
            if SECRET_FLAGS.contains(&flag) {
                ranges.push((*start, *end, format!("{flag}={redacted}")));
                i += 1;
                continue;
            }
        }
        // `--flag value` form: redact the NEXT word.
        if SECRET_FLAGS.contains(&w.as_str()) {
            if let Some((_, ns, ne)) = words.get(i + 1) {
                ranges.push((*ns, *ne, redacted.to_string()));
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    if ranges.is_empty() {
        return None;
    }
    let mut out = line.to_string();
    ranges.sort_by_key(|r| r.0);
    // Apply high-offset-first so earlier ranges' indices stay valid.
    for (s, e, rep) in ranges.into_iter().rev() {
        if e <= out.len() && out.is_char_boundary(s) && out.is_char_boundary(e) {
            out.replace_range(s..e, &rep);
        }
    }
    Some(out)
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
    fn redacts_model_add_key_argument() {
        // The P0 leak: `model add … --key <KEY>` must not reach shell.log verbatim.
        let r = redact_secret_flag_args("model add anthropic --key sk-ant-SECRET123").unwrap();
        assert_eq!(r, "model add anthropic --key <redacted>");
        assert!(!r.contains("sk-ant-SECRET123"));
    }

    #[test]
    fn redacts_flag_equals_value_and_token_form() {
        assert_eq!(
            redact_secret_flag_args("model add anthropic --key=sk-SECRET").unwrap(),
            "model add anthropic --key=<redacted>"
        );
        assert_eq!(
            redact_secret_flag_args("golem cluster add prod --token tok-SECRET").unwrap(),
            "golem cluster add prod --token <redacted>"
        );
    }

    #[test]
    fn redact_secret_flag_args_preserves_quoting_and_ignores_non_secret_flags() {
        // A quoted value is still fully redacted; a line with no secret flag is left alone.
        let r = redact_secret_flag_args(r#"model add anthropic --key "sk with spaces""#).unwrap();
        assert!(!r.contains("sk with spaces") && r.contains("<redacted>"));
        assert!(redact_secret_flag_args("ls -la /tmp").is_none());
        assert!(redact_secret_flag_args("model list").is_none());
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
