//! Cross-cutting tunables: the numbers that bound clank's behaviour in one reviewable place.
//!
//! Scope rule: a constant belongs here when it is a **policy knob** — a bound, budget or deadline
//! someone might reasonably want to reason about or change together with its siblings. A constant
//! stays with its implementation when it is part of that item's **identity** (a tool's `NAME` and
//! `SYNOPSIS`, a protocol's version string, a filesystem path a single module owns).
//!
//! Bounds matter more here than in an ordinary program: clank runs as a durable Golem agent that
//! never restarts, and Golem serializes invocations per instance — so an unbounded wait doesn't just
//! stall one command, it wedges everything queued behind it.

use std::time::Duration;

/// Outbound HTTP deadlines.
///
/// These apply to the transports that do **not** go through `whttp` (which carries its own matching
/// defaults): the LLM providers, the MCP clients, and the Golem REST client. Every one of them
/// previously built a `reqwest`/`wstd` client with no timeout at all, so a black-holing endpoint
/// parked the invocation forever.
pub mod net {
    use super::Duration;

    /// Bound on establishing a connection.
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

    /// Bound on an ordinary request/response exchange (MCP, registry, Golem REST).
    pub const REQUEST_TIMEOUT: Duration = Duration::from_mins(2);

    /// Bound on a single model call. Longer than [`REQUEST_TIMEOUT`] because a large completion
    /// legitimately takes minutes; still finite, because "wait forever" is never the right answer on
    /// an agent that cannot be restarted.
    pub const LLM_TIMEOUT: Duration = Duration::from_mins(5);
}

/// Reject a URL that would carry package payloads or tool traffic over cleartext.
///
/// The README promises "MCP server tools (HTTPS only)", but `grease registry add` and `mcp add`
/// accepted any string — so `grease registry add http://attacker` was one confirmed `grease install`
/// away from an attacker-authored script on `$PATH`, with the transport offering no integrity at all
/// underneath the signature checks.
///
/// **Loopback is allowed.** `http://localhost:<port>` is the documented workflow for the bundled
/// registry authoring tool (`grease-populate` serves on `127.0.0.1`), and traffic that never leaves
/// the machine has no meaningful interception surface.
///
/// # Errors
/// Returns the REASON as text, not a typed error: this is a shared validator, and its two callers
/// (`grease registry add`, `mcp add`) each embed the reason in their own subsystem error. A type
/// here would have to belong to neither subsystem, which is worse than a string that is only ever
/// interpolated.
pub fn require_secure_url(url: &str) -> Result<(), String> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(format!("'{url}' is not an absolute http(s) URL"));
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@') // skip any userinfo
        .next()
        .unwrap_or("");
    let bare = host.rsplit_once(':').map_or(host, |(h, _port)| h);
    let is_loopback = matches!(bare, "localhost" | "127.0.0.1" | "::1" | "[::1]");
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Ok(()),
        "http" if is_loopback => Ok(()),
        "http" => Err(format!(
            "'{url}' uses plain http; use https (loopback is exempt for local registries)"
        )),
        other => Err(format!("'{url}' uses unsupported scheme '{other}'")),
    }
}

/// Defaults for the model `ask` talks to.
///
/// These were duplicated across the three provider implementations — the native Anthropic client, the
/// native OpenAI-compatible client, and the durable golem-ai-llm provider in `clank-agent` — each
/// with a comment claiming it "matches" the others. A comment is not a mechanism; this module is.
pub mod model {
    /// The model `ask` targets when `--model` is not given and `ask.toml` sets no default.
    /// Deliberately the lightest/cheapest model — callers opt into a bigger one explicitly.
    pub const DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";

    /// The provider assumed for a bare model id (one with no `<provider>/` prefix).
    ///
    /// Distinct from a provider *backend's* own name: `ai::anthropic_native` keeps its own
    /// `PROVIDER` const because that is its identity — it must keep looking up the `anthropic` key
    /// even if the default provider here were changed to something else.
    pub const DEFAULT_PROVIDER: &str = "anthropic";

    /// Output-token ceiling for one `ask` reply, sent on every provider request.
    pub const MAX_TOKENS: u32 = 4096;
}

/// Bounds on the work a single command can be made to do.
///
/// These exist because the shell is driven by a model, not only by a person. A malformed format
/// string or a pathologically nested expression is a routine LLM emission, and on the agent the
/// failure mode for "allocate 10 GB" or "recurse until the stack ends" is a trap, not an error
/// message — so each of these turns an unbounded operation into a refusal.
pub mod limits {
    /// Default transcript safety cap, in estimated tokens. Sized so an ordinary session accumulates
    /// freely but a runaway one is bounded before it would blow an LLM context window. A fixed
    /// safety limit, not a user-tunable knob: set once at session construction from
    /// [`crate::configured_context_cap`], with no `context` subcommand to change it at runtime.
    pub const DEFAULT_CONTEXT_CAP_TOKENS: usize = 24_000;

    /// Cap on the buffer holding evicted transcript text awaiting an async summary.
    ///
    /// On a durable agent whose summarizer never succeeds (no provider, no API key, a persistent
    /// egress error) this grows without limit as entries are compacted — an unbounded leak that
    /// `context clear` does not reclaim. The most recent bytes are kept.
    pub const LAST_DROPPED_CAP: usize = 128 * 1024;

    /// The most tool-calling turns the agentic `ask` loop will drive before giving up.
    pub const ASK_MAX_ITERATIONS: usize = 40;

    /// Per-stream byte cap on a tool result fed back to the model. Bounds context growth from a
    /// `cat` of a large file; the payload is truncated with a marker, the JSON envelope is not.
    pub const ASK_TOOL_RESULT_CAP: usize = 16 * 1024;

    /// Cap on pending (`--trigger`ed, not yet awaited) Golem agent invocations a session tracks.
    pub const MAX_PENDING_INVOCATIONS: usize = 64;

    /// Cap on rows in the virtual process table backing `/proc` and `jobs`.
    pub const MAX_PROC_ROWS: usize = 512;

    /// Cap on concurrently-open MCP sessions retained by a session.
    ///
    /// The table had no bound and nothing reaped it, on an instance that never restarts.
    pub const MAX_MCP_SESSIONS: usize = 64;

    /// Cap on `tools/list` pages followed when enumerating an MCP server's tools. A server that
    /// paginates forever must not be able to park the invocation.
    pub const MAX_TOOL_PAGES: usize = 16;

    /// Byte ceiling on a log file's retained tail. Past this, the file is rewritten to its last
    /// whole lines rather than appended to — the agent's rolling window and the native rotation
    /// threshold are the same number.
    pub const MAX_LOG_BYTES: usize = 256 * 1024;

    /// Byte ceiling on an MCP HTTP response body. Distinct from `whttp`'s own `DEFAULT_MAX_BODY`
    /// (that crate is standalone and carries its own defaults): MCP replies are JSON-RPC envelopes
    /// destined for a model's context, so they are held to a much tighter bound than a `curl` fetch.
    pub const MAX_HTTP_BODY: usize = 4 * 1024 * 1024;

    /// The most continuation lines an unterminated construct (open quote, heredoc, trailing `\`)
    /// may accumulate before the native REPL abandons it. Without this a stray quote turns the
    /// prompt into a silent sink that never returns to `clank$`.
    pub const MAX_CONTINUATION_LINES: usize = 512;

    /// Largest field width or precision an `awk` `printf` conversion will honour.
    ///
    /// `awk 'BEGIN{printf "%9999999999d", 1}'` parsed its width as a bare `usize` and then built a
    /// pad string that long. 64 KiB is far past any legitimate column layout.
    pub const MAX_PRINTF_WIDTH: usize = 64 * 1024;
    /// Maximum expression-nesting depth the `awk` parser accepts.
    ///
    /// The recursive-descent cycle `parse_expr → … → parse_primary → parse_expr` had no depth
    /// limit, so deeply parenthesised input overflowed the stack — an abort, which unlike a panic
    /// cannot even be caught.
    pub const MAX_AWK_PARSE_DEPTH: usize = 200;

    /// Largest terminal width honoured from `$COLUMNS` when laying out multi-column output.
    ///
    /// The value was parsed as an unbounded `usize`, so `COLUMNS=18446744073709551615` produced a
    /// column count near `usize::MAX` and an effectively infinite layout loop.
    pub const MAX_COLUMNS: usize = 1000;
}

/// Every environment variable clank reads, by name.
///
/// The names live here so the set is enumerable — "what can I set to change clank's behaviour?" was
/// previously answerable only by grepping for `env::var`. The *resolution* (default, parsing,
/// validation) stays with the module that owns the setting, because each resolves differently: the
/// path seams fall back to a [`vfs`] default, the context cap requires a positive integer, and the
/// Golem credentials have no default at all.
///
/// Not listed here: per-provider API-key variables (`ANTHROPIC_API_KEY` and siblings), which
/// `ai::config` derives from the provider name rather than naming individually.
pub mod env {
    /// Overrides [`super::limits::DEFAULT_CONTEXT_CAP_TOKENS`]. On the durable agent it is injected
    /// by `golem.yaml` (`components.clank:agent.env`); natively it is an ordinary env var. Unset or
    /// non-positive ⇒ the default.
    pub const CONTEXT_CAP_TOKENS: &str = "CLANK_CONTEXT_CAP_TOKENS";

    /// Overrides [`super::vfs::LOG_DIR`]. Exists so native tests are hermetic.
    pub const LOG_DIR: &str = "CLANK_LOG_DIR";

    /// Overrides [`super::vfs::GREASE_ETC`] — `registries.toml` + one `<name>.toml` per package.
    pub const GREASE_ETC: &str = "CLANK_GREASE_ETC";
    /// Overrides [`super::vfs::GREASE_STORE`] — the versioned payload store.
    pub const GREASE_STORE: &str = "CLANK_GREASE_STORE";
    /// Overrides [`super::vfs::PROMPT_BIN`] — prompt bin stubs.
    pub const GREASE_BIN: &str = "CLANK_GREASE_BIN";
    /// Overrides [`super::vfs::SCRIPT_BIN`] — script bin stubs.
    pub const GREASE_SCRIPT_BIN: &str = "CLANK_GREASE_SCRIPT_BIN";
    /// Overrides [`super::vfs::SKILLS`] — skill directories.
    pub const GREASE_SKILLS: &str = "CLANK_GREASE_SKILLS";
    /// Overrides [`super::vfs::MCP_MOUNT`] — where static MCP resources are materialized.
    pub const GREASE_MCP_MOUNT: &str = "CLANK_GREASE_MCP_MOUNT";
    /// Overrides [`super::vfs::AGENT_BIN`] — Golem-agent bin stubs.
    pub const GREASE_AGENT_BIN: &str = "CLANK_GREASE_AGENT_BIN";

    /// Overrides [`super::vfs::MCP_ETC`] — one `<server>.toml` per MCP server.
    pub const MCP_ETC: &str = "CLANK_MCP_ETC";
    /// Overrides [`super::vfs::MCP_BIN`] — generated per-server commands.
    pub const MCP_BIN: &str = "CLANK_MCP_BIN";

    /// Golem REST endpoint for the native cluster client. No default — absent ⇒ the `golem`
    /// commands report that no cluster is configured.
    pub const GOLEM_URL: &str = "GOLEM_URL";
    /// Bearer token for [`GOLEM_URL`].
    pub const GOLEM_TOKEN: &str = "GOLEM_TOKEN";
    /// Golem application name used to qualify agent ids.
    pub const GOLEM_APP: &str = "GOLEM_APP";
    /// Golem environment name used to qualify agent ids.
    pub const GOLEM_ENV: &str = "GOLEM_ENV";
}

/// clank's filesystem namespace: the whole package layout in one map.
///
/// These are **defaults**. Each is reachable through an accessor in the module that owns it
/// (`grease::config`, `mcp::config`, `logging`), which applies the matching [`env`] override; nothing
/// outside those accessors should use these constants to build a path, or it will ignore the
/// override. They are gathered here because the layout is a single design that was spread across six
/// files — and because [`DEFAULT_PATH`] must stay consistent with the bin directories below it, a
/// constraint that is invisible when they live apart.
///
/// **Known divergence:** [`MCP_MOUNT`] is overridable (`grease` materializes static resources through
/// the accessor), but [`crate::runtime::mcpfs`] matches the literal `/mnt/mcp` prefix when classifying
/// a path. Under an override the two disagree — the virtual listing and the real files part company.
/// Harmless today (the override exists only as a test seam) and left alone deliberately, but it is a
/// bug, and it is only visible because these two now sit next to each other.
pub mod vfs {
    /// `grease` config directory.
    pub const GREASE_ETC: &str = "/etc/grease";
    /// `grease` payload store (`<store>/<name>/<kind>.json`) — the source of truth for derived
    /// executables.
    pub const GREASE_STORE: &str = "/var/lib/grease";
    /// Prompt bin stubs (`<dir>/<name>`), on `$PATH`.
    pub const PROMPT_BIN: &str = "/usr/lib/prompts/bin";
    /// Script bin stubs (`<dir>/<name>`), on `$PATH`.
    pub const SCRIPT_BIN: &str = "/usr/bin";
    /// Skill directories (`<dir>/<name>/`), whose `*/bin` glob is on `$PATH`.
    pub const SKILLS: &str = "/usr/share/skills";
    /// Golem-agent bin stubs (`<dir>/<name>`), on `$PATH`.
    pub const AGENT_BIN: &str = "/usr/lib/agents/bin";
    /// Mount root for an MCP server's resources (`<root>/<server>/`).
    pub const MCP_MOUNT: &str = "/mnt/mcp";
    /// MCP config directory (one `<server>.toml` per server).
    pub const MCP_ETC: &str = "/etc/mcp";
    /// Generated per-MCP-server commands (`<dir>/<server>`), on `$PATH`.
    pub const MCP_BIN: &str = "/usr/lib/mcp/bin";
    /// Log directory (`shell.log`, `http.log`, `mcp.log`, `ops.log`).
    pub const LOG_DIR: &str = "/var/log";

    /// The virtual `/bin` namespace: every resolvable command surfaced as a file. Synthesized from
    /// the registry, not backed by disk.
    pub const BIN_ROOT: &str = "/bin/";
    /// The virtual `/proc` namespace: session and agent introspection. Synthesized, not disk-backed.
    pub const PROC_ROOT: &str = "/proc/";

    /// The README's default `$HOME`. Seeded on the agent (whose env starts empty) so `~` expansion
    /// and `~/.config/ask/ask.toml` resolve; native keeps the host's real `$HOME`.
    pub const HOME: &str = "/home/user";

    /// The README's default `$PATH` — the resolution namespace clank's package layout installs into.
    ///
    /// Every segment after `/usr/local/bin` is one of the bin directories above, which is why they
    /// live in the same module: a change to [`PROMPT_BIN`] that is not mirrored here silently makes
    /// installed prompts unresolvable. `session::effective_path` builds the live value by resolving
    /// each directory through its env-overridable accessor, and a unit test pins the two equal when
    /// no override is set.
    pub const DEFAULT_PATH: &str =
        "/usr/local/bin:/usr/bin:/usr/lib/mcp/bin:/usr/lib/agents/bin:/usr/lib/prompts/bin:/usr/share/skills/*/bin";
}
