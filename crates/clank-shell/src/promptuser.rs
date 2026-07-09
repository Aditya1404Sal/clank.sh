//! `prompt-user`: the shell-internal builtin that pauses the current process, presents a prompt
//! to the human user, and returns the response to the caller (README "Human-in-the-Loop
//! Workflows"). Intercepted in [`crate::session::Session::eval_line`] before Brush dispatch —
//! the same precedent slot as `context` — rather than registered as a Brush builtin, because its
//! suspend logic (a real blocking read on native, a Golem promise await on wasm) cannot run
//! synchronously inside `SimpleCommand::execute` and must not nest inside Brush's own dispatch
//! loop (see the module-level risk notes in the phase plan: nested `.await`s inside Brush
//! dispatch reproduce the unresolved wasm pipeline hang).
//!
//! Only the detection + argument parsing is shared between native and wasm; the actual pause
//! mechanism is entirely different per target (native: a real blocking read; wasm: a Golem
//! promise, added in a later increment of this phase).

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

/// The result of a `prompt-user` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptUserOutcome {
    /// The human answered; the response text (never includes the trailing newline).
    Answered(String),
    /// The human aborted (Ctrl-C or, on wasm, an explicit abort signal in the promise response).
    Aborted,
}

impl PromptUserOutcome {
    /// The exit code this outcome maps to (README: `0` on response, `130` on abort).
    pub fn exit_code(&self) -> u8 {
        match self {
            PromptUserOutcome::Answered(_) => 0,
            PromptUserOutcome::Aborted => 130,
        }
    }

    /// The stdout bytes this outcome produces: the response text plus a trailing newline, or
    /// nothing on abort.
    pub fn stdout(&self) -> Vec<u8> {
        match self {
            PromptUserOutcome::Answered(s) => {
                let mut out = s.clone().into_bytes();
                out.push(b'\n');
                out
            }
            PromptUserOutcome::Aborted => Vec::new(),
        }
    }
}

/// Native backend: a real, synchronous blocking read. Takes an injectable reader (rather than
/// hardcoding `io::stdin()`) so tests can supply an in-memory buffer.
///
/// `stdin_md`, if `Some`, is markdown content to render before the question (README: piped stdin
/// is rendered as markdown before the question text). Real markdown rendering is deferred — clank
/// has no rich terminal yet — so it is written verbatim.
///
/// EOF (no more input) is treated as an abort, matching the Ctrl-C convention: the human is gone,
/// there is no answer to give.
#[cfg(not(target_arch = "wasm32"))]
pub fn run_native(
    args: &PromptUserArgs,
    stdin_md: Option<&str>,
    out: &mut dyn std::io::Write,
    input: &mut dyn std::io::BufRead,
) -> PromptUserOutcome {
    if let Some(md) = stdin_md {
        let _ = writeln!(out, "{md}");
    }
    let _ = write!(out, "{}", args.question);
    if let Some(choices) = &args.choices {
        let _ = write!(out, " [{}]", choices.join("/"));
    }
    let _ = writeln!(out);

    let mut line = String::new();
    let Ok(n) = input.read_line(&mut line) else {
        return PromptUserOutcome::Aborted;
    };
    if n == 0 {
        // EOF: no answer was or will be given.
        return PromptUserOutcome::Aborted;
    }
    let answer = line.trim_end_matches(['\n', '\r']).to_string();

    if let Some(choices) = &args.choices {
        if !choices.iter().any(|c| c == &answer) {
            // Not one of the offered choices: re-prompt is out of scope for this phase (no loop
            // driver here) — treat an invalid choice as the human declining to answer cleanly.
            let _ = writeln!(
                out,
                "prompt-user: '{answer}' is not one of [{}]",
                choices.join("/")
            );
            return PromptUserOutcome::Aborted;
        }
    }

    PromptUserOutcome::Answered(answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_run_returns_the_typed_answer() {
        let args = parse(r#"prompt-user "Which environment?""#).unwrap();
        let mut out = Vec::new();
        let mut input = Cursor::new(b"production\n".to_vec());
        let outcome = run_native(&args, None, &mut out, &mut input);
        assert_eq!(outcome, PromptUserOutcome::Answered("production".to_string()));
        assert_eq!(outcome.exit_code(), 0);
        assert_eq!(outcome.stdout(), b"production\n");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_run_renders_piped_markdown_before_the_question() {
        let args = parse(r#"prompt-user "Any concerns?""#).unwrap();
        let mut out = Vec::new();
        let mut input = Cursor::new(b"no\n".to_vec());
        run_native(&args, Some("# report\n\nall good"), &mut out, &mut input);
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.starts_with("# report\n\nall good\n"));
        assert!(rendered.contains("Any concerns?"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_run_eof_is_aborted() {
        let args = parse(r#"prompt-user "q?""#).unwrap();
        let mut out = Vec::new();
        let mut input = Cursor::new(Vec::new());
        let outcome = run_native(&args, None, &mut out, &mut input);
        assert_eq!(outcome, PromptUserOutcome::Aborted);
        assert_eq!(outcome.exit_code(), 130);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_run_rejects_answers_outside_choices() {
        let args = parse(r#"prompt-user "Approve?" --confirm"#).unwrap();
        let mut out = Vec::new();
        let mut input = Cursor::new(b"maybe\n".to_vec());
        let outcome = run_native(&args, None, &mut out, &mut input);
        assert_eq!(outcome, PromptUserOutcome::Aborted);
    }
}
