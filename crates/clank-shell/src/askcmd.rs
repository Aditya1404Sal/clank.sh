//! `ask`: the AI-native LLM command — clank's primary human-AI interface.
//!
//! `ask "question"` sends the shell's sliding-window **transcript as the model's context** (the same
//! bytes `context show` renders) plus the prompt to an LLM, and prints the reply. "The AI reads
//! exactly what you see": no curation step, no separate context to populate — the transcript *is* the
//! context.
//!
//! Like `curl`/`wget`, `ask` is NOT a Brush `SimpleCommand`: its LLM call must run at the `Session`
//! layer where the Golem durable-execution context is live (a Brush builtin runs under clank's nested
//! `rt.block_on`, the "Wall C" shape). So `ask` is intercepted in `Session::eval_line`/`run_command`.
//!
//! The LLM transport is a Golem host dependency (`golem-ai-llm-anthropic`, which links only in the
//! wasm/agent build), but `clank-shell` is dual-target (native + wasm) and must not pull it in. So
//! this module owns only the *seam*: the [`AskProvider`] trait, the request/outcome types, leading-word
//! detection, and the manifest. The concrete durable provider lives in `clank-agent` and is injected
//! into the `Session`; on native, no provider is installed and `ask` degrades to an informative error.

use brush_parser::{tokenize_str, unquote_str, Token};

use crate::manifest::{AuthorizationPolicy, ExecutionScope, Manifest};

/// The default model `ask` targets when `--model` is not given.
pub const DEFAULT_MODEL: &str = "claude-opus-4-8";

/// A short fixed system prompt for the Core `ask` increment. The real "computed from installed tools,
/// skills, and shell configuration" builder (shared with `/proc/clank/system-prompt`) lands with the
/// tool-surface increment — see `TODO(ask-tools)`.
pub const CORE_SYSTEM_PROMPT: &str =
    "You are clank, an AI assistant embedded in a Unix-like shell. The user's shell transcript \
     (commands they ran and the output) is provided as context. Answer their question concisely.";

/// A request to an [`AskProvider`]: the assembled context plus the prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskRequest {
    /// The system prompt (see [`CORE_SYSTEM_PROMPT`]). `None` sends no system message.
    pub system: Option<String>,
    /// The base context: the rendered transcript window (empty when `--fresh`).
    pub transcript: String,
    /// Supplementary input piped to `ask`'s stdin, appended *after* the transcript. `None` for now —
    /// stdin wiring into intercepted commands is a later increment. Kept in the type so the provider
    /// contract is stable when it lands. `TODO(ask-stdin)`.
    pub stdin: Option<String>,
    /// The prompt the user typed.
    pub prompt: String,
    /// The model id to target (defaults to [`DEFAULT_MODEL`]).
    pub model: String,
}

/// A provider's response, in the same `stdout`/`stderr`/`exit_code` shape the HTTP commands use so it
/// maps directly onto `LineResult::from_outcome`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskOutcome {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: u8,
}

impl AskOutcome {
    /// A successful reply: the model's text on stdout, exit 0.
    pub fn reply(text: impl Into<Vec<u8>>) -> Self {
        Self {
            stdout: text.into(),
            stderr: Vec::new(),
            exit_code: 0,
        }
    }

    /// A remote-call failure: exit 4 (README "remote call failed") with a stderr message.
    pub fn remote_error(message: impl Into<String>) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: message.into().into_bytes(),
            exit_code: 4,
        }
    }
}

/// The LLM transport seam. Implemented in `clank-agent` by the durable Anthropic provider and injected
/// into the `Session`; absent on native. **Synchronous** — the durable provider blocks internally via
/// the Golem host, and it must be called on the agent invocation where that durable context is live
/// (i.e. from `Session::run_command`), never on a background thread.
pub trait AskProvider: Send + Sync {
    /// Send the assembled request to the model and return its reply (or an error outcome).
    fn complete(&self, request: AskRequest) -> AskOutcome;
}

/// A parsed `ask` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskArgs {
    /// The prompt text (the non-flag words, space-joined).
    pub prompt: String,
    /// The model id (`--model <id>`), or [`DEFAULT_MODEL`] if not given.
    pub model: String,
    /// `--fresh` / `--no-transcript`: send no transcript context.
    pub fresh: bool,
}

/// If `line`'s leading command word is `ask`, parse its flags and prompt. `None` for any other line.
///
/// Recognizes `--fresh` / `--no-transcript` (alias) and `--inherit` (the default, accepted for
/// explicitness) and `--model <id>`. Every other non-flag word joins the prompt. An unknown `--flag`
/// is treated as prompt text (kept simple — no error surface this increment).
pub fn classify(line: &str) -> Option<AskArgs> {
    let words = leading_words(line)?;
    let (first, rest) = words.split_first()?;
    if first != "ask" {
        return None;
    }

    let mut model = DEFAULT_MODEL.to_string();
    let mut fresh = false;
    let mut prompt_words: Vec<String> = Vec::new();
    let mut iter = rest.iter();
    while let Some(w) = iter.next() {
        match w.as_str() {
            "--fresh" | "--no-transcript" => fresh = true,
            "--inherit" => fresh = false,
            "--model" => {
                if let Some(id) = iter.next() {
                    model = id.clone();
                }
            }
            other => prompt_words.push(other.to_string()),
        }
    }

    Some(AskArgs {
        prompt: prompt_words.join(" "),
        model,
        fresh,
    })
}

/// The `Word` tokens of `line`, dequoted (quote-aware via Brush's tokenizer; operators dropped).
/// `None` if the line doesn't tokenize. Mirrors `httpcmd::leading_words`.
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

/// The `ask` manifest. `Subprocess` scope (runs isolated, no shell-state access), `Confirm` policy
/// (outbound LLM HTTP pauses for user confirmation, mirroring the README's curl/wget "Outbound HTTP →
/// confirm" rule; `sudo ask` pre-authorizes).
pub fn manifests() -> Vec<Manifest> {
    vec![
        Manifest::builtin("ask", "invoke the AI model with the shell transcript as context")
            .with_scope(ExecutionScope::Subprocess)
            .with_policy(AuthorizationPolicy::Confirm)
            .with_help(
                "ask <question> [--fresh|--no-transcript] [--inherit] [--model <id>] — send the \
                 current shell transcript plus <question> to the AI model and print the reply. \
                 --fresh sends no transcript context; --inherit (default) sends the full window; \
                 --model overrides the default model. Outbound HTTP requires confirmation.",
            ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_plain_ask_and_captures_prompt() {
        let args = classify(r#"ask "what is this repo?""#).unwrap();
        assert_eq!(args.prompt, "what is this repo?");
        assert_eq!(args.model, DEFAULT_MODEL);
        assert!(!args.fresh);
    }

    #[test]
    fn parses_fresh_and_its_alias() {
        assert!(classify("ask --fresh hello").unwrap().fresh);
        assert!(classify("ask --no-transcript hello").unwrap().fresh);
        // --inherit is the default and can be stated explicitly.
        assert!(!classify("ask --inherit hello").unwrap().fresh);
    }

    #[test]
    fn parses_model_flag() {
        let args = classify("ask --model claude-sonnet-5 summarize this").unwrap();
        assert_eq!(args.model, "claude-sonnet-5");
        assert_eq!(args.prompt, "summarize this");
    }

    #[test]
    fn flags_do_not_leak_into_the_prompt() {
        let args = classify("ask --fresh --model x tell me a joke").unwrap();
        assert_eq!(args.prompt, "tell me a joke");
        assert_eq!(args.model, "x");
        assert!(args.fresh);
    }

    #[test]
    fn non_ask_lines_are_none() {
        assert!(classify("echo ask").is_none());
        assert!(classify("cat file").is_none());
        assert!(classify("").is_none());
    }

    #[test]
    fn manifest_is_confirm_subprocess() {
        let m = &manifests()[0];
        assert_eq!(m.name, "ask");
        assert_eq!(m.authorization_policy, AuthorizationPolicy::Confirm);
        assert_eq!(m.execution_scope, ExecutionScope::Subprocess);
    }

    #[test]
    fn outcome_constructors() {
        assert_eq!(AskOutcome::reply("hi").exit_code, 0);
        assert_eq!(AskOutcome::reply("hi").stdout, b"hi");
        assert_eq!(AskOutcome::remote_error("boom").exit_code, 4);
    }
}
