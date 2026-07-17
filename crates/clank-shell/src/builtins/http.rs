//! `curl`/`wget`: outbound-HTTP shell commands, backed by the [`wcurl`]/[`waget`] crates.
//!
//! Unlike file/text builtins, these are NOT Brush `SimpleCommand`s. Their HTTP is async and — on the
//! Golem agent — the `wstd` WASI-HTTP client requires the wstd reactor as the *running* executor.
//! Inside a Brush builtin, execution is nested under clank's own tokio `rt.block_on` (in
//! `Session::execute`), where the wstd reactor is not live — the "Wall C" shape. So HTTP must be
//! `.await`-ed at the `Session` layer, one level under the Golem SDK's `wstd::block_on`.
//!
//! They are therefore dispatched from `Session::run_command` (the shared execution choke point,
//! reached by both the direct-allow path and the post-authorization-approval path). This module owns
//! only the leading-word detection, the argv-tail extraction, and the command manifests. The
//! `curl`/`wget` manifests carry `AuthorizationPolicy::Confirm` (README: "Outbound HTTP → confirm"),
//! so the existing authz gate surfaces a confirmation before the request runs — no new gating logic.

use brush_parser::{tokenize_str, unquote_str, Token};

use crate::manifest::{AuthorizationPolicy, ExecutionScope, Manifest};

/// The two HTTP commands clank intercepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpCommand {
    Curl,
    Wget,
}

/// If `line`'s leading command word is `curl` or `wget`, return which one plus the argv **tail**
/// (the words after the command, dequoted). `None` for any other line.
pub fn classify(line: &str) -> Option<(HttpCommand, Vec<String>)> {
    let words = leading_words(line)?;
    let (first, rest) = words.split_first()?;
    let cmd = match first.as_str() {
        "curl" => HttpCommand::Curl,
        "wget" => HttpCommand::Wget,
        _ => return None,
    };
    Some((cmd, rest.to_vec()))
}

/// The `Word` tokens of `line`, dequoted (quote-aware via Brush's tokenizer; operators dropped).
/// `None` if the line doesn't tokenize (it falls through to Brush, which reports its own error).
fn leading_words(line: &str) -> Option<Vec<String>> {
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

/// The `curl` and `wget` manifests. `Subprocess` scope (they run isolated, no shell-state access),
/// `Confirm` policy (outbound HTTP pauses for user confirmation, per the README).
pub fn manifests() -> Vec<Manifest> {
    vec![
        Manifest::builtin("curl", "transfer data from a URL over HTTP")
            .with_scope(ExecutionScope::Subprocess)
            .with_policy(AuthorizationPolicy::Confirm)
            .with_help(
                "curl <url> [-o file] [-s] [-L] [-i] [-I] [-f] [-X method] [-d data|@file] \
                 [--json data] [-G] [-H \"K: V\"] [-A ua] [-u user:pass] [-e referer] \
                 [-m secs] [--connect-timeout secs] [-w fmt] [-v] — fetch a URL over HTTP. Body to \
                 stdout (or -o file). -L follows redirects, -i/-I include/only headers, -f fails on \
                 4xx/5xx, -w expands %{http_code} etc. Short flags cluster (-fsSL). Outbound HTTP \
                 requires confirmation.",
            ),
        Manifest::builtin("wget", "download a file from a URL over HTTP")
            .with_scope(ExecutionScope::Subprocess)
            .with_policy(AuthorizationPolicy::Confirm)
            .with_help(
                "wget <url> [-O file|-] [-q] [-S] [-T secs] [-t tries] [--max-redirect n] \
                 [--post-data data|--post-file file] [--header \"K: V\"] [-U ua] \
                 [--content-disposition] — download a URL over HTTP to a file (named after the URL \
                 by default, -O - for stdout). Follows redirects by default; -S prints response \
                 headers. Outbound HTTP requires confirmation.",
            ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_curl_and_splits_args() {
        let (cmd, args) = classify(r#"curl -s https://example.com"#).unwrap();
        assert_eq!(cmd, HttpCommand::Curl);
        assert_eq!(args, vec!["-s", "https://example.com"]);
    }

    #[test]
    fn classifies_wget() {
        let (cmd, args) = classify("wget https://example.com/f").unwrap();
        assert_eq!(cmd, HttpCommand::Wget);
        assert_eq!(args, vec!["https://example.com/f"]);
    }

    #[test]
    fn dequotes_a_quoted_header_arg() {
        let (_, args) = classify(r#"curl -H "Accept: application/json" https://x"#).unwrap();
        assert_eq!(args, vec!["-H", "Accept: application/json", "https://x"]);
    }

    #[test]
    fn non_http_command_is_none() {
        assert!(classify("echo curl").is_none());
        assert!(classify("cat file").is_none());
        assert!(classify("").is_none());
    }

    #[test]
    fn manifests_are_confirm_policy() {
        for m in manifests() {
            assert_eq!(m.authorization_policy, AuthorizationPolicy::Confirm);
            assert_eq!(m.execution_scope, ExecutionScope::Subprocess);
        }
    }
}
