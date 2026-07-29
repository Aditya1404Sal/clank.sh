//! Every word clank says to a model, in one file.
//!
//! This module holds **text only** — no logic. [`crate::ai::ask`] keeps the assembly (which blocks
//! are emitted, in what order, rendered from the live registry); this file holds what those blocks
//! say. The split exists because the system prompt is a *document* that someone must be able to read
//! and edit as prose, and before this it was interleaved with `writeln!` calls across a hundred lines
//! of `ask.rs` — you could not see the thing the model actually receives without mentally executing
//! the builder.
//!
//! It is also the review surface. The prompt is the model's entire instruction set: what tools exist,
//! which commands pause for confirmation, what it must not attempt. A change here changes agent
//! behaviour as surely as a change to the authorization gate, and it should be as easy to find.
//!
//! What is deliberately *not* here: the `help` builtin's listing and other human-facing text. Those
//! are shell UI, not model input, and mixing them would blur exactly the boundary this file draws.
//!
//! The rendered result of assembling these pieces is inspectable at runtime as
//! `/proc/clank/system-prompt` — the human view and the model's instructions are the same bytes.

// ---------------------------------------------------------------------------------------------
// System prompt — assembled by `ask::build_system_prompt` and its `_with_mcp` / `_with_capabilities`
// variants, in the order the constants appear below.
// ---------------------------------------------------------------------------------------------

/// The fixed preamble: who the model is and how the shell context works. `build_system_prompt`
/// appends the live command surface (rendered from the registry) after it.
pub const CORE_SYSTEM_PROMPT: &str =
    "You are clank, an AI assistant embedded in a Unix-like shell. The user's shell transcript \
     (commands they ran and the output) is provided as context. Answer their question concisely.";

/// Introduces the two built-in tools and the authorization markers, and opens the command listing.
/// Emitted immediately after [`CORE_SYSTEM_PROMPT`]; the per-command rows follow.
pub const TOOLS_PREAMBLE: &str =
    "\n\nYou have two tools. `shell` runs a single command line in this session and returns its \
     stdout, stderr, and exit code — use it to inspect and act on the system; compose with pipes, \
     redirects, and $(...) as needed. `prompt_user` asks the human a question and returns their \
     answer — use it to gather information or confirm intent.\n\nAvailable commands (authorization \
     in brackets; [confirm] and [sudo-only] commands pause for the user's approval unless the user \
     ran `sudo ask`, which pre-approves [confirm] commands):\n";

/// Closes the command listing: the shell-internal commands the `shell` tool cannot reach, and the
/// workarounds for the two cases (`cd`, variable assignment) a model most often gets wrong.
pub const COMMANDS_FOOTER: &str =
    "\nNotes: `context`, `cd`, `export`, `kill`, and other shell-internal commands are NOT \
     available through the `shell` tool (they mutate shell state a subprocess can't reach); to \
     reach the human, use the `prompt_user` tool, not a `prompt-user` shell line. `ask` cannot \
     call itself. To change directory or set a variable for a command, do it inside a single line \
     (e.g. `cd /tmp && ls`).";

/// Opens the installed-MCP-tools block (`build_system_prompt_with_mcp`), omitted when none are
/// installed.
pub const MCP_TOOLS_HEADER: &str =
    "\n\nInstalled MCP tools (call them by their exact tool name; they run over HTTP and \
     require confirmation unless the user ran `sudo ask`):\n";

/// Opens the installed-grease-prompts block (`build_system_prompt_with_capabilities`), omitted when
/// none are installed.
pub const PROMPT_TOOLS_HEADER: &str =
    "\n\nInstalled prompt tools (call them by their exact tool name; each runs a stored prompt \
     through the model and requires confirmation unless the user ran `sudo ask`):\n";

/// Opens the installed-skills block. Skills are context packages, not callable tools — the wording
/// has to prevent the model from trying to invoke one, which is the failure this text exists to stop.
pub const SKILLS_HEADER: &str =
    "\n\nInstalled skills (capability-context packages, not callable tools — consult the \
     skill's documents under /usr/share/skills/<name>/ and use its bundled $PATH scripts when \
     relevant):\n";

// ---------------------------------------------------------------------------------------------
// Tool definitions — name, description and JSON parameter schema for each built-in tool.
// ---------------------------------------------------------------------------------------------

/// The name of the generic shell tool the model calls to run a command line. Also the dispatch key
/// the `Session` matches on when a tool call comes back.
pub const SHELL_TOOL: &str = "shell";

/// What the `shell` tool advertises it can do.
pub const SHELL_TOOL_DESCRIPTION: &str =
    "Execute one shell command line in the clank session and return its stdout, stderr, and exit \
     code. Supports pipes, redirects, and command substitution.";

/// The JSON schema for the `shell` tool's parameters: a single required `command` string.
pub const SHELL_TOOL_SCHEMA: &str = r#"{"type":"object","properties":{"command":{"type":"string","description":"the shell command line to execute"}},"required":["command"]}"#;

/// The name of the tool the model calls to ask the human a question (the model→human back-channel).
pub const PROMPT_USER_TOOL: &str = "prompt_user";

/// What the `prompt_user` tool advertises it can do.
pub const PROMPT_USER_TOOL_DESCRIPTION: &str =
    "Ask the human user a question and get their answer. Use this to gather information you need, \
     confirm intent, or collect a missing value before proceeding. The user's typed reply is \
     returned to you.";

/// The JSON schema for the `prompt_user` tool: a required `question` string (Markdown allowed).
pub const PROMPT_USER_TOOL_SCHEMA: &str = r#"{"type":"object","properties":{"question":{"type":"string","description":"the question to put to the human; Markdown is allowed"}},"required":["question"]}"#;

// ---------------------------------------------------------------------------------------------
// The user turn — how the transcript, the question and any piped stdin are labelled for the model.
// ---------------------------------------------------------------------------------------------

/// Labels the shell transcript block in the user turn. Omitted entirely when the transcript is empty
/// (or under `--fresh`), so the model never sees an empty section it might try to explain.
pub const TRANSCRIPT_HEADER: &str = "# Shell transcript (context)";

/// Labels the user's actual question, after the transcript.
pub const QUESTION_HEADER: &str = "# Question";

/// Labels piped stdin, appended *after* the question — per the README, the transcript is the base
/// context and stdin is supplementary.
pub const STDIN_HEADER: &str = "# Piped input (stdin)";

// ---------------------------------------------------------------------------------------------
// Mode addenda — appended to the system prompt for a specific invocation shape.
// ---------------------------------------------------------------------------------------------

/// Appended to the system prompt when `ask --json` is in effect: the model's FINAL answer must be one
/// valid JSON value and nothing else. The `Session` still validates the output and enforces the
/// exit-6 contract — this only makes the happy path reliable.
pub const JSON_SYSTEM_ADDENDUM: &str =
    "\n\nOUTPUT FORMAT: Your final answer MUST be a single valid JSON value (object, array, string, \
     number, boolean, or null) and NOTHING else — no prose, no explanation, no Markdown code fences. \
     Emit only the JSON.";

/// The whole system prompt for `context summarize`: a narrow, tool-less summarization instruction.
/// Nothing above it applies — summarize never calls tools, so it gets no command surface. The
/// transcript is passed as the single user turn's content.
pub const SUMMARIZE_SYSTEM_PROMPT: &str =
    "You are summarizing a shell session transcript. Produce a concise plain-prose summary of what the \
     user did, the key command outputs, and the current state of the session. No preamble, no \
     bullet-point boilerplate, no Markdown headers — just the summary.";
