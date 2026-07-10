//! The `xargs` builtin: build and run command lines from standard input.
//!
//! On clank there is no exec — every command is an in-component builtin — so xargs re-enters the
//! shell instead of spawning: each batch becomes a command line run via
//! `context.shell.run_string(...)`, the same re-entry the `eval` builtin uses. That makes this a
//! Brush [`Command`] (async execute) rather than a `SimpleCommand`. Stdin is read through
//! [`crate::coreutils::tool_stdin`], which on wasm enforces the never-read-real-stdin trap guard.
//!
//! Subset: whitespace tokenization (or `-d DELIM`), `-n MAX-ARGS` batching, `-I REPLACE`
//! per-token substitution. Empty input runs nothing (GNU's `--no-run-if-empty` is our default —
//! silently invoking a command with no arguments surprises more than it helps). Tokens are
//! shell-quoted before re-entry so filenames with spaces survive the round trip.
//!
//! Authorization note: the re-entered lines run inside Brush directly and are not re-gated by
//! `Session::eval_line` — the same leading-command-only scope documented in [`crate::authz`]
//! (compound lines and `eval` already share this limitation).

use std::io::Read;

use brush_core::builtins;
use brush_core::commands::ExecutionContext;
use brush_core::ExecutionResult;
use clap::Parser;

use crate::manifest::Manifest;

/// build and run command lines from standard input
#[derive(Parser)]
pub(crate) struct XargsCommand {
    /// Use at most MAX-ARGS arguments per command line.
    #[arg(short = 'n', value_name = "MAX-ARGS")]
    max_args: Option<usize>,

    /// Replace occurrences of REPLACE in the command with each input token (implies one
    /// invocation per token).
    #[arg(short = 'I', value_name = "REPLACE")]
    replace: Option<String>,

    /// Input token delimiter (a single character; \n, \t, \0 recognized). Default: whitespace.
    #[arg(short = 'd', value_name = "DELIM")]
    delimiter: Option<String>,

    /// The command and its initial arguments (default: echo).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

/// Quote one word for safe re-entry through the shell parser: plain words pass through, anything
/// else is single-quoted with embedded quotes escaped (`can't` → `'can'\''t'`).
fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/=:,+%@".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// `\n`/`\t`/`\0` unescaped, else the literal first char.
fn parse_delimiter(spec: &str) -> Option<char> {
    match spec {
        r"\n" => Some('\n'),
        r"\t" => Some('\t'),
        r"\0" => Some('\0'),
        other => other.chars().next(),
    }
}

/// Tokenize the whole input per the options.
fn tokenize(input: &str, delimiter: Option<char>) -> Vec<String> {
    match delimiter {
        Some(d) => input
            .split(d)
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect(),
        None => input.split_whitespace().map(String::from).collect(),
    }
}

/// The command lines to run: `-I` substitutes each token into the command words; otherwise
/// batches of `-n` (or all) tokens are appended, everything shell-quoted.
fn build_lines(
    command: &[String],
    tokens: &[String],
    replace: Option<&str>,
    max_args: Option<usize>,
) -> Vec<String> {
    if let Some(replace) = replace {
        tokens
            .iter()
            .map(|token| {
                command
                    .iter()
                    .map(|word| shell_quote(&word.replace(replace, token)))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    } else {
        let batch_size = max_args.filter(|n| *n > 0).unwrap_or(tokens.len().max(1));
        tokens
            .chunks(batch_size)
            .map(|batch| {
                command
                    .iter()
                    .map(|w| shell_quote(w))
                    .chain(batch.iter().map(|t| shell_quote(t)))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }
}

impl builtins::Command for XargsCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let mut input = String::new();
        let _ = crate::coreutils::tool_stdin(&context)
            .read_to_string(&mut input);

        // -I tokenizes by lines (GNU: each replacement is a whole input line), so
        // `find | xargs -I {} cmd {}` survives filenames with spaces. An explicit -d wins.
        let delimiter = self
            .delimiter
            .as_deref()
            .and_then(parse_delimiter)
            .or(self.replace.is_some().then_some('\n'));
        let tokens = tokenize(&input, delimiter);
        if tokens.is_empty() {
            return Ok(ExecutionResult::success());
        }

        let default_command = vec!["echo".to_string()];
        let command = if self.command.is_empty() {
            &default_command
        } else {
            &self.command
        };

        let lines = build_lines(command, &tokens, self.replace.as_deref(), self.max_args);

        let mut any_failed = false;
        for line in lines {
            let source_info = context.shell.call_stack().current_pos_as_source_info();
            let result = context
                .shell
                .run_string(line, &source_info, &context.params)
                .await?;
            if !result.is_success() {
                any_failed = true;
            }
        }
        // 123 = "any invocation exited nonzero", the xargs convention.
        Ok(ExecutionResult::new(if any_failed { 123 } else { 0 }))
    }
}

pub(crate) fn builtins<SE: brush_core::ShellExtensions>(
) -> Vec<(String, builtins::Registration<SE>)> {
    vec![("xargs".into(), builtins::builtin::<XargsCommand, SE>())]
}

pub(crate) fn manifests() -> Vec<Manifest> {
    vec![Manifest::builtin("xargs", "build and run command lines from standard input").with_help(
        "xargs [-n MAX-ARGS] [-I REPLACE] [-d DELIM] [COMMAND [ARG...]] — read tokens from stdin \
         and run COMMAND (default: echo) with them appended, in batches of -n. -I runs COMMAND \
         once per token with REPLACE substituted. Empty input runs nothing. Commands run as \
         in-shell builtins (no exec).",
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_passes_plain_words_and_wraps_the_rest() {
        assert_eq!(shell_quote("plain-word.txt"), "plain-word.txt");
        assert_eq!(shell_quote("/tmp/a=b:c"), "/tmp/a=b:c");
        assert_eq!(shell_quote("has space"), "'has space'");
        assert_eq!(shell_quote("can't"), r"'can'\''t'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a\"b"), "'a\"b'");
    }

    #[test]
    fn tokenize_whitespace_and_custom_delims() {
        assert_eq!(tokenize("a b\nc\t d", None), vec!["a", "b", "c", "d"]);
        assert_eq!(
            tokenize("a b\nc d\n", Some('\n')),
            vec!["a b", "c d"]
        );
        assert_eq!(tokenize("a:b::c", Some(':')), vec!["a", "b", "c"]);
        assert!(tokenize("   \n ", None).is_empty());
    }

    #[test]
    fn build_lines_batches_and_substitutes() {
        let cmd = vec!["echo".to_string()];
        let tokens: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
        // One batch by default.
        assert_eq!(build_lines(&cmd, &tokens, None, None), vec!["echo a b c"]);
        // -n 2 → two batches.
        assert_eq!(
            build_lines(&cmd, &tokens, None, Some(2)),
            vec!["echo a b", "echo c"]
        );
        // -I {} → one line per token, substituted then quoted.
        let mk = vec!["mkdir".to_string(), "{}/sub".to_string()];
        assert_eq!(
            build_lines(&mk, &["x y".to_string()], Some("{}"), None),
            vec!["mkdir 'x y/sub'"]
        );
    }

    #[test]
    fn delimiter_escapes() {
        assert_eq!(parse_delimiter(r"\n"), Some('\n'));
        assert_eq!(parse_delimiter(r"\t"), Some('\t'));
        assert_eq!(parse_delimiter(":"), Some(':'));
    }
}
