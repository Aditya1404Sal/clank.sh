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
use crate::registry::CommandRegistry;

/// The default model `ask` targets when `--model` is not given and no ask.toml default is set.
/// Deliberately the lightest/cheapest model — callers opt into a bigger model explicitly.
pub const DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";

/// The fixed preamble of the `ask` system prompt: who the model is and how the shell context works.
/// [`build_system_prompt`] appends the live command surface (rendered from the registry) after it.
pub const CORE_SYSTEM_PROMPT: &str =
    "You are clank, an AI assistant embedded in a Unix-like shell. The user's shell transcript \
     (commands they ran and the output) is provided as context. Answer their question concisely.";

/// The system prompt for `context summarize`: a narrow, tool-less summarization instruction. Distinct
/// from [`build_system_prompt`] (no command surface — summarize never calls tools). The transcript is
/// passed as the single user turn's content.
pub const SUMMARIZE_SYSTEM_PROMPT: &str =
    "You are summarizing a shell session transcript. Produce a concise plain-prose summary of what the \
     user did, the key command outputs, and the current state of the session. No preamble, no \
     bullet-point boilerplate, no Markdown headers — just the summary.";

/// The name of the generic shell tool the model calls to run a command line.
pub const SHELL_TOOL: &str = "shell";

/// The name of the tool the model calls to ask the human a question (the model→human back-channel).
pub const PROMPT_USER_TOOL: &str = "prompt_user";

/// The JSON schema for the `shell` tool's parameters: a single required `command` string.
const SHELL_TOOL_SCHEMA: &str = r#"{"type":"object","properties":{"command":{"type":"string","description":"the shell command line to execute"}},"required":["command"]}"#;

/// The JSON schema for the `prompt_user` tool: a required `question` string (Markdown allowed).
const PROMPT_USER_TOOL_SCHEMA: &str = r#"{"type":"object","properties":{"question":{"type":"string","description":"the question to put to the human; Markdown is allowed"}},"required":["question"]}"#;

/// The tool definitions exposed to the model each turn: the generic [`SHELL_TOOL`] (executes a command
/// line in the clank session — pipes/redirects/`$(...)` come free) and [`PROMPT_USER_TOOL`] (the
/// model→human back-channel; pauses the loop for an answer). Per-command and MCP tools arrive later.
/// Takes the registry so future increments can derive tools from it without a signature change.
pub fn build_ask_tools(_registry: &CommandRegistry) -> Vec<AskTool> {
    vec![
        AskTool {
            name: SHELL_TOOL.to_string(),
            description: "Execute one shell command line in the clank session and return its \
                          stdout, stderr, and exit code. Supports pipes, redirects, and command \
                          substitution."
                .to_string(),
            parameters_schema: SHELL_TOOL_SCHEMA.to_string(),
        },
        AskTool {
            name: PROMPT_USER_TOOL.to_string(),
            description: "Ask the human user a question and get their answer. Use this to gather \
                          information you need, confirm intent, or collect a missing value before \
                          proceeding. The user's typed reply is returned to you."
                .to_string(),
            parameters_schema: PROMPT_USER_TOOL_SCHEMA.to_string(),
        },
    ]
}

/// Build the `ask` system prompt: the fixed [`CORE_SYSTEM_PROMPT`] preamble plus the live command
/// surface rendered from the registry. Only `Subprocess`-scoped commands are listed — per the README,
/// `shell-internal`/`parent-shell` commands are not part of the model's tool surface (they mutate
/// shell state a subprocess can't reach). Each line carries an authorization marker so the model knows
/// which commands run freely vs. which come back needing confirmation.
///
/// Shared with `/proc/clank/system-prompt` (inspectable at runtime) so the model's instructions and
/// the user-visible view are the same bytes.
pub fn build_system_prompt(registry: &CommandRegistry) -> String {
    let mut out = String::from(CORE_SYSTEM_PROMPT);
    out.push_str(
        "\n\nYou have two tools. `shell` runs a single command line in this session and returns its \
         stdout, stderr, and exit code — use it to inspect and act on the system; compose with pipes, \
         redirects, and $(...) as needed. `prompt_user` asks the human a question and returns their \
         answer — use it to gather information or confirm intent.\n\nAvailable commands (authorization \
         in brackets; [confirm] and [sudo-only] commands pause for the user's approval unless the user \
         ran `sudo ask`, which pre-approves [confirm] commands):\n",
    );

    let mut rows: Vec<(&str, &str, &str)> = registry
        .iter()
        .filter(|m| m.execution_scope == ExecutionScope::Subprocess)
        .map(|m| {
            let marker = match m.authorization_policy {
                AuthorizationPolicy::Allow => "",
                AuthorizationPolicy::Confirm => " [confirm]",
                AuthorizationPolicy::SudoOnly => " [sudo-only]",
            };
            (m.name.as_str(), m.synopsis.as_str(), marker)
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    for (name, synopsis, marker) in rows {
        out.push_str(&format!("  {name} — {synopsis}{marker}\n"));
    }

    out.push_str(
        "\nNotes: `context`, `cd`, `export`, `kill`, and other shell-internal commands are NOT \
         available through the `shell` tool (they mutate shell state a subprocess can't reach); to \
         reach the human, use the `prompt_user` tool, not a `prompt-user` shell line. `ask` cannot \
         call itself. To change directory or set a variable for a command, do it inside a single line \
         (e.g. `cd /tmp && ls`).",
    );
    out
}

/// Like [`build_system_prompt`] but also lists installed MCP tools (each exposed as its own
/// `mcp__<server>__<tool>` tool). `mcp_tools` is [`crate::mcpstate::McpState::ask_tool_definitions`].
pub fn build_system_prompt_with_mcp(
    registry: &CommandRegistry,
    mcp: &crate::mcpstate::McpState,
) -> String {
    let mut out = build_system_prompt(registry);
    append_mcp_tools(&mut out, mcp);
    out
}

/// The full agentic system prompt: the command surface + installed MCP tools + installed grease
/// prompts. Each installed prompt is a `prompt__<name>` tool the model can call. Shared with
/// `/proc/clank/system-prompt` so the human-visible view matches what the model sees.
pub fn build_system_prompt_with_capabilities(
    registry: &CommandRegistry,
    mcp: &crate::mcpstate::McpState,
    grease: &crate::greasestate::GreaseState,
) -> String {
    let mut out = build_system_prompt(registry);
    append_mcp_tools(&mut out, mcp);
    let prompts = grease.ask_tool_definitions();
    if !prompts.is_empty() {
        out.push_str(
            "\n\nInstalled prompt tools (call them by their exact tool name; each runs a stored prompt \
             through the model and requires confirmation unless the user ran `sudo ask`):\n",
        );
        for t in &prompts {
            out.push_str(&format!("  {} — {}\n", t.name, t.description));
        }
    }
    let skills = grease.skills();
    if !skills.is_empty() {
        out.push_str(
            "\n\nInstalled skills (capability-context packages, not callable tools — consult the \
             skill's documents under /usr/share/skills/<name>/ and use its bundled $PATH scripts when \
             relevant):\n",
        );
        for s in &skills {
            let intended = s
                .intended_use
                .as_deref()
                .map(|u| format!(" (use when: {u})"))
                .unwrap_or_default();
            out.push_str(&format!("  {} — {}{intended}\n", s.name, s.description));
        }
    }
    out
}

/// Append the installed-MCP-tools block to `out` (shared by both system-prompt builders).
fn append_mcp_tools(out: &mut String, mcp: &crate::mcpstate::McpState) {
    let mcp_tools = mcp.ask_tool_definitions();
    if !mcp_tools.is_empty() {
        out.push_str(
            "\n\nInstalled MCP tools (call them by their exact tool name; they run over HTTP and \
             require confirmation unless the user ran `sudo ask`):\n",
        );
        for t in &mcp_tools {
            out.push_str(&format!("  {} — {}\n", t.name, t.description));
        }
    }
}

/// One tool the model may call, rendered from the manifest registry. The clank-neutral mirror of
/// `golem-ai-llm`'s `ToolDefinition` — the `clank-agent` provider converts it at the seam so
/// `clank-shell` need not link the golem crates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskTool {
    /// The tool name the model calls (e.g. `shell`).
    pub name: String,
    /// Human/model-facing description of what the tool does.
    pub description: String,
    /// The tool's parameter schema, as a JSON-schema string.
    pub parameters_schema: String,
}

/// A tool call the model requested in a turn. Mirrors `golem-ai-llm`'s `ToolCall`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskToolCall {
    /// Provider-assigned call id, echoed back in the matching [`AskToolResult`].
    pub id: String,
    /// The tool name the model wants to run.
    pub name: String,
    /// The call arguments as a JSON string (always valid JSON — the provider requires it to
    /// round-trip through the Anthropic `tool_use` block, which parses it).
    pub arguments_json: String,
}

/// The result of executing a tool call, fed back to the model on the next turn. Mirrors
/// `golem-ai-llm`'s `ToolResult` (`Success`/`Error`) collapsed to a `Result`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskToolResult {
    /// The [`AskToolCall::id`] this answers.
    pub id: String,
    /// The tool name (echoed for the provider's `ToolSuccess`/`ToolFailure`).
    pub name: String,
    /// `Ok(result_json)` when the tool ran (even on a nonzero exit — the payload carries the code);
    /// `Err(message)` only for a guard/authorization refusal the tool never executed.
    pub outcome: Result<String, String>,
}

/// One entry of the running conversation the `Session` maintains across turns of the agentic loop.
/// The `Session` owns the multi-turn state; the provider is a single-turn transport (see
/// [`AskProvider::turn`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskTurn {
    /// The initial user message (the system prompt is passed separately to [`AskProvider::turn`]).
    User(String),
    /// The model's prior response: its text plus any tool calls it requested.
    Assistant {
        text: String,
        tool_calls: Vec<AskToolCall>,
    },
    /// The results the `Session` computed for the previous turn's tool calls.
    ToolResults(Vec<AskToolResult>),
}

/// What the model returned in a single turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskResponse {
    /// The model's text this turn (may be empty when it only made tool calls).
    pub text: String,
    /// The tool calls the model requested (empty ⇒ this is the terminal turn).
    pub tool_calls: Vec<AskToolCall>,
    /// Whether the finish reason was `ToolCalls` (informational; the loop drives off `tool_calls`).
    pub finished_for_tools: bool,
    /// Set on a transport error ⇒ the loop aborts with a `remote_error` outcome.
    pub error: Option<String>,
}

impl AskResponse {
    /// A terminal text-only response.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tool_calls: Vec::new(),
            finished_for_tools: false,
            error: None,
        }
    }

    /// A transport-failure response: the loop aborts and maps this to `AskOutcome::remote_error`.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            text: String::new(),
            tool_calls: Vec::new(),
            finished_for_tools: false,
            error: Some(message.into()),
        }
    }
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
/// into the `Session`; absent on native. **Async** — `golem-ai-llm`'s `LlmProvider::send` is an
/// `async fn`, so `turn` is too; it is awaited from `Session::run_ask` (under `run_command`), one level
/// under the Golem SDK's `wstd::block_on` where the durable-execution context and the WASI-HTTP reactor
/// are live (the same "await at the Session layer, never through the nested Brush runtime" rule as
/// curl/wget).
///
/// The provider is a **single-turn transport**: it sends one turn of the conversation (system +
/// history + tool definitions) and returns the model's response. The `Session` drives the multi-turn
/// agentic loop — it owns the shell state a tool call executes against, so the loop cannot live behind
/// this seam without leaking clank types into the golem crates (and vice versa). Each `turn` maps to
/// one `DurableAnthropic::send`, i.e. one durable oplog op, replay-safe.
///
/// Uses `#[async_trait(?Send)]` so it stays `dyn`-compatible as `Box<dyn AskProvider>` (a plain
/// `async fn` in a trait is not object-safe); `?Send` because wasip2 is single-threaded, matching how
/// `golem-ai-llm`'s own `ChatStreamInterface` is declared.
#[async_trait::async_trait(?Send)]
pub trait AskProvider {
    /// Send one turn of the conversation to the model and return its response.
    ///
    /// `system` is the system prompt (`None` sends none); `history` is the conversation so far (the
    /// first entry is the user's [`AskTurn::User`], then alternating `Assistant`/`ToolResults`);
    /// `tools` are the tool definitions the model may call this turn; `model` is the target id.
    async fn turn(
        &self,
        system: Option<&str>,
        history: &[AskTurn],
        tools: &[AskTool],
        model: &str,
    ) -> AskResponse;
}

/// A decorator that logs each LLM turn to `http.log` (the outbound Anthropic call) then delegates. The
/// prompt/response bodies are NOT logged (they can be large and carry user content already in the
/// transcript); only the model, tool count, and outcome are recorded. Wrapping the injected provider in
/// the portable core keeps emission testable with the native fake and out of the wasm-only impl.
pub struct LoggingAskProvider {
    inner: Box<dyn AskProvider>,
}

impl LoggingAskProvider {
    pub fn new(inner: Box<dyn AskProvider>) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait(?Send)]
impl AskProvider for LoggingAskProvider {
    async fn turn(
        &self,
        system: Option<&str>,
        history: &[AskTurn],
        tools: &[AskTool],
        model: &str,
    ) -> AskResponse {
        let resp = self.inner.turn(system, history, tools, model).await;
        let rec = crate::logging::Record::new("http")
            .field("kind", "llm")
            .field("model", model)
            .field("tools", tools.len().to_string());
        let rec = match &resp.error {
            Some(e) => rec.field("status", "error").field("error", e),
            None => rec.field("status", "ok"),
        };
        rec.emit(crate::logging::LogFile::Http);
        resp
    }
}

/// A parsed `ask` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskArgs {
    /// The prompt text (the non-flag words, space-joined).
    pub prompt: String,
    /// The model id from `--model <id>`, or `None` to fall back to the ask.toml default / built-in.
    /// The `Session` resolves the final id (`--model` > ask.toml > [`DEFAULT_MODEL`]).
    pub model: Option<String>,
    /// `--fresh` / `--no-transcript`: send no transcript context.
    pub fresh: bool,
    /// `--json`: the model's final answer must be a single valid JSON value. The `Session` validates
    /// it — valid JSON on stdout (exit 0), or exit 6 with the raw response on stderr (README's
    /// `--json` contract). Also instructs the model to emit JSON and nothing else.
    pub json: bool,
    /// Bytes piped into `ask` as supplementary context (`cat x | ask "…"`), decoded lossily. The
    /// `Session` sets this by pre-extracting the upstream pipeline stage; it is appended to the
    /// first user message after the transcript. `None` when nothing was piped.
    pub stdin: Option<String>,
}

/// If `line`'s leading command word is `ask`, parse its flags and prompt. `None` for any other line.
///
/// Recognizes `--fresh` / `--no-transcript` (alias) and `--inherit` (the default, accepted for
/// explicitness), `--model <id>`, and `--json` (validated-JSON output contract). Every other non-flag
/// word joins the prompt. An unknown `--flag` is treated as prompt text (kept simple — no error
/// surface this increment). `stdin` is always `None` here — the `Session` fills it when `ask` is the
/// tail of a pipeline.
pub fn classify(line: &str) -> Option<AskArgs> {
    let words = leading_words(line)?;
    let (first, rest) = words.split_first()?;
    if first != "ask" {
        return None;
    }

    let mut model: Option<String> = None;
    let mut fresh = false;
    let mut json = false;
    let mut prompt_words: Vec<String> = Vec::new();
    let mut iter = rest.iter();
    while let Some(w) = iter.next() {
        match w.as_str() {
            "--fresh" | "--no-transcript" => fresh = true,
            "--inherit" => fresh = false,
            "--json" => json = true,
            "--model" => {
                if let Some(id) = iter.next() {
                    model = Some(id.clone());
                }
            }
            other => prompt_words.push(other.to_string()),
        }
    }

    Some(AskArgs {
        prompt: prompt_words.join(" "),
        model,
        fresh,
        json,
        stdin: None,
    })
}

/// How an `ask repl` session seeds its isolated transcript on start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplSeed {
    /// `--fresh`: empty transcript (the model sees only what's typed in the REPL).
    Fresh,
    /// `--inherit`: the full parent transcript is copied in as the REPL's starting context.
    Inherit,
}

/// A parsed `ask repl` invocation: an interactive AI session with its OWN isolated transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplArgs {
    /// The model id from `--model <id>`, or `None` for the resolved default.
    pub model: Option<String>,
    /// How the REPL seeds its transcript on start.
    pub seed: ReplSeed,
}

/// If `line` is `ask repl [--fresh|--inherit] [--model <id>]`, parse it. `None` otherwise (including
/// a plain `ask …` — that's [`classify`]). The default seed is `Fresh`; the README's summary-injection
/// default is not yet wired for `repl`.
pub fn classify_repl(line: &str) -> Option<ReplArgs> {
    let words = leading_words(line)?;
    let mut iter = words.iter();
    if iter.next()?.as_str() != "ask" {
        return None;
    }
    if iter.next()?.as_str() != "repl" {
        return None;
    }
    let mut model = None;
    let mut seed = ReplSeed::Fresh;
    while let Some(w) = iter.next() {
        match w.as_str() {
            "--fresh" | "--no-transcript" => seed = ReplSeed::Fresh,
            "--inherit" => seed = ReplSeed::Inherit,
            "--model" => {
                if let Some(id) = iter.next() {
                    model = Some(id.clone());
                }
            }
            _ => {} // stray words after `repl` are ignored (no prompt on the repl start line)
        }
    }
    Some(ReplArgs { model, seed })
}

/// Build the first user message body: the transcript window (if any) as context, then the prompt.
/// Shared by the `Session` loop (which constructs the initial [`AskTurn::User`]); kept here so the
/// transcript-as-context shaping lives beside the rest of the `ask` seam.
pub fn user_content(transcript: &str, prompt: &str) -> String {
    user_content_with_stdin(transcript, prompt, None)
}

/// Like [`user_content`] but also appends piped stdin as a clearly-delimited supplementary block
/// AFTER the prompt (README: "the transcript is the base context, stdin is appended after it").
/// `--fresh` drops the transcript but keeps this block — piped input is explicit, not ambient.
pub fn user_content_with_stdin(transcript: &str, prompt: &str, stdin: Option<&str>) -> String {
    let mut out = if transcript.is_empty() {
        prompt.to_string()
    } else {
        format!("# Shell transcript (context)\n{transcript}\n# Question\n{prompt}")
    };
    if let Some(input) = stdin {
        out.push_str(&format!("\n# Piped input (stdin)\n{input}"));
    }
    out
}

/// The JSON-mode addendum appended to the system prompt when `ask --json` is in effect: the model's
/// FINAL answer must be one valid JSON value and nothing else. The `Session` still validates the
/// output and enforces the exit-6 contract, but the instruction makes the happy path reliable.
pub const JSON_SYSTEM_ADDENDUM: &str =
    "\n\nOUTPUT FORMAT: Your final answer MUST be a single valid JSON value (object, array, string, \
     number, boolean, or null) and NOTHING else — no prose, no explanation, no Markdown code fences. \
     Emit only the JSON.";

/// Append [`JSON_SYSTEM_ADDENDUM`] to `system` when `json` is set; return `system` unchanged
/// otherwise. Kept out of [`build_system_prompt`] so `/proc/clank/system-prompt` (the human-facing,
/// non-JSON view) is unaffected.
pub fn with_json_addendum(system: String, json: bool) -> String {
    if json {
        system + JSON_SYSTEM_ADDENDUM
    } else {
        system
    }
}

/// Extract a JSON value from a model's final text under `--json`. Strips a single leading/trailing
/// Markdown code fence (```json … ``` or ``` … ```) that small models tend to wrap around JSON, then
/// returns the trimmed body. The caller parses the result; this only unwraps the common fence case.
pub fn strip_json_fence(text: &str) -> &str {
    let t = text.trim();
    let Some(inner) = t.strip_prefix("```") else {
        return t;
    };
    // Drop an optional language tag on the opening fence line (```json), then the trailing fence.
    let inner = inner.strip_prefix("json").unwrap_or(inner);
    let inner = inner.trim_start_matches(['\n', '\r']);
    inner.strip_suffix("```").map(str::trim).unwrap_or(t)
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

/// The dequoted words of a **top-level** (operator-free) `line`. `None` if the line doesn't tokenize,
/// is empty, OR contains any shell operator (`|`/`;`/`&&`/redirects) — so a nested use falls through
/// to Brush (and its honest stub). Used by grease's prompt dispatch (a prompt can't run in a pipe/`$()`
/// — it makes an LLM call, the Wall-C wall). Public sibling of [`leading_words`].
pub fn dequote_words(line: &str) -> Option<Vec<String>> {
    let tokens = tokenize_str(line).ok()?;
    if tokens.iter().any(|t| matches!(t, Token::Operator(_, _))) {
        return None;
    }
    let words: Vec<String> = tokens
        .into_iter()
        .filter_map(|t| match t {
            Token::Word(s, _) => Some(unquote_str(&s)),
            Token::Operator(_, _) => None,
        })
        .collect();
    (!words.is_empty()).then_some(words)
}

/// The upstream + parsed `ask` tail of an ask-tail pipeline (`… | ask "q"`). `elevated` reflects a
/// `sudo` on the tail stage (`… | sudo ask "q"`), which pre-authorizes the ask like a top-level
/// `sudo ask`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskTailPipe {
    /// Everything before the last top-level `|` — the producer whose stdout becomes `ask`'s stdin.
    pub upstream: String,
    /// The parsed `ask` invocation (its `stdin` is `None`; the `Session` fills it from the capture).
    pub args: AskArgs,
    /// Whether the tail carried `sudo` (blanket confirm-tier authorization for the ask).
    pub elevated: bool,
}

/// If `line` is a pipeline whose FINAL stage is `ask` (`… | ask "q"` or `… | sudo ask "q"`), split it
/// into the upstream (everything before the last top-level `|`) and the parsed `ask` tail. Returns
/// `None` when the line isn't such a pipeline: a bare `ask`, no `ask` tail, or a non-`|` operator
/// between stages.
///
/// The split is byte-exact via the tokenizer's source spans (quoting in the upstream survives), and
/// only a literal `|` pipe operator counts — `||`/`|&`/`&&`/`;` are not pipe boundaries, so an `ask`
/// after them is not a pipe tail (and stays the honest stub error). This is how `cat x | ask "…"`
/// reaches the model: the `Session` runs the upstream, captures its stdout, and dispatches the tail
/// at the session layer with those bytes as stdin (the LLM call can't run inside Brush's pipeline).
pub fn split_ask_tail(line: &str) -> Option<AskTailPipe> {
    let tokens = tokenize_str(line).ok()?;
    // The byte index just past the last top-level `|` operator (word tokens after it are the tail).
    let mut last_pipe_end: Option<usize> = None;
    for t in &tokens {
        if let Token::Operator(op, span) = t {
            if op == "|" {
                last_pipe_end = Some(span.end.index);
            }
        }
    }
    let pipe_end = last_pipe_end?;
    // Guard against a byte index that isn't a char boundary (defensive; spans are byte offsets).
    if pipe_end > line.len() || !line.is_char_boundary(pipe_end) {
        return None;
    }
    let (upstream, tail) = line.split_at(pipe_end);
    // A `sudo` on the tail elevates the ask; strip it before classifying.
    let tail_trimmed = tail.trim_start();
    let (tail_effective, elevated) = match tail_trimmed.strip_prefix("sudo") {
        Some(rest) if rest.starts_with(char::is_whitespace) => (rest.trim_start(), true),
        _ => (tail_trimmed, false),
    };
    let args = classify(tail_effective)?; // the tail must itself be an `ask` invocation
    let upstream = upstream.trim_end().trim_end_matches('|').trim_end().to_string();
    if upstream.is_empty() {
        return None; // `| ask` with no producer — not a real pipeline
    }
    Some(AskTailPipe { upstream, args, elevated })
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
                "ask <question> [--fresh|--no-transcript] [--inherit] [--model <id>] [--json] — send \
                 the current shell transcript plus <question> to the AI model and print the reply. \
                 --fresh sends no transcript context; --inherit (default) sends the full window; \
                 --model overrides the default model; --json requires the model's answer to be valid \
                 JSON (printed on stdout, or exit 6 with the raw response on stderr). Piped input \
                 (cat x | ask \"…\") is sent to the model as supplementary context after the \
                 transcript. Outbound HTTP requires confirmation.",
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
        // No --model given: the Session resolves the default (ask.toml / built-in).
        assert_eq!(args.model, None);
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
        assert_eq!(args.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(args.prompt, "summarize this");
    }

    #[test]
    fn flags_do_not_leak_into_the_prompt() {
        let args = classify("ask --fresh --model x tell me a joke").unwrap();
        assert_eq!(args.prompt, "tell me a joke");
        assert_eq!(args.model.as_deref(), Some("x"));
        assert!(args.fresh);
        assert!(!args.json);
    }

    #[test]
    fn parses_json_flag() {
        let args = classify("ask --json list the causes").unwrap();
        assert!(args.json);
        assert_eq!(args.prompt, "list the causes");
        // Not set by default.
        assert!(!classify("ask hello").unwrap().json);
        // stdin is never populated by classify (the Session fills it for a pipeline tail).
        assert!(args.stdin.is_none());
    }

    #[test]
    fn json_addendum_only_applies_under_json() {
        let base = "SYSTEM".to_string();
        assert_eq!(with_json_addendum(base.clone(), false), "SYSTEM");
        assert!(with_json_addendum(base, true).contains("single valid JSON value"));
    }

    #[test]
    fn strip_json_fence_unwraps_common_forms() {
        // Plain JSON is returned trimmed.
        assert_eq!(strip_json_fence("  {\"a\":1}  "), "{\"a\":1}");
        // ```json … ``` fence.
        assert_eq!(strip_json_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        // bare ``` … ``` fence.
        assert_eq!(strip_json_fence("```\n[1,2]\n```"), "[1,2]");
        // An opening fence with no closing fence falls back to the original trimmed text.
        assert_eq!(strip_json_fence("```json\n{\"a\":1}"), "```json\n{\"a\":1}");
    }

    #[test]
    fn classify_repl_detects_and_parses() {
        let r = classify_repl("ask repl").unwrap();
        assert_eq!(r.seed, ReplSeed::Fresh); // default
        assert_eq!(r.model, None);

        let r = classify_repl("ask repl --inherit --model claude-sonnet-5").unwrap();
        assert_eq!(r.seed, ReplSeed::Inherit);
        assert_eq!(r.model.as_deref(), Some("claude-sonnet-5"));

        let r = classify_repl("ask repl --fresh").unwrap();
        assert_eq!(r.seed, ReplSeed::Fresh);
    }

    #[test]
    fn classify_repl_rejects_non_repl() {
        // Plain ask is not a repl.
        assert!(classify_repl(r#"ask "what""#).is_none());
        // `repl` as a prompt word (not the subcommand) — only `ask repl` as the leading two words.
        assert!(classify_repl("echo ask repl").is_none());
        assert!(classify_repl("").is_none());
        // But plain `ask repl …` IS a repl, and a plain ask with a "repl"-containing prompt is not.
        assert!(classify_repl(r#"ask "tell me about repl loops""#).is_none());
    }

    #[test]
    fn split_ask_tail_detects_pipe_tail() {
        // Basic: cat x | ask "q" → upstream "cat x", tail is an ask with the prompt.
        let p = split_ask_tail(r#"cat x | ask "summarize this""#).unwrap();
        assert_eq!(p.upstream, "cat x");
        assert_eq!(p.args.prompt, "summarize this");
        assert!(p.args.stdin.is_none()); // classify never fills stdin; the Session does
        assert!(!p.elevated);

        // Flags on the tail survive.
        let p = split_ask_tail(r#"echo hi | ask --json --fresh "q""#).unwrap();
        assert_eq!(p.upstream, "echo hi");
        assert!(p.args.json && p.args.fresh);

        // Multi-stage upstream: the LAST pipe is the boundary; upstream keeps its inner pipe.
        let p = split_ask_tail(r#"cat a | grep x | ask "q""#).unwrap();
        assert_eq!(p.upstream, "cat a | grep x");
        assert_eq!(p.args.prompt, "q");

        // `sudo` on the tail elevates and is stripped from the parsed ask.
        let p = split_ask_tail(r#"cat x | sudo ask "q""#).unwrap();
        assert_eq!(p.upstream, "cat x");
        assert!(p.elevated);
        assert_eq!(p.args.prompt, "q");
    }

    #[test]
    fn split_ask_tail_rejects_non_tails() {
        // Bare ask (no pipe) — not a pipe tail.
        assert!(split_ask_tail(r#"ask "q""#).is_none());
        // ask is NOT the tail.
        assert!(split_ask_tail(r#"ask "q" | cat"#).is_none());
        // A non-pipe operator before ask is not a pipe boundary.
        assert!(split_ask_tail(r#"echo hi || ask "q""#).is_none());
        assert!(split_ask_tail(r#"echo hi ; ask "q""#).is_none());
        // A `|` inside quotes is not a top-level pipe.
        assert!(split_ask_tail(r#"echo "a | ask b""#).is_none());
        // `| ask` with no producer.
        assert!(split_ask_tail(r#"| ask "q""#).is_none());
        // Not an ask line at all.
        assert!(split_ask_tail(r#"cat x | grep y"#).is_none());
    }

    #[test]
    fn user_content_appends_stdin_block() {
        let c = user_content_with_stdin("", "summarize", Some("log line one"));
        assert!(c.contains("# Piped input (stdin)"));
        assert!(c.contains("log line one"));
        assert!(c.contains("summarize"));
        // With a transcript, stdin still comes AFTER the question.
        let c2 = user_content_with_stdin("prior history", "q", Some("piped"));
        let q = c2.find("# Question").unwrap();
        let s = c2.find("# Piped input").unwrap();
        assert!(q < s, "stdin block must follow the question");
        // No stdin ⇒ identical to user_content.
        assert_eq!(user_content_with_stdin("t", "p", None), user_content("t", "p"));
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
