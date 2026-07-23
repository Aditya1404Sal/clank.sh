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
pub(crate) enum HttpCommand {
    Curl,
    Wget,
}

/// If `line` is an **operator-free** invocation whose leading word is `curl` or `wget`, return
/// which one plus the argv **tail** (the words after the command, dequoted). `None` for any other
/// line — including a curl/wget line carrying ANY shell operator (`|`, `&&`, `;`, a redirect):
/// those must not be flattened into an argv (the old behavior dropped the operators and handed
/// wcurl `[-s, URL, jq, .x]` for `curl -s URL | jq .x` — "unknown option: jq"). Operator-bearing
/// lines either match [`split_http_head`] (a curl/wget-headed pipeline, handled at the Session
/// layer) or fall through to Brush, where the honest `CurlStub` answers.
pub(crate) fn classify(line: &str) -> Option<(HttpCommand, Vec<String>)> {
    let words = operator_free_words(line)?;
    let (first, rest) = words.split_first()?;
    let cmd = match first.as_str() {
        "curl" => HttpCommand::Curl,
        "wget" => HttpCommand::Wget,
        _ => return None,
    };
    Some((cmd, rest.to_vec()))
}

/// The `Word` tokens of `line`, dequoted (quote-aware via Brush's tokenizer) — but `None` the
/// moment ANY `Operator` token appears, or if the line doesn't tokenize (it falls through to
/// Brush, which reports its own error).
fn operator_free_words(line: &str) -> Option<Vec<String>> {
    let tokens = tokenize_str(line).ok()?;
    let mut words = Vec::with_capacity(tokens.len());
    for t in tokens {
        match t {
            Token::Word(s, _) => words.push(unquote_str(&s)),
            Token::Operator(_, _) => return None,
        }
    }
    (!words.is_empty()).then_some(words)
}

/// A pipeline whose FIRST stage is a curl/wget invocation: `curl … | rest…`. The head's HTTP runs
/// at the Session layer (Wall C: async HTTP can't run inside Brush), and `rest` runs through Brush
/// with the response bytes as its stdin — so `curl -s URL | jq .x | head -1` composes on both
/// targets.
pub(crate) struct HttpHeadPipe {
    pub cmd: HttpCommand,
    /// The head's argv tail (after `curl`/`wget`, dequoted).
    pub args: Vec<String>,
    /// The byte-exact remainder of the line after the first top-level `|` — an arbitrary Brush
    /// program (may itself contain more pipes, redirects, …).
    pub downstream: String,
}

/// Split a curl/wget-headed pipeline at the FIRST top-level `|` (byte-exact via the tokenizer's
/// source spans, the same technique as `split_ask_tail` — quoting inside arguments survives).
///
/// `None` unless ALL of: the first operator token in the line is a literal `|` (a redirect or
/// `&&`/`;` before it declines — those shapes stay with Brush's honest stub); the head (with a
/// leading `sudo` stripped — the authz gate already consumed the elevation) classifies as an
/// operator-free curl/wget invocation; and the downstream is non-empty.
pub(crate) fn split_http_head(line: &str) -> Option<HttpHeadPipe> {
    let tokens = tokenize_str(line).ok()?;
    // The first operator in the line must be the pipe we split at.
    let mut pipe_end: Option<usize> = None;
    for t in &tokens {
        if let Token::Operator(op, span) = t {
            if op == "|" {
                pipe_end = Some(span.end.index);
            }
            break;
        }
    }
    let pipe_end = pipe_end?;
    // Guard against a byte index that isn't a char boundary (defensive; spans are byte offsets).
    if pipe_end > line.len() || !line.is_char_boundary(pipe_end) {
        return None;
    }
    let (head_raw, downstream) = line.split_at(pipe_end);
    let head = head_raw.trim_end().trim_end_matches('|').trim_end();
    // A `sudo` on the head elevated the line at the gate; strip it before classifying.
    let head_effective = match head.strip_prefix("sudo") {
        Some(rest) if rest.starts_with(char::is_whitespace) => rest.trim_start(),
        _ => head,
    };
    let (cmd, args) = classify(head_effective)?;
    let downstream = downstream.trim().to_string();
    if downstream.is_empty() {
        return None; // `curl … |` with no consumer — not a real pipeline
    }
    Some(HttpHeadPipe { cmd, args, downstream })
}

/// The `curl` and `wget` manifests. `Subprocess` scope (they run isolated, no shell-state access),
/// `Confirm` policy (outbound HTTP pauses for user confirmation, per the README).
pub(crate) fn manifests() -> Vec<Manifest> {
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
        let (cmd, args) = classify(r"curl -s https://example.com").unwrap();
        assert_eq!(cmd, HttpCommand::Curl);
        assert_eq!(args, vec!["-s", "https://example.com"]);
    }

    #[test]
    fn classifies_wget() {
        let (cmd, args) = classify("wget https://example.com/f").unwrap();
        assert_eq!(cmd, HttpCommand::Wget);
        assert_eq!(args, vec!["https://example.com/f"]);
    }

    /// A curl/wget line carrying ANY operator no longer classifies — the old behavior flattened
    /// `curl -s URL | jq .x` into wcurl argv `[-s, URL, jq, .x]` ("unknown option: jq").
    #[test]
    fn operator_lines_do_not_classify() {
        assert!(classify("curl -s https://x | jq .x").is_none());
        assert!(classify("curl https://x && echo done").is_none());
        assert!(classify("curl https://x > out.html").is_none());
        assert!(classify("echo hi; wget https://x").is_none());
    }

    #[test]
    fn splits_a_curl_headed_pipeline() {
        let p = split_http_head("curl -s https://x | jq .rates.INR").unwrap();
        assert_eq!(p.cmd, HttpCommand::Curl);
        assert_eq!(p.args, vec!["-s", "https://x"]);
        assert_eq!(p.downstream, "jq .rates.INR");
    }

    #[test]
    fn splits_at_the_first_pipe_with_multistage_downstream() {
        let p = split_http_head("curl -s https://x | jq .a | head -1").unwrap();
        assert_eq!(p.args, vec!["-s", "https://x"]);
        assert_eq!(p.downstream, "jq .a | head -1");
    }

    #[test]
    fn head_split_survives_quoted_pipes_in_args() {
        let p = split_http_head(r"curl -H 'X-A: a|b' https://x | grep ok").unwrap();
        assert_eq!(p.args, vec!["-H", "X-A: a|b", "https://x"]);
        assert_eq!(p.downstream, "grep ok");
    }

    #[test]
    fn head_split_strips_a_leading_sudo() {
        let p = split_http_head("sudo curl -s https://x | jq .").unwrap();
        assert_eq!(p.cmd, HttpCommand::Curl);
        assert_eq!(p.args, vec!["-s", "https://x"]);
    }

    #[test]
    fn wget_head_splits_too() {
        let p = split_http_head("wget -O - https://x | grep body").unwrap();
        assert_eq!(p.cmd, HttpCommand::Wget);
        assert_eq!(p.args, vec!["-O", "-", "https://x"]);
    }

    #[test]
    fn head_split_declines_non_head_and_junk_shapes() {
        // curl mid-pipeline: the head is `cat f`, not curl.
        assert!(split_http_head("cat f | curl https://x").is_none());
        // `||` is not a pipe.
        assert!(split_http_head("curl https://x || echo fail").is_none());
        // A redirect (an operator) before the pipe declines — Brush's stub answers.
        assert!(split_http_head("curl -o f https://x 2>&1 | grep ok").is_none());
        // Empty downstream is not a pipeline.
        assert!(split_http_head("curl https://x |").is_none());
        // No pipe at all.
        assert!(split_http_head("curl https://x").is_none());
    }

    /// Single quotes are POSIX-literal all the way through the intercept: `-w '\n'` must reach
    /// wcurl as the two characters `\` `n`, not a bare `n`. Regression pin for the brush-fork
    /// `unquote_str` fix (rev 02de798) — the old flat scan consumed the backslash and curl printed
    /// a literal `n` where the user asked for a newline.
    #[test]
    fn single_quoted_backslash_survives_dequoting() {
        let (_, args) = classify(r"curl -s -w '\n' https://x").unwrap();
        assert_eq!(args, vec!["-s", "-w", r"\n", "https://x"]);
        let (_, args) = classify(r"curl -w '%{http_code}\n' https://x").unwrap();
        assert_eq!(args[1], r"%{http_code}\n");
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
