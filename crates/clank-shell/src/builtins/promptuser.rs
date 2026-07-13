//! `prompt-user`: the shell-internal builtin that pauses the current process, presents a prompt
//! to the human user, and returns the response to the caller (README "Human-in-the-Loop
//! Workflows"). Intercepted in [`crate::session::Session::eval_line`] before Brush dispatch —
//! the same precedent slot as `context`.
//!
//! **The pause never hangs the shell.** `prompt-user` does not block waiting for a human: it
//! records a [`PendingPrompt`] as durable [`Session`](crate::session::Session) state, flips the
//! process row to `P`, and returns immediately — surfacing the question *to whoever invoked the
//! session* (the interactive REPL, the AI via `ask`, a future UI). That caller owns the human
//! channel and delivers the answer on the **next** `eval` call as [structured input](AnswerInput).
//! This is the honest fit for both deployments: the native REPL loop reads the next stdin line and
//! feeds it back inline; the Golem agent returns the pending prompt in its result and the external
//! caller answers on a follow-up invocation. (An earlier design suspended the whole invocation on a
//! Golem promise — abandoned once a spike found Golem serializes per-agent invocations, so a parked
//! agent is unreachable; the README's model is interactive terminal I/O, not an out-of-band
//! completer.)
//!
//! This module owns detection, argument parsing, the [`PendingPrompt`] shape, and resolving a
//! pending prompt against a delivered answer.

use brush_parser::{tokenize_str, unquote_str, Token};

/// Whether `line` invokes `prompt-user` (the leading shell word, quote-aware). Mirrors
/// [`crate::dispatch_context`]'s "leading word" detection, but uses Brush's own tokenizer instead
/// of `split_whitespace()` since `prompt-user`'s question argument is routinely a quoted string
/// containing spaces (every README example quotes it).
pub fn is_prompt_user(line: &str) -> bool {
    leading_word(line).as_deref() == Some("prompt-user")
}

/// The first word token of `line`, or `None` if it doesn't tokenize (fall through to Brush, which
/// will produce its own parse error) or is empty.
fn leading_word(line: &str) -> Option<String> {
    let tokens = tokenize_str(line).ok()?;
    tokens.into_iter().find_map(|t| match t {
        Token::Word(s, _) => Some(unquote_str(&s)),
        Token::Operator(_, _) => None,
    })
}

/// A parsed `prompt-user` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptUserArgs {
    pub question: String,
    pub choices: Option<Vec<String>>,
    pub secret: bool,
}

/// Error parsing a `prompt-user` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// No question text was given.
    MissingQuestion,
    /// A flag needed a value that wasn't supplied (e.g. `--choices` with nothing after it).
    MissingValue(String),
    /// An unrecognized flag.
    UnknownFlag(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::MissingQuestion => write!(f, "prompt-user: missing question"),
            ParseError::MissingValue(flag) => write!(f, "prompt-user: {flag}: missing value"),
            ParseError::UnknownFlag(flag) => write!(f, "prompt-user: unknown flag: {flag}"),
        }
    }
}

/// Parse a full `prompt-user` line (including the leading `prompt-user` word itself) into its
/// arguments. Accepts `--choices <a,b,...>`, `--confirm` (shorthand for `--choices yes,no`), and
/// `--secret`. Exactly one non-flag word is the question; a second non-flag word is an error to
/// avoid silently swallowing a mistyped flag as a second question.
pub fn parse(line: &str) -> Result<PromptUserArgs, ParseError> {
    let tokens = tokenize_str(line).unwrap_or_default();
    let words: Vec<String> = tokens
        .into_iter()
        .filter_map(|t| match t {
            Token::Word(s, _) => Some(unquote_str(&s)),
            Token::Operator(_, _) => None,
        })
        .collect();

    // words[0] is "prompt-user" itself (checked by the caller via `is_prompt_user`).
    let mut question: Option<String> = None;
    let mut choices: Option<Vec<String>> = None;
    let mut confirm = false;
    let mut secret = false;

    let mut iter = words.into_iter().skip(1);
    while let Some(word) = iter.next() {
        match word.as_str() {
            "--choices" => {
                let value = iter
                    .next()
                    .ok_or_else(|| ParseError::MissingValue("--choices".to_string()))?;
                choices = Some(value.split(',').map(str::to_string).collect());
            }
            "--confirm" => confirm = true,
            "--secret" => secret = true,
            _ if word.starts_with("--") => return Err(ParseError::UnknownFlag(word)),
            _ if question.is_none() => question = Some(word),
            other => return Err(ParseError::UnknownFlag(other.to_string())),
        }
    }

    if confirm {
        choices = Some(vec!["yes".to_string(), "no".to_string()]);
    }

    Ok(PromptUserArgs {
        question: question.ok_or(ParseError::MissingQuestion)?,
        choices,
        secret,
    })
}

/// A pending `prompt-user`: the durable state recorded when the shell surfaces a question and
/// awaits a response on a follow-up call. Held on the [`Session`](crate::session::Session) (so it
/// persists across Golem invocations via the oplog) and returned to the caller in the eval result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPrompt {
    pub question: String,
    pub choices: Option<Vec<String>>,
    pub secret: bool,
}

impl PromptUserArgs {
    /// Convert parsed args into a [`PendingPrompt`]. `stdin_md`, if present, is prepended to the
    /// question verbatim (README: piped stdin is rendered as markdown before the question; real
    /// markdown rendering is deferred — clank has no rich terminal yet).
    pub fn into_pending(self, stdin_md: Option<&str>) -> PendingPrompt {
        let question = match stdin_md {
            Some(md) => format!("{md}\n{}", self.question),
            None => self.question,
        };
        PendingPrompt {
            question,
            choices: self.choices,
            secret: self.secret,
        }
    }
}

/// A structured answer delivered to a pending prompt on the follow-up call. The caller is explicit
/// about whether the human answered or aborted, rather than overloading a sentinel line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnswerInput {
    /// The human's response text.
    Response(String),
    /// The human aborted / gave no answer (README: exit `130`, the Ctrl-C convention).
    Abort,
}

/// The result of resolving a pending prompt against a delivered [`AnswerInput`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// A valid response: stdout bytes (the response + newline) and exit `0`.
    Answered { stdout: Vec<u8>, secret: bool },
    /// Aborted: no stdout, exit `130`.
    Aborted,
    /// The response wasn't one of the prompt's `--choices`: an error message for stderr, and the
    /// prompt stays pending (the caller re-asks). Exit `1`.
    InvalidChoice { message: String },
}

/// Resolve a [`PendingPrompt`] against a delivered answer. A response is validated against the
/// prompt's `choices`; a non-matching response leaves the prompt pending (the caller can re-ask).
pub fn resolve(pending: &PendingPrompt, answer: AnswerInput) -> Resolution {
    match answer {
        AnswerInput::Abort => Resolution::Aborted,
        AnswerInput::Response(text) => {
            // With `--choices`, resolve the answer to its canonical choice: an exact
            // (case-insensitive) match wins, else a UNIQUE case-insensitive prefix — so the
            // `(y)es, (n)o, (a)ll` confirmation accepts `y`/`n`/`a`, not only the full words. The
            // canonical value (not the raw keystroke) flows downstream, so the authz gate's
            // `yes`/`all` check and a `$(prompt-user …)` capture both see the real choice.
            let response = match &pending.choices {
                Some(choices) => match match_choice(choices, &text) {
                    Some(canonical) => canonical,
                    None => {
                        return Resolution::InvalidChoice {
                            message: format!(
                                "prompt-user: '{text}' is not one of [{}]\n",
                                choices.join("/")
                            ),
                        };
                    }
                },
                None => text,
            };
            let mut stdout = response.into_bytes();
            stdout.push(b'\n');
            Resolution::Answered {
                stdout,
                secret: pending.secret,
            }
        }
    }
}

/// Match a response against the offered `--choices`, returning the canonical choice. An exact
/// case-insensitive match wins outright (so an exact answer never loses to prefix ambiguity);
/// otherwise a single case-insensitive prefix match resolves (`y` → `yes`), while an ambiguous or
/// empty prefix is rejected.
fn match_choice(choices: &[String], text: &str) -> Option<String> {
    let t = text.trim();
    if let Some(c) = choices.iter().find(|c| c.eq_ignore_ascii_case(t)) {
        return Some(c.clone());
    }
    if t.is_empty() {
        return None;
    }
    let tl = t.to_ascii_lowercase();
    let mut hits = choices
        .iter()
        .filter(|c| c.to_ascii_lowercase().starts_with(&tl));
    match (hits.next(), hits.next()) {
        (Some(c), None) => Some(c.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn into_pending_prepends_stdin_markdown() {
        let args = parse(r#"prompt-user "Any concerns?""#).unwrap();
        let pending = args.into_pending(Some("# report\n\nall good"));
        assert_eq!(pending.question, "# report\n\nall good\nAny concerns?");
    }

    #[test]
    fn resolve_answer_produces_stdout_and_carries_secret() {
        let pending = parse(r#"prompt-user "q?" --secret"#).unwrap().into_pending(None);
        match resolve(&pending, AnswerInput::Response("prod".to_string())) {
            Resolution::Answered { stdout, secret } => {
                assert_eq!(stdout, b"prod\n");
                assert!(secret);
            }
            other => panic!("expected Answered, got {other:?}"),
        }
    }

    #[test]
    fn resolve_abort_is_aborted() {
        let pending = parse(r#"prompt-user "q?""#).unwrap().into_pending(None);
        assert_eq!(resolve(&pending, AnswerInput::Abort), Resolution::Aborted);
    }

    #[test]
    fn resolve_rejects_answer_outside_choices() {
        let pending = parse(r#"prompt-user "Approve?" --confirm"#).unwrap().into_pending(None);
        match resolve(&pending, AnswerInput::Response("maybe".to_string())) {
            Resolution::InvalidChoice { message } => assert!(message.contains("yes/no")),
            other => panic!("expected InvalidChoice, got {other:?}"),
        }
    }

    #[test]
    fn resolve_accepts_a_valid_choice() {
        let pending = parse(r#"prompt-user "Approve?" --confirm"#).unwrap().into_pending(None);
        match resolve(&pending, AnswerInput::Response("yes".to_string())) {
            Resolution::Answered { stdout, .. } => assert_eq!(stdout, b"yes\n"),
            other => panic!("expected Answered, got {other:?}"),
        }
    }

    #[test]
    fn resolve_accepts_letter_shortcuts_as_canonical_choice() {
        // The authz confirm offers `(y)es, (n)o, (a)ll`; single letters must resolve to the full
        // canonical word (so the gate's `yes`/`all` check downstream still matches). Same `resolve`
        // path a `confirm_choices()` gate uses, exercised here via an explicit three-way choice set.
        let pending = parse(r#"prompt-user "Approve?" --choices yes,no,all"#).unwrap().into_pending(None);
        for (input, want) in [("y", "yes\n"), ("n", "no\n"), ("a", "all\n"), ("YES", "yes\n")] {
            match resolve(&pending, AnswerInput::Response(input.to_string())) {
                Resolution::Answered { stdout, .. } => {
                    assert_eq!(stdout, want.as_bytes(), "input {input:?}")
                }
                other => panic!("expected Answered for {input:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn resolve_prefers_exact_match_over_ambiguous_prefix() {
        // An exact answer must win even when it is also a prefix of another choice.
        let pending = parse(r#"prompt-user "n?" --choices 1,10,100"#).unwrap().into_pending(None);
        match resolve(&pending, AnswerInput::Response("1".to_string())) {
            Resolution::Answered { stdout, .. } => assert_eq!(stdout, b"1\n"),
            other => panic!("expected Answered, got {other:?}"),
        }
    }

    #[test]
    fn resolve_rejects_ambiguous_prefix() {
        let pending = parse(r#"prompt-user "?" --choices yes,yesterday"#).unwrap().into_pending(None);
        assert!(matches!(
            resolve(&pending, AnswerInput::Response("y".to_string())),
            Resolution::InvalidChoice { .. }
        ));
    }

    #[test]
    fn detects_leading_prompt_user_word() {
        assert!(is_prompt_user(r#"prompt-user "hi""#));
        assert!(is_prompt_user("prompt-user"));
        assert!(!is_prompt_user("echo prompt-user"));
        assert!(!is_prompt_user("promptuser foo"));
    }

    #[test]
    fn parses_plain_question() {
        let args = parse(r#"prompt-user "Which environment?""#).unwrap();
        assert_eq!(args.question, "Which environment?");
        assert_eq!(args.choices, None);
        assert!(!args.secret);
    }

    #[test]
    fn parses_choices() {
        let args = parse(
            r#"prompt-user "Which environment?" --choices staging,production,development"#,
        )
        .unwrap();
        assert_eq!(
            args.choices,
            Some(vec![
                "staging".to_string(),
                "production".to_string(),
                "development".to_string()
            ])
        );
    }

    #[test]
    fn confirm_expands_to_yes_no_choices() {
        let args = parse(r#"prompt-user "Approve this operation?" --confirm"#).unwrap();
        assert_eq!(
            args.choices,
            Some(vec!["yes".to_string(), "no".to_string()])
        );
    }

    #[test]
    fn secret_flag_is_parsed() {
        let args = parse(r#"prompt-user "Enter the API key:" --secret"#).unwrap();
        assert!(args.secret);
    }

    #[test]
    fn missing_question_is_an_error() {
        assert_eq!(parse("prompt-user --confirm"), Err(ParseError::MissingQuestion));
    }

    #[test]
    fn missing_choices_value_is_an_error() {
        assert_eq!(
            parse("prompt-user \"q\" --choices"),
            Err(ParseError::MissingValue("--choices".to_string()))
        );
    }

    #[test]
    fn unknown_flag_is_an_error() {
        assert_eq!(
            parse("prompt-user \"q\" --bogus"),
            Err(ParseError::UnknownFlag("--bogus".to_string()))
        );
    }
}
