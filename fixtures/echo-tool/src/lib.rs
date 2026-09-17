//! T2 FIXTURE — `echo-tool`, the tool the agent-tools tickets are tested against.
//!
//! No real Golem tools exist yet (upstream ships the mechanism with a deliberately empty
//! inventory), so this fixture is the stand-in. It is not meant to be useful; it is meant to be
//! *exhaustive*. Between them its commands declare every construct the CLI projection in ticket 5
//! must parse, so that ticket can be written test-first:
//!
//! | Construct | Where |
//! |---|---|
//! | inherited globals (option + count-flag) | every command's `color` / `verbose` |
//! | positional | `greet <name>` |
//! | scalar option with a default and a short | `greet -n/--times` |
//! | negatable bool flag | `greet --shout` / `--no-shout` |
//! | repeatable option, `either` form + delimiter | `grep -e/--pattern` |
//! | repeatable option, `delimited` form | `grep --items` |
//! | tail positional behind a `--` separator, verbatim | `grep -- <files...>` |
//! | constraint between arguments | `grep` (`all-match` with `pattern`) |
//! | stdin and stdout streams | `upper` |
//! | declared error cases with distinct exit codes | `fail` |
//! | subcommand subtree with an alias | `tree add` (alias `t`) |
//! | repeatable map option (`k=v`) and a record-typed option | `tree add -c k=v --meta {...}` |
//! | structured result, no stream | `list` |
//! | result formatters | `greet` |
//! | annotations driving authorization | `list` read-only, `destroy` destructive |
//! | filesystem-capable tool (owner lane) | `capable-echo write` / `read` |
//!
//! The descriptor of every tool here is pinned by `tools.snapshot.json` (the `snapshot` test at
//! the bottom): building it natively takes seconds, whereas a descriptor error at component load
//! panics inside a ctor and fails the whole deploy.
//!
//! Wire shape, confirmed by the T1 probe's metadata dump: the tool name is the *root* command node
//! (`body: None` when every method is a subcommand) and the wire `command-path` EXCLUDES it, so
//! `greet` is reached as `["greet"]` and `tree add` as `["tree", "add"]`.

// The tool macros expand to dispatch items carrying no doc comments; everything hand-written below
// is documented. Mirrors the allow on `fixtures/greeter-agent`. The other two lints fire on the
// same generated items: the per-trait dispatcher takes one argument per command parameter, and the
// generated `Result`-returning wrappers carry no `# Errors` section. Neither is reachable from
// hand-written code here.
#![allow(missing_docs, clippy::too_many_arguments, clippy::missing_errors_doc)]
// The SDK subtree client macro returns ToolError by value; its error size is SDK-owned.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;

use golem_rust::agentic::{InputStream, OutputStream};
use golem_rust::{FromSchema, IntoSchema, ToolError, tool_definition, tool_implementation};

/// What a stream-consuming command reports once its input is exhausted.
#[derive(Debug, Clone, IntoSchema, FromSchema)]
pub struct Summary {
    pub chunks: u32,
    pub bytes: u64,
}

/// A record-typed option value: spellable on a command line only as a JSON literal, which is
/// exactly the coercion fallback ticket 5 defines.
#[derive(Debug, Clone, IntoSchema, FromSchema)]
pub struct Meta {
    pub k: String,
    pub n: u32,
}

/// Two error cases with different kinds and different exit codes, so a consumer can prove the
/// tool's own exit code reaches the shell rather than collapsing to 1.
#[derive(Debug, Clone, ToolError)]
pub enum EchoError {
    #[tool_error(kind = "usage-error", exit_code = 2)]
    BadInput { reason: String },
    #[tool_error(kind = "runtime-error", exit_code = 7)]
    Boom,
}

/// Placeholder return type for the `tree` subtree dispatcher; never constructed by a caller.
pub struct TreeSubtree;

#[tool_definition(version = "1.0.0")]
pub trait EchoTool {
    /// Greet someone by name.
    #[command(aliases = ["hi"], annotations(read_only = true, idempotent = true))]
    #[arg(
        verbose = "global",
        short = 'v',
        kind = "count-flag",
        max = 3,
        env = "ECHO_VERBOSE"
    )]
    #[arg(color = "global", default = "auto")]
    #[arg(name = "positional")]
    // A numeric default is an unquoted literal; `default = "1"` is a type mismatch the descriptor
    // build rejects at component load (and a failed descriptor build panics a ctor, which fails
    // the whole deploy — see dev-docs/research/agent-tools-probes.md).
    #[arg(times = "option", short = 'n', default = 1)]
    #[arg(shout = "flag", negatable = true, default = false, env = "ECHO_SHOUT")]
    #[result(formatters = ["plain", "loud"], default = "plain")]
    async fn greet(
        &self,
        verbose: u32,
        color: String,
        name: String,
        times: u32,
        shout: bool,
    ) -> Result<String, EchoError>;

    /// Filter lines, echoing back what the projection actually parsed.
    #[command(annotations(read_only = true))]
    #[arg(verbose = "global", short = 'v', kind = "count-flag", max = 3)]
    #[arg(color = "global", default = "auto")]
    #[arg(pattern = "option", short = 'e', repeatable = "either", delim = ',')]
    #[arg(items = "option", repeatable = "delimited", delim = ',')]
    #[arg(all_match = "flag")]
    #[arg(files = "tail", separator = "--", min = 0, verbatim = true)]
    #[constraint(all_or_none = ["all-match", "pattern"])]
    async fn grep(
        &self,
        verbose: u32,
        color: String,
        pattern: Vec<String>,
        items: Vec<String>,
        all_match: bool,
        files: Vec<String>,
    ) -> Result<Vec<String>, EchoError>;

    /// Uppercase stdin onto stdout: the stream-in, stream-out shape.
    #[command(annotations(read_only = true))]
    #[arg(verbose = "global", short = 'v', kind = "count-flag", max = 3)]
    #[arg(color = "global", default = "auto")]
    async fn upper(
        &self,
        verbose: u32,
        color: String,
        stdin: InputStream,
        stdout: OutputStream,
    ) -> Result<Summary, EchoError>;

    /// Fail on purpose. `--usage` selects the usage-kind case (exit 2), otherwise runtime (exit 7).
    #[command(annotations(read_only = true))]
    #[arg(verbose = "global", short = 'v', kind = "count-flag", max = 3)]
    #[arg(color = "global", default = "auto")]
    #[arg(usage = "flag")]
    async fn fail(&self, verbose: u32, color: String, usage: bool) -> Result<String, EchoError>;

    /// Pure dispatcher: a subcommand subtree, reached as `tree add` (or `t add`).
    #[command(subtree = Tree, aliases = ["t"])]
    #[arg(verbose = "global", short = 'v', kind = "count-flag", max = 3)]
    #[arg(color = "global", default = "auto")]
    fn tree(&self, verbose: u32, color: String) -> TreeSubtree;

    /// A structured result with no stdout stream — the other half of the output rule.
    #[command(annotations(read_only = true))]
    #[arg(verbose = "global", short = 'v', kind = "count-flag", max = 3)]
    #[arg(color = "global", default = "auto")]
    async fn list(&self, verbose: u32, color: String) -> Result<Vec<String>, EchoError>;

    /// Destructive: the annotation ticket 4 turns into a confirmation policy.
    #[command(annotations(destructive = true))]
    #[arg(verbose = "global", short = 'v', kind = "count-flag", max = 3)]
    #[arg(color = "global", default = "auto")]
    async fn destroy(&self, verbose: u32, color: String) -> Result<String, EchoError>;

    /// Return a positional string, using stdin when the argument is absent or `-`.
    #[command(annotations(read_only = true))]
    #[arg(text = "positional", accepts_stdio = true)]
    async fn stdin_arg(&self, text: String) -> Result<String, EchoError>;
}

#[tool_definition]
pub trait Tree {
    /// Repeatable map option (`-c k=v`) plus a record-typed option, with a constraint tying them.
    ///
    /// `meta` is deliberately `Meta`, not `Option<Meta>`. An `Option<T>` option parameter cannot be
    /// invoked at all as of SDK `f5a3d29b9`: the descriptor unwraps `Option<T>` to `T`
    /// (`golem-rust-macro/src/tool/descriptor.rs:1125`, "it only makes the argument not-required"),
    /// so the canonical input field is a bare `Meta` and the host demands a record value — but the
    /// generated guest decode still runs `<Option<Meta> as FromSchema>::from_value`, which accepts
    /// only an `option` value. Observed live on 2026-09-14: the host accepted the record, the parent
    /// forwarded it to this subtree, and the guest replied
    /// `InvalidInput("shape mismatch in Option: expected option, got record")`. No caller can satisfy
    /// both sides; see `dev-docs/research/agent-tools-probes.md` (T2) and upstream-asks Issue 9.
    #[command(annotations(read_only = true))]
    #[arg(entries = "option", short = 'c', repeatable = "repeated")]
    #[arg(meta = "option")]
    #[constraint(all_or_none = ["meta", "entries"])]
    async fn add(&self, entries: BTreeMap<String, String>, meta: Meta) -> Result<Meta, EchoError>;
}

/// The filesystem-capable half of the fixture. Declared in `golem.yaml` with a provisioned marker
/// file (`/.echo/MARKER`), which is what implies the filesystem grant today, so its commands run
/// against the *calling agent's* filesystem under the owner lane. `write` then `read` round-tripping
/// through the shell proves the capability classification, not just the wire.
#[tool_definition(version = "1.0.0")]
pub trait CapableEcho {
    /// Write `text` to `path` as a whole-file write (idempotent under replay); returns the byte count.
    #[command(annotations(destructive = true, idempotent = true))]
    #[arg(path = "positional")]
    #[arg(text = "positional")]
    async fn write(&self, path: String, text: String) -> Result<u64, EchoError>;

    /// Read `path` back.
    #[command(annotations(read_only = true, idempotent = true))]
    #[arg(path = "positional")]
    async fn read(&self, path: String) -> Result<String, EchoError>;
}

struct EchoToolImpl;

#[tool_implementation]
impl EchoTool for EchoToolImpl {
    #[allow(clippy::unused_async_trait_impl)]
    async fn greet(
        &self,
        verbose: u32,
        color: String,
        name: String,
        times: u32,
        shout: bool,
    ) -> Result<String, EchoError> {
        // Echo the parsed inputs back so an acceptance test can assert exactly what the tool got.
        let body = std::iter::repeat_n(format!("hello {name}"), times.max(1) as usize)
            .collect::<Vec<_>>()
            .join(" ");
        let body = if shout { body.to_uppercase() } else { body };
        Ok(format!("{body} [color={color} verbose={verbose}]"))
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn grep(
        &self,
        verbose: u32,
        color: String,
        pattern: Vec<String>,
        items: Vec<String>,
        all_match: bool,
        files: Vec<String>,
    ) -> Result<Vec<String>, EchoError> {
        Ok(vec![
            format!("pattern={}", pattern.join("|")),
            format!("items={}", items.join("|")),
            format!("all_match={all_match}"),
            format!("files={}", files.join("|")),
            format!("color={color} verbose={verbose}"),
        ])
    }

    async fn upper(
        &self,
        _verbose: u32,
        _color: String,
        mut stdin: InputStream,
        mut stdout: OutputStream,
    ) -> Result<Summary, EchoError> {
        let mut chunks = 0;
        let mut bytes = 0u64;
        while let Some(item) = stdin.next().await {
            let Ok(chunk) = item else { break };
            chunks += 1;
            bytes += chunk.len() as u64;
            let upper = String::from_utf8_lossy(&chunk).to_uppercase().into_bytes();
            if stdout.write(upper).await.is_err() {
                return Err(EchoError::Boom);
            }
        }
        Ok(Summary { chunks, bytes })
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn fail(&self, _verbose: u32, _color: String, usage: bool) -> Result<String, EchoError> {
        if usage {
            Err(EchoError::BadInput {
                reason: "asked for a usage error".to_string(),
            })
        } else {
            Err(EchoError::Boom)
        }
    }

    fn tree(&self, _verbose: u32, _color: String) -> TreeSubtree {
        TreeSubtree
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn list(&self, _verbose: u32, _color: String) -> Result<Vec<String>, EchoError> {
        Ok(vec!["alpha".to_string(), "beta".to_string()])
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn destroy(&self, _verbose: u32, _color: String) -> Result<String, EchoError> {
        Ok("destroyed".to_string())
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn stdin_arg(&self, text: String) -> Result<String, EchoError> {
        Ok(text)
    }
}

struct TreeImpl;

#[tool_implementation]
impl Tree for TreeImpl {
    #[allow(clippy::unused_async_trait_impl)]
    async fn add(&self, entries: BTreeMap<String, String>, meta: Meta) -> Result<Meta, EchoError> {
        // Echo back what arrived: the map's keys joined, prefixed by any `meta.k` the caller sent,
        // and the entry count added to `meta.n`.
        let n = u32::try_from(entries.len()).unwrap_or(u32::MAX);
        let keys = entries.keys().cloned().collect::<Vec<_>>().join(",");
        Ok(Meta {
            k: if meta.k.is_empty() {
                keys
            } else {
                format!("{}+{keys}", meta.k)
            },
            n: meta.n.saturating_add(n),
        })
    }
}

struct CapableEchoImpl;

#[tool_implementation]
impl CapableEcho for CapableEchoImpl {
    #[allow(clippy::unused_async_trait_impl)]
    async fn write(&self, path: String, text: String) -> Result<u64, EchoError> {
        std::fs::write(&path, text.as_bytes()).map_err(|e| EchoError::BadInput {
            reason: format!("write {path}: {e}"),
        })?;
        Ok(text.len() as u64)
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn read(&self, path: String) -> Result<String, EchoError> {
        std::fs::read_to_string(&path).map_err(|e| EchoError::BadInput {
            reason: format!("read {path}: {e}"),
        })
    }
}

/// Pins the descriptor of every tool this component registers.
///
/// This runs the same `try_to_tool()` the SDK runs inside a `#[ctor]` at component load — the
/// one that, on failure, panics and takes the whole `golem deploy` down with a wasm backtrace
/// (a mistyped `default = "1"` on a `u32` did exactly that on 2026-09-13). Here it fails in
/// seconds, natively, with the error text. The JSON is the golem-schema model, so any upstream
/// change to how the macros derive metadata shows up as a diff against `tools.snapshot.json`.
/// Regenerate with `UPDATE_SNAPSHOT=1 cargo test -p echo-tool`.
#[cfg(test)]
mod snapshot {
    const SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tools.snapshot.json");
    /// Every tool the component registers, in snapshot order. A tool added to the crate without
    /// being added here fails the count assertion below rather than silently going unpinned.
    const TOOLS: [&str; 3] = ["capable-echo", "echo-tool", "tree"];

    fn current() -> String {
        let registered = golem_rust::agentic::get_all_tools().len();
        assert_eq!(
            registered,
            TOOLS.len(),
            "the component registers {registered} tool(s) but the snapshot lists {} — update TOOLS",
            TOOLS.len()
        );
        let mut out = String::from("{\n");
        for (i, name) in TOOLS.iter().enumerate() {
            let extended = golem_rust::agentic::get_extended_tool_by_name(name)
                .unwrap_or_else(|| panic!("tool `{name}` is not registered"));
            // The descriptor build proper: this is what a ctor would panic on. It yields the wire
            // form; `decode_tool` lifts it to the golem-schema model, which is what serializes and
            // what a consumer (clank's `decode_tool` → `canonical_input_model`) actually reads.
            let wire = extended
                .try_to_tool()
                .unwrap_or_else(|e| panic!("descriptor build failed for `{name}`: {e:?}"));
            let tool = golem_rust::schema::tool::wit::decode_tool(wire)
                .unwrap_or_else(|e| panic!("decode_tool failed for `{name}`: {e:?}"));
            let json = serde_json::to_string_pretty(&tool).expect("serialize tool");
            out.push_str("  \"");
            out.push_str(name);
            out.push_str("\": ");
            out.push_str(&json.replace('\n', "\n  "));
            out.push_str(if i + 1 < TOOLS.len() { ",\n" } else { "\n" });
        }
        out.push_str("}\n");
        out
    }

    #[test]
    fn descriptor_builds_and_matches_snapshot() {
        let current = current();
        if std::env::var_os("UPDATE_SNAPSHOT").is_some() {
            std::fs::write(SNAPSHOT, &current).expect("write tools.snapshot.json");
            return;
        }
        let stored = std::fs::read_to_string(SNAPSHOT).unwrap_or_else(|e| {
            panic!("cannot read {SNAPSHOT}: {e} — generate it with UPDATE_SNAPSHOT=1")
        });
        if stored != current {
            let first_diff = stored
                .lines()
                .zip(current.lines())
                .enumerate()
                .find(|(_, (a, b))| a != b)
                .map_or_else(
                    || {
                        format!(
                            "line counts differ: {} vs {}",
                            stored.lines().count(),
                            current.lines().count()
                        )
                    },
                    |(n, (a, b))| format!("line {}:\n  snapshot: {a}\n  current:  {b}", n + 1),
                );
            panic!(
                "tool metadata drifted from tools.snapshot.json — first difference at {first_diff}\n\
                 If the change is intended: UPDATE_SNAPSHOT=1 cargo test -p echo-tool"
            );
        }
    }
}
