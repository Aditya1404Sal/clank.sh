---
title: "Agent Tools Integration - Implementation Plan"
date: 2026-09-10
author: agent
issue: dev-docs/issues/open/agent-tools-integration.md
design: dev-docs/designs/proposed/agent-tools-integration.md
research: dev-docs/research/agent-tools-probes.md
---

# Agent Tools Integration - Implementation Plan

## Table of Contents

- [Targets and phases](#targets-and-phases)
- [Spec](#spec)
- [Architecture](#architecture)
- [Tickets](#tickets)
  1. SDK re-target, wasip3 fix, raw tool-call probe
  2. Test infrastructure: `echo-tool` fixture, conformance `tools` tier, e2e, CI
  3. Core model and the `ToolInvoker` seam
  4. Discovery → `$PATH`, manifests, `--help`/`type`/`which`/`man`, native stubs
  5. The projection: argv parser, coercion, constraints, canonical record
  6. Session dispatch: pipelines, result rendering, error mapping, audit
  7. `ask` exposure and authorization from annotations
  8. grease: deploy-time bind, host-side honesty, documentation
  9. Upstream contributions
  10. The `clank` tool: export, filesystem grant, session persistence, latency
  11. clank-embed features `tool` / `full`, greeter as reference host
  12. Conformance `embedded-tool` backend, parity gate, trap gate
  13. Optional: stream-typed `shell` method

## Targets and phases

clank runs on three targets that share one core. All shell logic — the `ToolDefinition` model,
the argv projection, the `ToolInvoker` seam, manifests, authorization, audit — is identical on
every target. Only the provider injected behind the seam differs.

**Native `clank` binary:** no Golem host, so no tool can be invoked (tools are reachable only
through the `golem:tool/host` guest import; there is no REST, CLI or MCP route). Native clank
reads the application manifest for the tools bound to `ClankAgent` and registers exit-4 stubs so
`grease list` shows the same set everywhere and an unbound tool is distinguishable from an
unknown command. A wasmtime-embedded local runner is a follow-up, not part of this plan.

**`ClankAgent` on Golem:** the durable shell agent. The embed provider (`AmbientToolInvoker`)
discovers the agent's bound tools through `get-all-tools` and invokes them through `tool-rpc`.
This is where the whole consumer surface (tickets 3–8) is exercised.

**Third-party agent hosting clank:** an agent that includes `clank-embed`. With the `tool`
feature (default after ticket 11) its `eval`/`answer_prompt`/`abort_prompt` methods forward to
the `clank` tool bound in its manifest; with the `full` feature it carries the in-process
`Session` exactly as today. Either way `golem agent shell` works against it.

The manifest and feature flags select the mode:

```yaml
# golem.yaml — a third-party host binding clank as a tool
tools:
  clank:
    release: { account: aditya, name: clank, version: 0.1.0 }   # or component: clank:agent
    files:
      - sourcePath: fixtures/clank-marker
        targetPath: /.clank/MARKER          # implies the filesystem grant until GOL-29 lands
agents:
  Greeter:
    tools:
      clank: {}
```

```toml
# Cargo.toml of the host
clank-embed = { path = "...", features = ["providers"] }        # default = ["tool"]
# clank-embed = { ..., default-features = false, features = ["full", "providers"] }
```

Phases: **P0** (ticket 1–2) removes the prerequisites; **P1** (3–9) is the consumer direction
and ships as the `agent-tools-integration` branch; **P2** (10–13) is the provider direction
and opens its own slug (`clank-as-tool`) when P1 merges, per the one-issue/one-design/one-plan
rule.

---

## Spec

The integration addresses three gaps between clank and Golem's agent tools:

1. **Bound tools are invisible to the shell.** A tool bound to `ClankAgent` in `golem.yaml` is
   not on `$PATH`; `type`, `which`, `--help`, `man`, `ask` and `grease` do not know it. The tool
   metadata is CLI-shaped precisely so it can be used like a Unix utility, and the WIT names a
   "full CLI projection" as a future deliverable (`golem-common/wit/deps/golem-tool/common.wit:9-14`).
   No such projection exists anywhere; clank is where it belongs.
2. **A shell inside another agent means compiling the whole shell into it.** `clank-embed` today
   is the entire `Session`. There is no way to bind clank the way every other tool is bound —
   declared once, updated independently, governed by the operator's binding and, once GOL-39
   lands, by bypass-resistant middleware. `agent shell` therefore only reaches agents that embed
   the full shell.
3. **grease package type 6 is unfilled and the native target has no story.** README.md:677
   reserves "wRPC WASM components" as a roadmap kind; agent tools are that kind. Nothing tells a
   native user which tools exist or why they cannot run.

**The integration:**

- **clank is the CLI projection.** A metadata-driven argv parser walks each bound tool's
  command tree, parses options/flags/positionals/tail with the full CLI vocabulary, evaluates
  constraints, coerces values against the tool's schema graph, and emits the canonical input
  record as a `typed-schema-value`. Stdout streams into the pipeline, structured results render
  through the default formatter, declared error cases become exit codes, `--help` renders from
  `doc` at any depth. One implementation serves every tool; nothing is generated per tool.
- **Bound tools are ambient; grease binds at deploy time.** Bindings are compiled per agent
  type at deploy (`cli/golem-cli/src/model/app_raw.rs:370`; environment-level bindings were
  removed in #3788). Inside a host every bound tool appears under `/usr/lib/tools/bin/` with
  no ceremony. `grease install tool:<name>` is the native-side verb that edits `golem.yaml`
  and deploys; inside a host it fails honestly.
- **clank ships as a tool, and the host reaches it through its own agent methods.** clank's
  component exports a `clank` tool beside `ClankAgent` (the `golem-agentic` world exports both
  guests). `clank-embed` gains a `tool` feature whose `eval`/`answer_prompt`/`abort_prompt`
  forward to that tool over `tool-rpc`, so `golem agent shell` works against any host that
  includes the shim. The `full` feature keeps today's in-process session; the conformance
  corpus keeps the two behaviourally identical.
- **The runtime is authoritative about isolation; clank works with it, not around it.** A
  tool body runs in a fresh Store per invocation (gol-33), so the `clank` tool persists its
  session to the owner's filesystem with whole-file writes (the replay-safe pattern). A
  filesystem-capable tool's stdout is published only at the terminal, so interactivity is
  per-line. A trap inside a tool interrupts the owning agent; clank's panic hygiene becomes a
  hosting-safety property with a gate in ticket 12.

**`agent shell` remains the primary interactive client.** golemcloud/golem#3700 was auto-closed
by the vouching bot, not reviewed. It validates any agent exposing the three methods against the
reflected schema and renders pending prompts as a select menu. A stream-typed `shell` method
driven by the stock CLI (`golem agent invoke 'X()' shell - --stdin-format raw --stdout-format
raw`) is an optional secondary route (ticket 13) for people without the patched binary.

**What is out of scope:** component composition, MCP import/export of tools through Golem, TTY
host imports, authoring tool middleware in clank, changes to the wRPC/agent package kind.
Follow-ups noted for a later plan: local wasmtime tool runner for native; structured `ask` tool
definitions (only if ticket 7's live run shows the model misusing CLI syntax); a `clank-guard`
universal middleware once GOL-39 lands; `&`/`kill` via `async-invoke-and-await`; secrets-typed
arguments once the tool model carries `secret`.

---

## Architecture

**Consumer path (inside `ClankAgent`):**

```
 line ──► Brush split ──► Session classifier ──► is_tool_line? ──► authz (per subcommand)
                                                                         │
                          ┌──────────────────────────────────────────────┘
                          ▼
   golem/tool/argv.rs  parse(def, argv, env) ──► ParsedInvocation { command_path, input: Record }
                          │
                          ▼
   ToolInvoker::invoke(ToolCall) ─── clank-embed AmbientToolInvoker ───► golem:tool/host tool-rpc
                          ▲               encode ToolValue → TypedSchemaValue         │
                          │               create_stdout / create-stdin-from-stream    ▼
      stdout chunks ◄─────┴───────────────────────────────────────── sidecar Store (fresh per call,
      result / error                                                  owner root if FS-capable)
                          │
                          ▼
   session/tool.rs  LineResult { stdout → pipeline pipe, stderr, exit } ──► /var/log/ops.log
```

**Provider path (third-party host with `clank-embed`, feature `tool`):**

```
 golem agent shell 'Greeter("g1")'  ──eval(line)──►  Greeter (agent method)
                                                      │  EmbeddedShell::eval  (tool feature)
                                                      ▼
                                          AmbientToolRpc("clank").invoke_and_await([], {line})
                                                      │
                                                      ▼  fresh Store of clank's component
                                          Clank::clank(line):
                                              load  /.clank/session.json   (owner root)
                                              restore Session, eval, capture
                                              save  /.clank/session.json   (whole-file write)
                                              return EvalResult { stdout, stderr, exit_code, pending_prompt, cwd }
```

### Core types (`crates/clank-core/src/golem/tool/mod.rs`)

clank-core owns a mirror of the wire `tool` record. It has no golem crate in its dependency
graph (the native binary must not build against the golem clone); `clank-embed` converts the
wire form to this model once at discovery time.

```rust
pub type TypeNodeIndex = i32;
pub type CommandIndex = i32;

// Every type in this model derives `Serialize`/`Deserialize` as well: the `clank` tool's session
// file (ticket 10) caches the last discovery, and the fixture snapshot tests (ticket 3) diff it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,                    // == commands[0].name
    pub version: String,
    pub commands: Vec<CommandNode>,      // root at index 0
    pub schema: Vec<TypeNode>,           // the tool's type-node pool
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandNode {
    pub name: String,
    pub aliases: Vec<String>,
    pub doc: Doc,
    pub globals: Globals,
    pub subcommands: Vec<CommandIndex>,
    pub body: Option<CommandBody>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Globals { pub options: Vec<OptionSpec>, pub flags: Vec<FlagSpec> }

#[derive(Clone, Debug, PartialEq)]
pub struct CommandBody {
    pub positionals: Positionals,
    pub options: Vec<OptionSpec>,
    pub flags: Vec<FlagSpec>,
    pub constraints: Vec<Constraint>,
    pub stdin: Option<StreamSpec>,
    pub stdout: Option<StreamSpec>,
    pub result: Option<ResultSpec>,
    pub errors: Vec<ErrorCase>,
    pub annotations: Option<CommandAnnotations>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Positionals { pub fixed: Vec<Positional>, pub tail: Option<TailPositional> }

#[derive(Clone, Debug, PartialEq)]
pub struct Positional {
    pub name: String, pub doc: Doc, pub value_name: Option<String>,
    pub ty: TypeNodeIndex, pub default: Option<ToolValue>, pub required: bool, pub accepts_stdio: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TailPositional {
    pub name: String, pub doc: Doc, pub value_name: Option<String>,
    pub item_type: TypeNodeIndex, pub min: u32, pub max: Option<u32>,
    pub separator: Option<String>, pub verbatim: bool, pub accepts_stdio: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OptionSpec {
    pub long: String, pub short: Option<char>, pub aliases: Vec<String>, pub doc: Doc,
    pub value_name: Option<String>, pub shape: OptionShape,
    pub default: Option<ToolValue>, pub required: bool, pub env_var: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OptionShape {
    Scalar(TypeNodeIndex),
    OptionalScalar(TypeNodeIndex),
    RepeatableList { repetition: Repetition, item_type: TypeNodeIndex },
    RepeatableMap { repetition: Repetition, map_type: TypeNodeIndex, duplicate_key_policy: DuplicateKeyPolicy },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Repetition { Repeated, Delimited(char), Either(char) }

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DuplicateKeyPolicy { Reject, LastWins }

#[derive(Clone, Debug, PartialEq)]
pub struct FlagSpec {
    pub long: String, pub short: Option<char>, pub aliases: Vec<String>, pub doc: Doc,
    pub shape: FlagShape, pub env_var: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FlagShape { Bool { default: bool, negatable: bool }, Count { max: Option<u32> } }

#[derive(Clone, Debug, PartialEq)]
pub enum Ref { Present(String), ValueIs { name: String, value: ToolValue } }

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Quantifier { All, Any }

#[derive(Clone, Debug, PartialEq)]
pub enum Constraint {
    RequiresAll(Vec<Ref>),
    AllOrNone(Vec<Ref>),
    RequiresAny(Vec<Ref>),
    MutexGroups(Vec<Vec<Ref>>),
    Implies { lhs_quant: Quantifier, lhs: Vec<Ref>, rhs_quant: Quantifier, rhs: Vec<Ref> },
    Forbids { lhs_quant: Quantifier, lhs: Vec<Ref>, rhs: Vec<Ref> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamSpec { pub doc: Doc, pub mime: Vec<String>, pub required: bool }

#[derive(Clone, Debug, PartialEq)]
pub struct ResultSpec {
    pub ty: TypeNodeIndex, pub doc: Doc,
    pub formatters: Vec<(String, Doc)>, pub default_formatter: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ErrorKind { Usage, Runtime }

#[derive(Clone, Debug, PartialEq)]
pub struct ErrorCase {
    pub name: String, pub doc: Doc, pub kind: ErrorKind, pub exit_code: u8,
    pub payload: Option<TypeNodeIndex>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CommandAnnotations { pub read_only: bool, pub destructive: bool, pub idempotent: bool, pub open_world: bool }

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Doc { pub summary: String, pub description: String, pub examples: Vec<(String, String)> }

/// The subset of golem:core/types@2.0.0 type nodes a command line can spell. Anything else
/// becomes `Opaque` and accepts only a JSON literal, which the embed encoder decodes against the
/// real wire node.
#[derive(Clone, Debug, PartialEq)]
pub enum TypeNode {
    Bool,
    Int { signed: bool, bits: u8, min: Option<i128>, max: Option<i128> },
    Float { bits: u8 },
    Char,
    Str { regex: Option<String>, min_len: Option<u32>, max_len: Option<u32> },
    Bytes,
    List(TypeNodeIndex),
    Option(TypeNodeIndex),
    Map { key: TypeNodeIndex, value: TypeNodeIndex },
    Record(Vec<(String, TypeNodeIndex)>),
    Enum(Vec<String>),
    Variant(Vec<(String, Option<TypeNodeIndex>)>),
    Named { name: String, inner: TypeNodeIndex },   // a `schema-type-def` reference; `name` matches error-case payloads
    Opaque { wire_index: TypeNodeIndex },
}

/// Values the projection produces. `JsonLiteral` carries a value the command line could only
/// express as JSON (records, variants, opaque nodes); the encoder turns it into the typed form.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolValue {
    Bool(bool), Int(i64), UInt(u64), Float(f64), Str(String), Bytes(Vec<u8>),
    List(Vec<ToolValue>), Map(Vec<(ToolValue, ToolValue)>), Option(Option<Box<ToolValue>>),
    Record(Vec<(String, ToolValue)>), Enum(String), JsonLiteral(serde_json::Value),
}
```

### The seam (`crates/clank-core/src/golem/tool/mod.rs`)

```rust
#[derive(Clone, Debug)]
pub struct RegisteredTool { pub name: String, pub component: String, pub definition: ToolDefinition }

pub struct ToolCall {
    pub tool: String,
    pub command_path: Vec<String>,
    pub input: ToolValue,            // always ToolValue::Record, in canonical field order
    pub stdin: Option<Vec<u8>>,      // staged pipeline bytes when the body declares stdin
    pub expects_stdout: bool,        // body declares a stdout stream-spec
}

pub struct ToolOutcome { pub result: Option<ToolValue> }

#[derive(Debug)]
pub enum ToolFailure {
    /// `custom-error`: the payload plus the name of its root type when the type is named.
    ToolError { type_name: Option<String>, payload: Option<ToolValue> },
    /// Host-side `invalid-input` / `constraint-violation` / `invalid-command-path` / `invalid-result`.
    InvalidInput(String),
    Denied(String),
    NotFound(String),
    Protocol(String),
    Cancelled,
    ResourceExhausted(String),
    /// Native provider, or an embed without a Golem host.
    Unavailable(String),
}

pub trait ToolOutputSink { fn write_stdout(&mut self, bytes: &[u8]); }

#[async_trait::async_trait(?Send)]
pub trait ToolInvoker {
    async fn discover(&self) -> Result<Vec<RegisteredTool>, String>;
    async fn invoke(&self, call: ToolCall, sink: &mut dyn ToolOutputSink) -> Result<ToolOutcome, ToolFailure>;
}
```

### Filesystem layout

```
/usr/lib/tools/bin/<tool>        stub per bound tool (so which/Brush/type resolve it); on $PATH after /usr/lib/mcp/bin
/.clank/session.json             the `clank` tool's persisted session (owner root of the host agent)
/.clank/MARKER                   provisioned file that implies the filesystem grant (until GOL-29)
/var/log/ops.log                 one `tool-invoke` record per invocation
```

### Lifecycle

```
discover (Session::new, and whenever ToolState.version bumps):
    tools = invoker.discover()                       ← get-all-tools; durable upstream, replay-safe
    for t in tools: manifests += manifest_for(&t.definition); write stub /usr/lib/tools/bin/<t.name>
    ToolState { tools, version += 1 }                ← dynreg cache key = (mcp.version, grease.version, tools.version)

invoke (Session::run_tool, one Session-layer stage per line):
    argv = split(line); def = tools.definition(argv[0])
    help?         → render_help(def, path)            ← pre-authz dynamic-help hook, same as pkg_help_for
    authz         → manifest(subcommand).authorization_policy → Decision (Confirm ⇒ pending prompt / sudo)
    parsed        = parse(def, argv[1..], env)        ← Usage ⇒ stderr + exit 2, nothing invoked
    stdin         = staged pipeline bytes if parsed.wants_stdin (pre-extracted like `ask`)
    outcome       = invoker.invoke(ToolCall{..}, sink) ← stdout chunks land in the pipeline pipe
    stdout        = stream if expects_stdout else render(outcome.result)      ← never both
    exit          = 0 | exit_code_for(failure, matched error-case)
    ops.log       += tool-invoke { tool, path, exit, ms }; transcript += line + stdout

provider (Clank::clank in a fresh Store):
    snap = SessionSnapshot::load("/.clank/session.json")      ← Err ⇒ fresh session + one stderr warning
    shell = EmbeddedShell (full); snap.restore(shell.session_mut())
    result = shell.eval(line)
    SessionSnapshot::capture(shell.session()).save(path)      ← whole-file write; idempotent under replay
    return result
```

### Error mapping

| Source | Exit | stderr |
|---|---|---|
| `ArgvError::Usage` (clank-side parse, constraint, coercion) | 2 | `<tool> <path>: <reason>` + one usage line |
| `ToolFailure::ToolError` matching a declared `error-case` | its `exit_code` | `<tool> <path>: <error-name>: <payload>` |
| `ToolFailure::ToolError` with no matching case | 1 (`Runtime`) / 2 (`Usage` kind unknown ⇒ 1) | `<tool> <path>: tool error: <payload>` |
| `ToolFailure::InvalidInput` | 2 | `... rejected by the host (projection bug or metadata drift): <msg>` |
| `ToolFailure::Denied` | 3 | `... permission denied: <msg>` |
| `ToolFailure::NotFound` | 127 | `... no longer bound (deploy drift); tools are re-discovered on the next line` |
| `ToolFailure::Protocol` | 1 | verbatim |
| `ToolFailure::Cancelled` | 130 | `... cancelled` |
| `ToolFailure::ResourceExhausted` | 1 | `... exceeded the 16 MiB attachment cap; filter or paginate the output` |
| `ToolFailure::Unavailable` | 4 | `<tool>: agent tools need a Golem host` |
| guest trap inside the tool | — | never observed by clank; Golem interrupts and retries the owner |

---

## Tickets

### 1. SDK re-target, wasip3 fix, raw tool-call probe

The SDK clone clank builds against (`~/Desktop/clank.sh/golem-stuff/golem`, branch
`clank-connect-patch`) is 91 commits behind upstream `main` and predates tool execution
entirely; `crates/clank-embed` already fails to compile against its wasip3 rebase. This ticket
makes the toolchain match the design's baseline (`8d0fd1f3c`) and proves one raw tool call from
`ClankAgent` before any projection code exists.

**`~/Desktop/clank.sh/golem-stuff/golem`** — rebase `clank-connect-patch` onto `upstream/main`:

```sh
git fetch upstream main
git rebase upstream/main          # only the `agent shell` CLI commits should replay
git log --oneline upstream/main..HEAD   # expected: the interactive_shell.rs / command.rs / worker/mod.rs commits, nothing SDK-side
cargo build -p golem              # the patched binary RC_TESTING.md points at
```

Conflicts, if any, are confined to `cli/golem-cli/src/command_handler/worker/interactive_shell.rs`
and `command.rs`; resolve in favour of upstream for everything the shell commit does not own.

**`crates/clank-embed/src/agent_invoker.rs`** — the two sites the wasip3 SDK broke:

```rust
// :96  invoke-and-await now returns invocation-result-with-metadata
match client.invoke_and_await(&inv.method, input) {
    Ok(with_meta) => render_result(with_meta.result),          // was: render_result(result)
    Err(e) => Err(format!("agent invocation failed: {e:?}")),
}

// :123 wall-clock datetime is now wasi:clocks/system-clock@0.3.0 `instant`, re-exported as ScheduledTime
let dt = golem_rust::ScheduledTime { seconds: secs, nanoseconds: 0 };   // seconds: i64
```

`Cargo.lock` is committed with the re-resolved golem-rust dependency set (wasip3, wit-bindgen
0.59, no wstd under golem-rust).

**Probe (throwaway branch, findings in `dev-docs/research/agent-tools-probes.md`)** — deploy
upstream's `test-components/tool-streaming` provider next to clank (add it to `golem.yaml` as
`tools: streaming, capable-streaming` bound to `ClankAgent`), add a temporary agent method that
calls the tool with the raw client, and record:

```rust
let rpc = golem_rust::agentic::AmbientToolRpc::new("streaming");
let (stdout_target, stdout_reader) = golem_rust::golem_agentic::golem::tool::host::create_stdout();
let input = raw_run_input("probe");   // copied from test-components/tool-streaming/rust-caller/src/lib.rs
let result = rpc.invoke_and_await(vec!["run".into()], input, None, Some(stdout_target));
```

Record: (a) the exact canonical record (field names and order) the generated `StreamingClient`
sends for one leaf command, captured by logging `build_canonical_input`'s output, next to what
`canonical_input_record_schema` reports; (b) that `capable-streaming`'s stdout arrives only at
the terminal while `streaming` arrives live; (c) p50 over 20 calls per variant at the `release`
preset; (d) a go/no-go line for ticket 3 at the top of the file.

**`RC_TESTING.md`** — the data-dir note and command agree (`/tmp/clank-rc1`); add the rebase
step and the `cargo build -p golem` line under Prereqs.

**Tests** — `cargo check`/`clippy -D warnings` on `wasm32-wasip2` for `clank-agent` and
`greeter-agent`; the existing `scripts/golem-e2e.sh --with-grease` (agent-invoke round trip
including `--schedule`) stays green on the rebased SDK; RC_TESTING.md's four-step smoke passes.

**Done when:** the wasm crates build and clippy clean on the rebased SDK, the e2e is green, the
probe doc exists with all four findings, and `git log upstream/main..clank-connect-patch` shows
only the `agent shell` commits.

**Deviations noted during implementation (2026-09-11, rebase onto `f5a3d29b9`):**

- The SDK had moved past the two breakages listed above. `WasmRpc::invoke_and_await`, `invoke`
  and `schedule_cancelable_invocation` all gained a trailing `scope-card:
  option<borrow<permission-card>>` argument (passed as `None`: the caller's own authority), and
  `schedule_cancelable_invocation` now returns `Result`. `golem_rust::get_self_metadata()` and
  `fork()` are fallible too; `clank-embed/src/golem_cluster.rs` propagates the errors.
- The rebase itself conflicted only on the `command_handler/worker` → `agent` directory rename
  (resolved once; `git -c merge.directoryRenames=true rebase --continue` relocated the rest) and
  the app lists in `test-components/build-components.sh`. The `agent shell` code then needed a
  rename pass against upstream: `WorkerCommandHandler` → `AgentCommandHandler`, `AgentNameMatch`
  → `crate::model::agent::AgentIdMatch` (field `agent_id`), `format_agent_name_match` →
  `format_agent_id_match`, `match_agent_name` → `match_agent_id`, `component_by_agent_name_match`
  → `component_by_agent_id_match`, `validate_worker_and_function_names` →
  `validate_agent_and_function_names(&component, &RawAgentId, None)`.
- Toolchain drift, not the rebase: clippy 1.98 (`channel = "stable"`) added
  `unused_async_trait_impl` (three cfg-gated `async fn`s gained scoped allows) and flagged
  `EmbeddedShell::ensure`'s `Err(EvalResult)` as `result_large_err` (now `Box<EvalResult>`).
- The newer CLI refuses confirmed actions on a non-interactive stdin: `golem server run --clean`
  needed `-Y` in `scripts/golem-e2e.sh` and `scripts/conformance-golem.sh` (`build`/`deploy`
  already had it). RC_TESTING.md's interactive flow is unaffected.
- The clone's `cargo build -p golem` from scratch took ~35 minutes and the debug binary is ~940 MB.
- **Hosting-safety finding (goes to ticket 12's gate):** the first e2e run against the rebuilt
  toolchain wedged the `ClankAgent` instance at the transcript-eviction setup, and 230 later
  assertions failed with empty output ("Previous invocation failed"). Cause: Rust ≥1.88 stores
  `core::fmt` widths as `u16`, so `printf '%150000s' ''` panics in the uucore fork's
  `write_padded` (`Argument::from_usize`), and a panic traps the durable instance for good.
  Bisected: width 65000 works, 70000 traps. The e2e now builds its oversized entry from three
  conversions under the limit; the real fix is in the fork's `printf` padding (pad without a
  `fmt` width) plus a rev bump, and this is exactly the trap class ticket 12's panic-hygiene gate
  exists for.
- **The scope grew by one unplanned migration: `wstd` → `wasi-fetch`.** The rebased golem-rust
  drives agent methods with wit-bindgen's async runtime and has dropped `wstd` as a dependency
  altogether. wstd's HTTP client resolves its reactor from a thread-local that only
  `wstd::block_on` installs, so on this SDK *every* outbound request panics
  `Reactor::current must be called within a wstd runtime` (wstd-0.6.5/src/runtime/reactor.rs:115)
  and traps the instance — `curl`, `wget`, `ask` and MCP/grease alike. The second e2e run reached
  80/272 and wedged at the first approved `curl`. All three transports now use
  **`wasi-fetch = "=0.2.0"`** over the wasip3 bindings, the client upstream uses in its own agent
  components (`test-components/agent-rpc`, `test-components/http-tests`) and pins in the app
  template the CLI generates (`cli/golem-cli/tests/app/agents.rs`):
  `utilities/whttp/src/lib.rs` (`fetch_once`'s wasm arm — `curl`/`wget`),
  `crates/clank-embed/src/mcp_http.rs` (`WstdMcpHttp` → `WasiFetchMcpHttp`), and
  `crates/clank-embed/src/ask_provider.rs`. Three consequences worth carrying forward:
  - Wall C is **unchanged, for a restated reason**. It is no longer "the wstd reactor is not live";
    it is that a WASI-HTTP future polled by clank's nested tokio `rt.block_on` is never woken,
    because nothing there performs the component-model wait. Every doc that stated the old reason
    was corrected.
  - Two wit-bindgen crates now coexist in the component — 0.57.1 (under `wasip3`/`wasi-fetch`) and
    the golemcloud fork 0.59.0 (under `golem-rust`). This is exactly what upstream's own agent
    test components ship, so it is a supported configuration rather than a lucky resolution.
  - `wasi-fetch` pulls in `url` → `idna` → `icu`, the Unicode-table stack `whttp` deliberately
    avoided by choosing `iri-string`. Only its redirect resolver uses it and clank sets
    `redirect_limit(0)`, so the path is dead code — but dead code still has to be stripped by LTO.
    Worth a size check against ticket 12's budget. (Debug wasm went *down* 2 MB net, since wstd
    and its async-executor stack left: the lock is −8 packages, +1.)

  **Verified live**, not merely compiled: `sudo curl -sI` returns real response headers, `curl -s`
  the body, `curl -s | grep -c` composes through the Wall C pipeline, and the instance answers
  normally afterwards. Full `scripts/golem-e2e.sh`: **272 passed, 0 failed.**
- **Two harness defects cost more time than the migration and both can fabricate results.** Neither
  is a code bug; both make a healthy agent look broken, and one makes a broken agent look tested:
  - `golem build` **silently skipped `clank:agent`** (`[UP-TO-DATE]`) while rebuilding the greeter,
    so an e2e run deployed a wasm predating every source edit and reported a confident 80/272
    against code that was never compiled. The e2e pipes `golem -Y build` through `tail -4`, which
    hides the skip line. A guard comparing artifact mtime against sources before deploy would have
    caught it; worth adding to the harness.
  - `scripts/golem-probe.sh` was the **third** reader still on the pre-rc-1 invoke JSON shape
    (`result_json` + named fields) after golem-e2e.sh and `clank-conformance`'s golem backend were
    fixed. Both mismatches fail silently, so every probed command prints empty and a healthy agent
    reads as stone dead. Fixed by lifting the e2e's `EVAL_REMAP` in. The lesson generalizes: when a
    golem-facing reader shows empty output, suspect the JSON shape before the agent, and fix these
    readers as a SET (`grep -rn result_json`) rather than one at a time.

---

### 2. Test infrastructure: `echo-tool` fixture, conformance `tools` tier, e2e, CI

Every later ticket's acceptance test runs against one fixture tool that exercises every
projection construct, deployed by the conformance harness and the e2e script. The harness
already parses `@requires <tier>` but does not wire any tier
(`crates/clank-conformance/src/harness.rs:113-118`); this ticket wires `tools`.

**`fixtures/echo-tool/src/lib.rs`** — the fixture, authored with the SDK macros:

```rust
use golem_rust::agentic::{InputStream, OutputStream};
use golem_rust::{tool_definition, tool_implementation, ToolError};

#[derive(golem_rust::IntoSchema, golem_rust::FromSchema, Clone, Copy)]
pub enum Color { Auto, Always, Never }

#[derive(golem_rust::IntoSchema, golem_rust::FromSchema)]
pub struct Meta { pub k: String, pub n: u32 }

#[derive(ToolError, Debug)]
pub enum FailError {
    #[tool_error(kind = "usage-error", exit_code = 2)]   BadInput { reason: String },
    #[tool_error(kind = "runtime-error", exit_code = 7)] Boom,
}

/// Echo fixture: one command per projection construct.
#[tool_definition(version = "1.0.0")]
pub trait EchoTool {
    /// Root body: prints its positionals, honours the globals.
    #[arg(color = "global", default = "auto")]
    #[arg(verbose = "global", short = 'v', kind = "count-flag", max = 3)]
    #[arg(words = "tail", min = 0)]
    async fn echo_tool(&self, color: Color, verbose: u32, words: Vec<String>, stdout: OutputStream);

    /// Greet: scalar option + negatable flag.
    #[command(annotations(read_only = true, idempotent = true))]
    #[arg(name = "positional")]
    #[arg(times = "option", short = 'n', default = "1", min = 1, max = 9)]
    #[arg(shout = "flag", negatable = true, default = false)]
    async fn greet(&self, color: Color, verbose: u32, name: String, times: u32, shout: bool, stdout: OutputStream);

    /// Grep: repeatable(either) option, delimited list, tail after `--`.
    #[arg(pattern = "option", short = 'e', repeatable = "either", delim = ',')]
    #[arg(items = "option", repeatable = "delimited", delim = ',')]
    #[arg(files = "tail", separator = "--", min = 0, verbatim = true)]
    async fn grep(&self, color: Color, verbose: u32, pattern: Vec<String>, items: Vec<String>, files: Vec<String>, stdout: OutputStream);

    /// Upper: required stdin → stdout.
    async fn upper(&self, color: Color, verbose: u32, stdin: InputStream, stdout: OutputStream);

    /// Fail: declared error cases; `--usage` selects the usage-kind case.
    #[arg(usage = "flag")]
    async fn fail(&self, color: Color, verbose: u32, usage: bool) -> Result<(), FailError>;

    /// Tree: subcommand subtree with an alias.
    #[command(subtree = Tree, aliases = ["t"])]
    fn tree(&self, color: Color, verbose: u32) -> Tree;

    /// List: read-only; no stdout stream, structured result.
    #[command(annotations(read_only = true))]
    async fn list(&self, color: Color, verbose: u32) -> Vec<String>;

    /// Destroy: destructive annotation.
    #[command(annotations(destructive = true))]
    async fn destroy(&self, color: Color, verbose: u32) -> String;
}

#[tool_definition]
pub trait Tree {
    /// Add: repeatable-map option and a record-typed option.
    #[arg(entries = "option", short = 'c', repeatable = "repeated")]   // k=v map
    #[arg(meta = "option")]                                             // Meta record → JSON literal on the CLI
    #[constraint(all_or_none = ["meta", "entries"])]
    async fn add(&self, entries: Vec<(String, String)>, meta: Option<Meta>) -> Meta;
}
```

Plus `CapableEcho` (`write <path> <text>` / `read <path>`) in the same crate, declared with a
provisioned marker so it is filesystem-capable, and the implementations (`#[tool_implementation]`)
that echo their parsed inputs so every acceptance test can assert what the tool received.

**`golem.yaml`** — declare and bind:

```yaml
components:
  fixtures:echo-tool:
    dir: fixtures/echo-tool
    templates: rust
tools:
  echo-tool: {}
  capable-echo:
    files:
      - sourcePath: fixtures/echo-tool/marker
        targetPath: /.echo/MARKER
agents:
  ClankAgent:
    tools:
      echo-tool: {}
      capable-echo: {}
```

**`fixtures/echo-tool/discover-tools.snapshot.json`** — the tool's `discover-tools` output,
regenerated by `scripts/tool-snapshot.sh` (runs the component under `wasmtime` the way
`golem build` introspects it) and asserted by a test so metadata drift is caught.

**`crates/clank-conformance/src/harness.rs`** — wire the tier: `CLANK_CONFORMANCE_TOOLS=1`
un-ignores scenarios tagged `@requires tools`; `scripts/conformance-golem.sh` deploys the
fixture when the variable is set (it already deploys the app). `build_trials` keeps its
"ignored with reason" behaviour for the tier when the variable is absent. The native backend
sets `CLANK_APP_MANIFEST` to `crates/clank-conformance/fixtures/golem.yaml` (a copy of the
bindings block) before constructing its `Session`, so the native exit-4 scenarios (ticket 4) see
the same tool names the golem tier does.

**`scripts/golem-e2e.sh`** — `--with-tools` deploys the fixture and runs the tool assertions
tickets 4–8 add.

**`.github/workflows/conformance.yml`** — a `workflow_dispatch` job that builds `golem` from a
pinned upstream commit (cached by commit hash) and runs `scripts/conformance-golem.sh` with
`GOLEM_BIN` pointing at it; `scripts/conformance-golem.sh` fails fast naming the minimum version
when it finds a 1.5.x binary. `docs/TESTING.md` documents the variable, the flag and the job.

**Tests** — the fixture builds on `wasm32-wasip2`; after deploy `golem tool list` shows both
tools; the snapshot test passes; a raw-client call to `capable-echo write /x hi` then `read /x`
returns `hi` and the same against `echo-tool` fails with an ordinary error (no preopens).

**Done when:** the fixture is deployed by both scripts, the `tools` tier exists (ignored without
the variable, running with it), the CI job has run green once, and the snapshot test guards the
metadata.

---

### 3. Core model and the `ToolInvoker` seam

Adds the types in §Architecture to clank-core and both providers. Nothing is parsed or
dispatched yet; this ticket is the compile-time boundary every later one builds on.

**`crates/clank-core/src/golem/tool/mod.rs`** — the types above verbatim, plus:

```rust
pub mod errors;                     // exit codes and error-case matching (used from ticket 6)

impl ToolFailure {
    /// The root type name of a `ToolError` payload, for error-case matching.
    pub fn type_name(&self) -> Option<&str> {
        match self { ToolFailure::ToolError { type_name, .. } => type_name.as_deref(), _ => None }
    }
}

impl ToolDefinition {
    pub fn node(&self, idx: CommandIndex) -> &CommandNode { &self.commands[idx as usize] }
    pub fn child(&self, idx: CommandIndex, token: &str) -> Option<CommandIndex> {
        self.node(idx).subcommands.iter().copied()
            .find(|c| { let n = self.node(*c); n.name == token || n.aliases.iter().any(|a| a == token) })
    }
    /// Globals inherited by `idx`, root first.
    pub fn inherited_globals(&self, idx: CommandIndex) -> Vec<&Globals>;   // walks parent links built at construction
}
```

**`crates/clank-core/src/golem/tool/errors.rs`**:

```rust
pub fn resolve_error_case<'a>(def: &ToolDefinition, body: &'a CommandBody, type_name: Option<&str>) -> Option<&'a ErrorCase> {
    let name = type_name?;
    body.errors.iter().find(|e| matches!(e.payload.map(|i| &def.schema[i as usize]),
        Some(TypeNode::Named { name: n, .. }) if n == name))
}

pub fn exit_code_for(failure: &ToolFailure, matched: Option<&ErrorCase>) -> u8 {
    match (failure, matched) {
        (ToolFailure::ToolError { .. }, Some(case)) => case.exit_code,
        (ToolFailure::ToolError { .. }, None) => 1,
        (ToolFailure::InvalidInput(_), _) => 2,
        (ToolFailure::Denied(_), _) => 3,
        (ToolFailure::NotFound(_), _) => 127,
        (ToolFailure::Protocol(_), _) | (ToolFailure::ResourceExhausted(_), _) => 1,
        (ToolFailure::Cancelled, _) => 130,
        (ToolFailure::Unavailable(_), _) => 4,
    }
}

pub fn message_for(tool: &str, path: &[String], failure: &ToolFailure, matched: Option<&ErrorCase>) -> String;  // the §Error mapping stderr column
```

**`crates/clank-core/src/session/mod.rs`** — a `tool_invoker: Option<Box<dyn ToolInvoker>>`
field beside `agent_invoker` (`:251-282`) and `pub fn set_tool_invoker(&mut self, inv: Box<dyn
ToolInvoker>)` beside `set_agent_invoker` (`:473`).

**`crates/clank-core/src/native.rs`** — `UnavailableToolInvoker` (discover → `Ok(vec![])`,
invoke → `Err(ToolFailure::Unavailable("agent tools need a Golem host".into()))`), injected in
`inject_native_providers` (`:642`).

**`crates/clank-embed/src/tool_invoker.rs`** (behind `providers`) — the wasm provider:

```rust
use golem_rust::golem_agentic::golem::tool::host as tool_host;   // create_stdout, create_stdin_from_stream, get_tool

pub struct AmbientToolInvoker { wire: RefCell<HashMap<String, golem_rust::agentic::Tool>> }  // wire defs kept for encoding

impl AmbientToolInvoker {
    pub fn definition_from_wire(tool: &golem_rust::agentic::Tool) -> ToolDefinition;   // one arm per WIT record/variant arm; unknown type nodes → TypeNode::Opaque
    pub fn encode(value: &ToolValue, wire: &golem_rust::agentic::Tool, node: TypeNodeIndex) -> Result<TypedSchemaValue, String>;   // one value against one wire node; JsonLiteral decoded against it
    /// The canonical input record: resolves `command_path` to its body, lists inherited globals +
    /// body params with their wire nodes in canonical order, synthesizes the record schema (the
    /// shape `canonical_input_record_schema` produces), and encodes each field with `encode`.
    pub fn encode_input(record: &ToolValue, wire: &golem_rust::agentic::Tool, command_path: &[String]) -> Result<TypedSchemaValue, String>;
    pub fn decode(value: &TypedSchemaValue) -> (ToolValue, Option<String>);              // value + root def name
}

#[async_trait::async_trait(?Send)]
impl ToolInvoker for AmbientToolInvoker {
    async fn discover(&self) -> Result<Vec<RegisteredTool>, String> {
        golem_rust::agentic::get_all_tools().into_iter().map(|t| Ok(RegisteredTool {
            name: t.commands.nodes[0].name.clone(), component: t.implemented_by.to_string(),
            definition: Self::definition_from_wire(&t.definition) })).collect()
    }
    async fn invoke(&self, call: ToolCall, sink: &mut dyn ToolOutputSink) -> Result<ToolOutcome, ToolFailure> {
        let rpc = golem_rust::agentic::AmbientToolRpc::new(&call.tool);
        let stdout = call.expects_stdout.then(|| tool_host::create_stdout());       // (target, reader)
        let stdin = call.stdin.map(|bytes| tool_host::create_stdin_from_stream(finite_stream(bytes)));
        let wire = self.wire.borrow().get(&call.tool).cloned().ok_or_else(|| ToolFailure::NotFound(call.tool.clone()))?;
        let input = Self::encode_input(&call.input, &wire, &call.command_path).map_err(ToolFailure::InvalidInput)?;
        let fut = rpc.invoke_and_await(call.command_path, input, stdin, stdout.map(|s| s.0));
        // drain stdout concurrently with the terminal wait; chunks reach the sink as they arrive
        let (result, ()) = futures::join!(fut, drain(stdout.map(|s| s.1), sink));
        map_result(result)   // rpc-error arms → ToolFailure arms; custom-error → ToolError { type_name, payload }
    }
}

/// One `Ok(bytes)` item followed by closure: a clean EOF, so the tool sees exactly the staged bytes.
fn finite_stream(bytes: Vec<u8>) -> wit_stream::StreamReader<ByteStreamItem>;

/// `ToolInvocationStdout::next()` until `None`; every `Ok(chunk)` goes to `sink.write_stdout`;
/// an `Err(failure)` item ends the drain (the terminal reports it through `map_result`).
async fn drain(reader: Option<ToolInvocationStdout>, sink: &mut dyn ToolOutputSink);

/// `Ok(r)` → `ToolOutcome { result: r.result.map(|v| Self::decode(&v).0) }`;
/// `Err(remote-tool-error(custom-error(v)))` → `ToolError { type_name, payload }` via `decode`;
/// `Err(remote-tool-error(invalid-input | constraint-violation | invalid-command-path | invalid-result))` → `InvalidInput`;
/// `denied` / `not-found` / `protocol-error` / `remote-internal-error` / `cancelled` / `resource-exhausted` → the same-named arms.
fn map_result(r: Result<InvocationResult, RpcError>) -> Result<ToolOutcome, ToolFailure>;
```

`with_default_golem_providers` (`clank-embed/src/shell.rs:73`) injects it beside `WasmRpcInvoker`.

**Tests** — clank-embed unit tests: `definition_from_wire` on ticket 2's snapshot round-trips
every command, option, flag, positional, constraint, stream spec, error case and annotation
(`assert_eq!` against a hand-written `ToolDefinition` of the fixture); `encode` of every
`ToolValue` arm decodes back to the same `SchemaValue` through golem-schema; `map_result`
covers every `rpc-error` arm with a table. clank-core: `exit_code_for` over every row of the
error table; `resolve_error_case` finds `BadInput`/`Boom` by name and returns `None` for an
unnamed payload. `cargo tree -p clank-core | grep -c golem` is `0`.

**Done when:** all three crates compile on both targets, the tests above pass, and clank-core
has no golem crate in its graph.

---

### 4. Discovery → `$PATH`, manifests, `--help`/`type`/`which`/`man`, native stubs

Bound tools become visible everywhere a command is visible, on both targets. Invocation is
ticket 6.

**`crates/clank-core/src/runtime/toolstate.rs`**:

```rust
pub struct ToolState { tools: Vec<RegisteredTool>, version: u64 }
impl ToolState {
    pub fn replace(&mut self, tools: Vec<RegisteredTool>) { self.tools = tools; self.version += 1; }
    pub fn version(&self) -> u64;
    pub fn definition(&self, name: &str) -> Option<&ToolDefinition>;
    pub fn manifest_for(&self, name: &str) -> Option<Manifest> { self.definition(name).map(manifest_for) }   // the fourth source for ticket 6's resolvers
    pub fn all_manifests(&self) -> Vec<Manifest> { self.tools.iter().map(|t| manifest_for(&t.definition)).collect() }
    pub fn registered(&self) -> &[RegisteredTool];
}
```

`Session::new` calls `invoker.discover()` once and stores the result; the dynamic-registry cache
key at `session/mod.rs:669-696` becomes `(mcp.version(), grease.version(), tools.version())`.

**`crates/clank-core/src/golem/tool/manifest.rs`**:

```rust
pub fn manifest_for(def: &ToolDefinition) -> Manifest {
    fn build(def: &ToolDefinition, idx: CommandIndex, path: &[String]) -> Manifest {
        let node = def.node(idx);
        let policy = match node.body.as_ref().and_then(|b| b.annotations) {
            Some(a) if a.read_only && !a.destructive => AuthorizationPolicy::Allow,
            None if node.body.is_none() => AuthorizationPolicy::Allow,   // pure dispatcher: help only
            _ => AuthorizationPolicy::Confirm,                            // destructive, open-world, or unset
        };
        let mut m = Manifest::builtin(&node.name)
            .with_scope(ExecutionScope::Subprocess)
            .with_policy(policy)
            .with_params(param_specs(def, idx))                           // globals + body params → ParamSpec
            .with_help(render_help(def, path).unwrap_or_default());
        m.subcommands = node.subcommands.iter().map(|c| {
            let mut p = path.to_vec(); p.push(def.node(*c).name.clone()); build(def, *c, &p)
        }).collect();
        m
    }
    build(def, 0, &[def.name.clone()])
}

/// One `ParamSpec` per canonical input field of `idx` (inherited globals, then the body's
/// positionals, options and flags), built the way `mcp/state.rs:193 schema_to_params` builds them
/// from an MCP input schema: name, required, and the field's `doc.summary`.
fn param_specs(def: &ToolDefinition, idx: CommandIndex) -> Vec<ParamSpec>;
```

**`crates/clank-core/src/golem/tool/help.rs`** — `pub fn render_help(def: &ToolDefinition, path:
&[String]) -> Result<String, String>`: resolves the path, prints `summary`, a synthesized usage
line (`echo-tool grep [OPTIONS] -e <PAT>... [-- <files>...]`), `description`, then sections
`Commands` (name, aliases, summary), `Arguments`, `Options` (short/long, value name, default,
env var, repeatable form), `Flags`, `Streams` (stdin/stdout MIME and required), `Result`
(formatters with the default marked), `Errors` (name, kind, exit code), `Annotations`, and
`Examples`. Unknown path → `Err("unknown command: ...")`.

**`crates/clank-core/src/grease/config.rs`** — `pub const TOOLS_BIN: &str = "/usr/lib/tools/bin";`
appended to `DEFAULT_PATH` after `/usr/lib/mcp/bin`; the stub writer mirrors
`mcp/config.rs:187 write_bin_stub` (a two-line shebang stub; the Session intercepts the name
before Brush would execute it, exactly as for MCP servers).

**`crates/clank-core/src/session/mod.rs:857`** — the dynamic help hook tries `tool_help_for`
(`<tool> [sub…] --help` and `man <tool>`) before `pkg_help_for`.

**`crates/clank-core/src/builtins/typecmd.rs`** — `type <tool>` prints `<tool> is an agent tool
(bound; component <component>)`; `which` needs no change (it finds the stub).

**`crates/clank-core/src/native.rs`**:

```rust
pub fn tool_names_from_app_manifest(path: &Path) -> Result<Vec<String>, String>;   // agents.ClankAgent.tools keys, serde_yaml
```

Native `Session::new` reads `CLANK_APP_MANIFEST` or `./golem.yaml`; for each name it registers a
`Manifest` whose help text is `<name>: agent tool bound in golem.yaml; run inside a Golem host`
and writes the stub; invoking it (ticket 6's dispatch) exits 4 with that message.

**Tests** — golem tier (`scenarios/tools-discovery.clank`, `@requires tools`, `@only golem`):
`ls /usr/lib/tools/bin` lists both tools; `which echo-tool`; `type echo-tool`; `echo-tool --help`,
`echo-tool grep --help`, `echo-tool tree add --help` and `man echo-tool` contain every section
above; `golem agent simulate-crash` then `type echo-tool` still resolves. Native scenario:
with a fixture `golem.yaml`, `type echo-tool` resolves and `echo-tool` exits 4 with the message;
without one, `echo-tool` is exit 127. Unit: `manifest_for` policies (`list` Allow, `destroy`
Confirm, `fail` Confirm, `tree` Allow); `render_help` golden files for the three depths.

**Done when:** both scenarios are green and `grease list` (ticket 8 adds the kind column) can
enumerate `ToolState`.

---

### 5. The projection: argv parser, coercion, constraints, canonical record

The heart of the consumer direction: Brush's argv against a `ToolDefinition`, producing the
canonical input record. Written to be exhaustive on the first pass because every construct is
independently testable against the fixture; splitting scalars from repeatables would only
create an interim UX that contradicts the model.

**`crates/clank-core/src/golem/tool/argv.rs`**:

```rust
pub struct ParsedInvocation {
    pub command_index: CommandIndex,
    pub command_path: Vec<String>,
    pub input: ToolValue,            // Record, canonical order
    pub wants_stdin: bool,
    pub expects_stdout: bool,
}

pub enum ArgvError { Usage(String), Help(String) }

pub fn parse(def: &ToolDefinition, argv: &[String], env: &dyn Fn(&str) -> Option<String>) -> Result<ParsedInvocation, ArgvError>
```

Algorithm:

```
1. walk:      node = 0, path = [def.name]
              while argv[i] matches def.child(node, argv[i]): node = child; path.push(argv[i]); i += 1
              body = node.body else Usage("expected one of: <children>")
2. specs:     inherited globals (root → node) ++ body.options ++ body.flags
              long names, aliases and shorts index into one table (unique by construction)
3. scan:      "--help" | "-h" (when no spec owns 'h')     → Help(render_help(def, path))
              tail.separator == Some(sep) && tok == sep    → everything after is tail; verbatim ⇒ no flag parsing
              "--"  (no declared separator)                → everything after is positional
              "--name=value" | "--name value"              → option value (Usage if the option is a flag)
              "--no-name"                                  → negatable flag = false (Usage if not negatable)
              "-s value" | "-svalue" | "-abc" (bundled)    → shorts; a short option consumes the rest of its token or the next token
              "-vvv"                                       → count flag (Usage above `max`)
              bare "--name" on OptionalScalar               → the option's `default`
              anything else                                → positional queue
4. assign:    fixed positionals in order; remainder → tail (Usage below `min` / above `max`; Usage if no tail)
5. fill:      for every missing option/flag: env_var → default → (required ⇒ Usage("missing --name"))
              repeatables: Repeated collects occurrences; Delimited(c) splits one occurrence; Either(c) does both
              RepeatableMap: each item is `k=v` (Usage otherwise); duplicate keys per DuplicateKeyPolicy
6. constrain: evaluate body.constraints over `present(name)` / `value-is(name, lit)`; Usage names the arguments
7. coerce:    each raw string against def.schema[node] (table below); Usage on failure
8. record:    input = Record([(field, value)…]) in canonical order:
              inherited globals root→node (options then flags), then body positionals (fixed, tail),
              body options, body flags — pinned by the goldens below and confirmed upstream in ticket 9
```

Coercion table (raw command-line text → `ToolValue`):

| `TypeNode` | Accepts | Produces |
|---|---|---|
| `Bool` (positional) | `true/false/yes/no/1/0` | `Bool` |
| `Int { signed, bits, min, max }` | decimal within range | `Int`/`UInt` |
| `Float` | decimal / exponent | `Float` |
| `Char` | one scalar | `Str` (single char) |
| `Str { regex, min_len, max_len }` | text matching the constraints | `Str` |
| `Bytes` | text (UTF-8 bytes) | `Bytes` |
| `Enum(cases)` | one of the cases (kebab, case-insensitive) | `Enum` |
| `List(t)` | from repeatable / delimited / tail items | `List` of coerced `t` |
| `Map { k, v }` | `k=v` items | `Map` |
| `Option(t)` | present ⇒ `Some(coerce t)`, absent ⇒ `None` | `Option` |
| `Record`, `Variant`, `Opaque` | a JSON literal | `JsonLiteral` |
| `Named { inner }` | as `inner` | as `inner` |

**Tests** — unit, table-driven, over a `ToolDefinition` of ticket 2's fixture and over the SDK's
`tool_canonical.rs` Grep and Git tools transcribed as fixtures (`golem/tool/fixtures/{grep,git}.rs`):
the subcommand walk (alias, stop at first non-child, dispatcher usage error); every option form;
`--no-x`; `-vvv` and `max`; `Repeated`/`Delimited`/`Either`; `--` and `verbatim`; fixed vs tail
counts; env-var and default precedence; each constraint kind passing and failing with the
argument names in the message; every coercion row; `Help` at three depths. **Goldens:**
`golem/tool/goldens/*.json` hold the canonical record for six invocations of the Grep/Git
fixtures with the SDK commit hash in a header comment; a test compares `parse` output
field-for-field, in order. Golem tier: `scenarios/tools-args.clank` runs the same invocations
against the deployed fixture and asserts what the tool echoes back.

**Done when:** every row of the coercion table and every constraint kind has a passing unit test,
the goldens match, and `tools-args.clank` is green.

---

### 6. Session dispatch: pipelines, result rendering, error mapping, audit

Connects the projection to the Session so a tool line runs like any other command, with the
`curl`/`ask` pipeline rule, the output rule, the error table and the audit trail.

**`crates/clank-core/src/session/tool.rs`** (mirrors `session/agent.rs`):

```rust
impl Session {
    pub(super) fn is_tool_line(&self, line: &str) -> bool;            // first word (after an optional leading `sudo`, as is_mcp_tool_line handles it) ∈ ToolState, not shadowed by an intercepted builtin
    pub(super) async fn run_tool(&mut self, line: &str, staged_stdin: Option<Vec<u8>>) -> LineResult {
        let argv = split_argv(line);                                     // the helper run_mcp_tool uses
        let def = self.tools.definition(&argv[0]).expect("classified");  // is_tool_line guarantees it
        let parsed = match parse(def, &argv[1..], &|k| self.env_value(k)) {   // env_value: accessor over the Brush shell environment (add if absent)
            Ok(p) => p,
            Err(ArgvError::Help(text)) => return LineResult::continue_with_stdout(text),
            Err(ArgvError::Usage(msg)) => return LineResult::from_outcome("", format!("{}: {msg}", argv[0]), 2),
        };
        let body = def.node(parsed.command_index).body.as_ref().expect("parse returned a body");
        if parsed.wants_stdin && staged_stdin.is_none() && body.stdin.as_ref().is_some_and(|s| s.required) {
            return LineResult::from_outcome("", format!("{}: stdin required", argv[0]), 2);
        }
        let call = ToolCall { tool: argv[0].clone(), command_path: parsed.command_path.clone(),
                              input: parsed.input, stdin: staged_stdin, expects_stdout: parsed.expects_stdout };
        let mut sink = PipeSink::default();                              // ToolOutputSink into the in-memory pipeline pipe
        let started = std::time::Instant::now();                         // durable under Golem (intercepted monotonic clock)
        let Some(invoker) = self.tool_invoker.as_ref() else {
            return LineResult::from_outcome("", format!("{}: agent tools need a Golem host", argv[0]), 4);
        };
        let result = invoker.invoke(call, &mut sink).await;
        let (stdout, stderr, exit) = match result {
            Ok(outcome) => (if parsed.expects_stdout { sink.into_string() } else { render_result(outcome.result) }, String::new(), 0),
            Err(failure) => {
                let matched = errors::resolve_error_case(def, body, failure.type_name());
                (sink.into_string(), errors::message_for(&argv[0], &parsed.command_path, &failure, matched), errors::exit_code_for(&failure, matched))
            }
        };
        // audit: the same logging::Record path session/agent.rs:185 uses for "agent-invoke",
        // with fields tool, path, exit, ms = started.elapsed().as_millis()
        self.record_ops_event("tool-invoke", &argv[0], &parsed.command_path, exit, started.elapsed());
        LineResult::from_outcome(stdout, stderr, exit)                   // the transcript records line + stdout as for every command
    }
}

impl Session {
    /// Builds the `logging::Record` exactly as `session/agent.rs:185` does for "agent-invoke" and
    /// hands it to the injected `LogSink` (which is what writes /var/log/ops.log).
    fn record_ops_event(&self, kind: &str, tool: &str, path: &[String], exit: u8, elapsed: std::time::Duration) {
        let record = logging::Record::new(kind)
            .with("tool", tool).with("path", path.join(" ")).with("exit", exit.to_string()).with("ms", elapsed.as_millis().to_string());
        self.log_sink.write(record);
    }
}

/// Collects the tool's stdout chunks; the pipeline pipe reads the collected bytes as the stage's output.
#[derive(Default)]
struct PipeSink(Vec<u8>);
impl ToolOutputSink for PipeSink { fn write_stdout(&mut self, bytes: &[u8]) { self.0.extend_from_slice(bytes); } }
impl PipeSink { fn into_string(self) -> String { String::from_utf8_lossy(&self.0).into_owned() } }

fn render_result(v: Option<ToolValue>) -> String {                       // never printed when a stdout stream was declared
    match v { None => String::new(), Some(ToolValue::Str(s)) => s + "\n", Some(other) => canonical_json(&other) + "\n" }
}

/// The Brush word-splitting `run_mcp_tool` already applies to the raw line
/// (`mcp/cmd.rs:200 parse_tool_invocation`'s tokenizer); quotes resolved, no expansion.
fn split_argv(line: &str) -> Vec<String>;

/// `ToolValue` → JSON with sorted object keys: `Map` is an object when every key is a `Str`,
/// otherwise an array of `[k, v]` pairs; `Bytes` is base64; `Enum` is its case name; `JsonLiteral`
/// is emitted as-is.
fn canonical_json(v: &ToolValue) -> String;
```

**`crates/clank-core/src/session/mod.rs`** — the classifier (`:1005-1017`) adds `is_tool_line`
after `is_mcp_tool_line` and before `is_agent_line`; the head-split in `run_command` (the
`curl` case) treats a tool line as a Session-layer stage: head position feeds its stdout into
the Brush tail (`echo-tool greet Ada | tr a-z A-Z`), tail position receives the pre-extracted
pipeline bytes as `staged_stdin` (`cat f | echo-tool upper`, the `ask` path), middle position
returns the same honest error `curl` returns (exit 2). Flag-argument redaction
(`log_safe_line`) already covers `--token abc` in the audit record.

**`crates/clank-core/src/session/ask.rs:22,464,494`** — the three manifest resolvers
(`resolve_command_scope`, `resolve_authz`, `resolve_authz_strictest`) consult a fixed chain —
static registry, then `mcp.manifest_for`, then `grease.manifest_for` — and would never see a
tool. Each gains a fourth source, `self.tools.manifest_for(name)` (subcommand-aware through the
manifest's `subcommands`, like the others), so the interactive `Confirm` prompt and the model gate
both apply to tool lines. `ToolState::manifest_for(&self, name: &str) -> Option<Manifest>` is added
to ticket 4's `ToolState`.

Binary stdout: `LineResult` carries `String` output today, so a tool declaring an `image/*` or
`application/octet-stream` stdout is rendered lossily (as `curl` is today). Recorded as a known
limitation in ticket 8; binary-safe stage output is not in this plan.

**`crates/clank-core/src/builtins/kill.rs`** — `tool &` runs through the existing job machinery;
`kill %n` on a tool job prints `detached (tool invocation completes on the host)` and drops the
job, because a synchronous `invoke-and-await` cannot be cancelled once accepted.

**Tests** — golem tier `scenarios/tools-invoke.clank`, `tools-pipeline.clank`,
`tools-errors.clank`: `echo-tool greet Ada` → `hello Ada` exit 0; `echo-tool tree add -c a=1
--meta '{"k":"x","n":1}'` prints canonical JSON (result, no stream); `echo-tool greet Ada` prints
only the stream; head/tail/middle pipeline forms; `echo-tool fail --usage` exit 2 and `echo-tool
fail` exit 7 with `echo-tool fail: boom` on stderr; a tool unbound-and-redeployed → exit 127;
`echo-tool greet Ada &` then `kill %1` prints the detach message; `/var/log/ops.log` has one
`tool-invoke` line per invocation with `--token <redacted>`. Native scenario: `echo-tool greet
Ada` exits 4 with the host message.

**Done when:** the three scenarios and the native one are green and the audit assertion holds
in `golem-e2e.sh --with-tools`.

---

### 7. `ask` exposure and authorization from annotations

The model reaches bound tools through the existing `shell` tool plus the system prompt's
capability section; the annotation-derived policies from ticket 4 gate it. No structured tool
definitions in this plan.

**`crates/clank-core/src/ai/ask.rs:144`** — `build_system_prompt_with_capabilities` appends a
section:

```
Agent tools available as commands (run `<tool> --help` for usage):
  echo-tool — Echo fixture: one command per projection construct.
  capable-echo — ...
```

sourced from `ToolState::registered()` (`definition.commands[0].doc.summary`).

**`crates/clank-core/src/session/ask.rs:1038-1075`** — the model gate itself is unchanged: with
ticket 6's fourth manifest source in place, `resolve_command_scope` returns `Subprocess` for a
tool line (⇒ allowed) and `authz::decide` applies the per-subcommand policy (`authz::resolve`
is subcommand-aware, `authz.rs:123`). This ticket's work is the prompt section plus the proof that
the gate behaves per annotation; if the scripted tests fail, the defect is in ticket 6's source
wiring, not here.

**Tests** — unit: the prompt section text; scripted-provider tests (`session/tests/ask*.rs`
pattern) where the model's `shell` call is `echo-tool list` (read-only → runs without
confirmation), `echo-tool destroy` (destructive → `ToolStep::Pause` confirmation; with the sudo
grant it runs), `echo-tool fail` (unset → confirmation). `--with-llm` e2e: `ask "use echo-tool
to greet Ada"` — the transcript shows `echo-tool greet Ada` was run and the answer contains
`hello Ada`.

**Done when:** the three scripted cases pass and the `--with-llm` run is green (gated as today).

---

### 8. grease: deploy-time bind, host-side honesty, documentation

`grease install tool:<name>` becomes the one verb for binding a tool — a native-side manifest
edit plus deploy — while inside a host the tool kind is list/info only and says why.

**`crates/clank-core/src/grease/tool_bind.rs`**:

```rust
pub enum ToolSource { Component(String), Release { account: String, name: String, version: String } }
pub struct ManifestEdit { pub path: PathBuf, pub changed: bool, pub next_step: String }   // "golem deploy"

pub fn bind_tool(manifest: &Path, agent: &str, tool: &str, source: ToolSource) -> Result<ManifestEdit, String>;
pub fn unbind_tool(manifest: &Path, agent: &str, tool: &str) -> Result<ManifestEdit, String>;
```

Line-based editing, not a YAML round-trip (which would destroy comments): locate the top-level
`tools:` block and insert `  <tool>:\n    component: <id>` or the `release:` form at its end;
locate `agents:` → `  <agent>:` → `    tools:` (creating the `tools:` key under the agent if
absent) and insert `      <tool>: {}`; every other byte of the file is untouched. Missing
`tools:`/`agents:` structure fails with a message naming the missing key rather than guessing.
`unbind_tool` removes exactly the lines it would have inserted. Both are idempotent.

**`crates/clank-core/src/grease/cmd.rs`** — `install`/`remove`/`info` accept `tool:<name>`
with `--component <id> | --release <account>/<name>/<version>`, `--agent <Type>` (default
`ClankAgent`; third-party hosts name their own type) and `--deploy` (runs the steps via the
existing process seam on native; each exit code is surfaced). A `release:`-sourced binding is
resolved against the environment's **tool grants** at deploy (`ResolvedToolGrants`,
`cli/golem-cli/src/command_handler/component/mod.rs:813`), so for `--release` the printed steps —
and `--deploy` — are `golem tool grant create --account <a> --name <n> --version <v>` followed by
`golem deploy`; `remove` prints/runs `golem tool grant delete <grant-id>` after unbinding.
`grease install tool:clank --agent ClankAgent` is refused (`clank cannot be bound to itself`; see
ticket 10). Inside a host
(`ToolInvoker` is the ambient provider) `install`/`remove tool:` exit 4 with `bindings are
compile-time; run from native clank`. `grease list` shows `echo-tool  tool  fixtures:echo-tool@1.0.0`;
`grease info echo-tool` shows commands, annotations, and `integrity: Golem release registry
(grease signing does not apply)`.

**`dev-tools/grease-tool`** — the same verbs in the TUI.

**`README.md:677`** — type 6 becomes "Agent tools", documenting ambience, `/usr/lib/tools/bin`,
`grease install tool:`, the native exit-4 behaviour and the five known limitations (no
middle-of-pipeline, 16 MiB attachment cap, terminal-only stdout for filesystem-capable tools,
`kill` detaches, binary stdout rendered lossily). `docs/USAGE.md` gets the worked
`git commit -m fix --amend | tee log` example; the README gap-audit table gains a row per
limitation.

**Tests** — unit: `bind_tool`/`unbind_tool` on a fixture `golem.yaml` with comments — the diff
is exactly the inserted/removed lines, applying twice is a no-op, a manifest without `agents:`
fails with the named key. Native scenario `tools-grease-native.clank`; golem scenario
`tools-grease-host.clank` (exit 4 + info text).

**Done when:** both scenarios are green, `--deploy` round-trips on the dev cluster, and the
README section is rewritten.

---

### 9. Upstream contributions

Three conversations with the Golem project that the design depends on, tracked here so they
are not lost between tickets.

- **`TryFrom<wire::Tool> for ExtendedToolType`** — a PR to
  `sdks/rust/golem-rust/src/agentic/extended_tool_type.rs` with round-trip tests over the
  `tool_canonical.rs` fixtures. Once merged, `AmbientToolInvoker::encode` switches to
  `canonical_input_model` + `build_canonical_input` and `render_help` can delegate to the SDK's;
  ticket 5's goldens remain the guard until then. The PR is also where the canonical field order
  (ticket 5 step 8) is confirmed with the maintainers.
- **GOL-29 manifest key** — file or comment on the request for `filesystemAccess: allowed` on
  tool declarations/bindings, attaching ticket 10's probe findings; the marker-file workaround
  in `golem.yaml` carries a comment linking to it.
- **`agent shell` (#3700)** — obtain vouching per upstream `CONTRIBUTING.md`, re-submit rebased
  on current `main` with the `test-components/agent-shell` reference implementer green in
  upstream CI.
- **Warm-instance cache** — post ticket 10's p50/p95 per-line numbers to the gol-33 follow-up
  as a concrete workload.

**Done when:** the PR is open with tests, the GOL-29 request carries the probe data, the
vouching request is made, and the latency numbers are posted.

---

### 10. The `clank` tool: export, filesystem grant, session persistence, latency

clank's component exports a `clank` tool beside `ClankAgent`. Because a tool body runs in a
fresh Store per invocation, the session lives in the owner's filesystem between calls.

**Probe first (findings in `dev-docs/research/agent-tools-probes.md`)** — using ticket 2's
`capable-echo`: a tool declared with `files:` sees the owner root; the same tool without does
not; deleting the marker at runtime does not revoke the grant for later invocations (the verdict
is pinned at activation). Go/no-go for the rest of the ticket.

**`crates/clank-agent/src/clank_tool.rs`**:

```rust
use clank_embed::wire::EvalResult;                       // { stdout, stderr, exit_code, pending_prompt, cwd } — same as the agent method
use clank_embed::EmbeddedShell;
use clank_core::session::persist::SessionSnapshot;

/// A clank shell session that lives in this agent's filesystem.
#[tool_definition(version = "0.1.0")]
pub trait Clank {
    /// Evaluate one shell line in the persisted session.
    #[arg(line = "positional")]
    async fn clank(&self, line: String) -> EvalResult;               // implicit body: `clank "ls /"`
    /// Answer the pending prompt-user question.
    #[arg(text = "positional")]
    async fn answer_prompt(&self, text: String) -> EvalResult;       // `clank answer-prompt <text>`
    /// Abort the pending prompt-user question.
    async fn abort_prompt(&self) -> EvalResult;                      // `clank abort-prompt`
}

pub struct ClankTool;

#[tool_implementation]
impl Clank for ClankTool {
    async fn clank(&self, line: String) -> EvalResult { with_session(|s| Box::pin(async move { s.eval(&line).await })).await }
    async fn answer_prompt(&self, text: String) -> EvalResult { with_session(|s| Box::pin(async move { s.answer(Some(text)).await })).await }
    async fn abort_prompt(&self) -> EvalResult { with_session(|s| Box::pin(async move { s.answer(None).await })).await }
}

const SESSION_PATH: &str = "/.clank/session.json";

async fn with_session<F>(f: F) -> EvalResult
where F: for<'a> FnOnce(&'a mut EmbeddedShell) -> Pin<Box<dyn Future<Output = EvalResult> + 'a>> {
    let mut shell = EmbeddedShell::with_default_golem_providers();    // clank-agent builds clank-embed with `full`
    let mut warning = String::new();
    match SessionSnapshot::load(Path::new(SESSION_PATH)) {
        Ok(Some(snap)) => snap.restore(shell.session_mut()),
        Ok(None) => {}
        Err(e) => warning = format!("clank: session state unreadable ({e}); starting fresh\n"),
    }
    let mut result = f(&mut shell).await;
    if let Err(e) = SessionSnapshot::capture(shell.session()).save(Path::new(SESSION_PATH)) {
        result.stderr.push_str(&format!("clank: failed to persist session: {e}\n")); result.exit_code = result.exit_code.max(1);
    }
    result.stderr.insert_str(0, &warning);
    result
}
```

`clank_agent.rs` keeps `ClankAgent` unchanged; `lib.rs` declares `mod clank_tool;` — the
`golem-agentic` world exports both guests under `export_golem_agentic`, so no feature changes.

**`crates/clank-core/src/session/persist.rs`**:

```rust
#[derive(Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,                                 // 1; a mismatch is treated as corrupt (fresh + warning)
    pub cwd: String,
    pub last_exit_code: u8,                           // so `echo $?` on the next line sees the previous line's status
    /// The Brush shell state as a re-sourceable script, captured by running the shell's own
    /// builtins through the Session: `declare -p` (all variables with attributes, exported or
    /// not; `--secret` values are redacted to their placeholder), `declare -f` (functions),
    /// `alias -p`, `set +o`, `shopt -p`. Restored with `eval` before the line runs.
    pub shell_state: String,
    pub pending_prompt: Option<PendingPromptView>,
    pub transcript: Transcript,                       // cap-bounded already; Transcript/Elided gain serde derives
    pub tools: Option<(u64, Vec<RegisteredTool>)>,    // last discovery + version, so a line does not re-run get-all-tools
}
impl SessionSnapshot {
    pub fn capture(session: &Session) -> Self;
    pub fn restore(self, session: &mut Session);       // cd, set `$?`, eval shell_state, re-install the pending prompt, replace the transcript, seed ToolState
    pub fn load(path: &Path) -> Result<Option<Self>, String>;   // Ok(None) when the file does not exist
    pub fn save(&self, path: &Path) -> Result<(), String>;      // std::fs::create_dir_all + std::fs::write (whole-file; idempotent under replay)
}
```

The script form is what makes `x=1` on one line and `echo $x` on the next behave as in `full`
mode: Brush's `declare`, `alias`, `set` and `shopt` builtins already print their state in
re-sourceable syntax (`brush-builtins/src/{declare,alias,set,shopt}.rs`), so no Brush fork change
is needed. Background jobs are the one thing that cannot cross the file; `jobs` on a new line is
empty by construction.

**Behaviour that differs in tool mode, all documented and tagged in the corpus (ticket 12):**

- Host APIs that outlive an invocation are rejected in an entity context (gol-33 §Host
  identity): the AGENT package kind's `--schedule` and any promise-based wait return an honest
  `not available inside a tool invocation` (exit 4) instead of trapping.
- A synchronous call from the tool back into its own owner agent (invoking the host's own
  methods through the AGENT kind) would deadlock; the runtime returns an explicit error, which
  clank surfaces as-is.
- `ask`, `curl`, MCP and cross-agent calls work from inside the tool: the sidecar reuses the
  owner's host imports (`wasi:http`, `golem:agent/host`) under the owner's network authority;
  `ask`'s provider talks to Anthropic over the transport directly (`clank-embed/src/ask_provider.rs`),
  so no extra host interface is involved.
- One session per host agent: two concurrent `agent shell` sessions against the same host share
  `/.clank/session.json`; the second is a client of the same shell, not a second shell.
- Every line is a tool invocation in the **host's** oplog and is re-executed on the host's replay
  (gol-33 §Why completed tool calls execute during replay). A host with thousands of shell lines
  recovers slowly; the mitigation is snapshotting on the host (`golem-custom-snapshot`), which
  the reference host (ticket 11) demonstrates.
- `clank` must not be bound to `ClankAgent` itself (it would appear as a PATH command inside the
  shell and recurse); ticket 8's `grease install tool:clank` refuses when `--agent ClankAgent`.

`EmbeddedShell` gains `pub fn session(&self) -> &Session` and `pub fn session_mut(&mut self) ->
&mut Session`.

**`golem.yaml`** — declare the tool with the marker so it is filesystem-capable, and publish it
so third-party hosts can bind by `release:` rather than by a local component:

```yaml
tools:
  clank:
    files:
      - sourcePath: fixtures/clank-marker      # GOL-29: replace with `filesystemAccess: allowed` when available
        targetPath: /.clank/MARKER
toolReleases:
  local:
    clank: {}                                  # `golem deploy` then publishes; `golem tool release list` shows it
```

`agents.ClankAgent.tools` does **not** list `clank` (see the last behaviour note above).

**Latency** — `scripts/tool-latency.sh` runs `clank "pwd"` 50 times from a raw-client caller at
the `release` preset and prints p50/p95; the numbers go into the research doc and the threshold
in the design's Open questions is filled in.

**Tests** — from a raw `ToolRpc` caller (ticket 2's harness or a test agent): `clank "cd /tmp &&
pwd"` then `clank "pwd"` → `/tmp`; `clank "prompt-user 'Deploy?' --choices a,b"` returns
`pending_prompt`, `clank answer-prompt a` resolves it, `clank abort-prompt` clears a fresh one;
a corrupt `session.json` → next call succeeds with the warning on stderr and the line's own exit
code; `golem agent simulate-crash` on the host mid-session → `clank "pwd"` returns the pre-crash
cwd after recovery; `cargo clippy -W clippy::unwrap_used -W clippy::expect_used` reports nothing
in `clank_tool.rs` and `persist.rs`.

**Done when:** `golem tool list` shows `clank`, the five behaviours above hold, and the latency
numbers are recorded.

---

### 11. clank-embed features `tool` / `full`, greeter as reference host

`EmbeddedShell` keeps its API; a cargo feature decides whether it runs the session in-process
(`full`) or forwards to the bound `clank` tool (`tool`, the default). Deviation from design §4.4:
the shim uses `AmbientToolRpc` with ticket 3's encoder rather than the build-generated
`ClankClient`, because `golem build` emits that crate inside the *calling component's* directory
(`<component>/golem-temp/bridge-sdk/rust/internal/clank-tool-guest-client`), which a shared
library cannot depend on. The canonical record is one string field, and ticket 5's goldens guard
the encoding; the generated client is used only by the greeter integration test to cross-check
the wire shape.

**`crates/clank-embed/Cargo.toml`**:

```toml
[features]
default = ["tool"]
tool = []                                   # forward to the bound `clank` tool over tool-rpc
full = []                                   # in-process Session (today's behaviour); clank-agent and the clank tool use this
providers = ["dep:wasi-fetch", "dep:http", "dep:async-trait"] # unchanged
```

`tool` and `full` are mutually exclusive (`compile_error!` when both are set).

**`crates/clank-embed/src/tool_shim.rs`** (feature `tool`):

```rust
pub struct ToolShell;                                  // no Session at all
impl ToolShell {
    pub async fn eval(&mut self, cmd: &str) -> EvalResult { self.call(vec![], "line", cmd).await }
    pub async fn answer(&mut self, response: Option<String>) -> EvalResult {
        match response { Some(t) => self.call(vec!["answer-prompt".into()], "text", &t).await,
                         None => self.call(vec!["abort-prompt".into()], "", "").await }
    }
    async fn call(&mut self, path: Vec<String>, field: &str, value: &str) -> EvalResult {
        let unavailable = |why: String| EvalResult { stdout: String::new(), stderr: format!("clank tool unavailable: {why}\n"), exit_code: 4, pending_prompt: None, cwd: String::from("/") };
        let Some(registered) = tool_host::get_tool("clank") else { return unavailable("not bound to this agent".into()) };
        let record = ToolValue::Record(if field.is_empty() { vec![] } else { vec![(field.into(), ToolValue::Str(value.into()))] });
        let input = match AmbientToolInvoker::encode_input(&record, &registered.definition, &path) { Ok(i) => i, Err(e) => return unavailable(e) };
        match AmbientToolRpc::new("clank").invoke_and_await(path, input, None, None) {
            Ok(r) => decode_eval_result(r.result),
            Err(e) => unavailable(format!("{e:?}")),
        }
    }
}

/// `EvalResult` derives `FromSchema` (it is already an agent-method result type), so the tool's
/// structured result decodes with the SDK's own decoder; a missing or undecodable value is exit 4.
fn decode_eval_result(value: Option<TypedSchemaValue>) -> EvalResult {
    match value.map(|v| golem_rust::decode_typed_schema_value::<EvalResult>(&v)) {
        Some(Ok(r)) => r,
        other => EvalResult { stdout: String::new(), stderr: format!("clank tool returned an undecodable result: {other:?}\n"), exit_code: 4, pending_prompt: None, cwd: String::from("/") },
    }
}
```

**`crates/clank-embed/src/shell.rs`** — `EmbeddedShell` wraps either the in-process
`Session` (`full`) or `ToolShell` (`tool`) behind the same `eval`/`answer` methods and the
same constructors (`with_default_golem_providers` is a no-op under `tool` beyond the log sink).

**`crates/clank-agent/Cargo.toml`** — `clank-embed = { ..., default-features = false, features
= ["full", "providers"] }` (the agent and the tool are the shell).

**`fixtures/greeter-agent`** — `Cargo.toml` keeps the default (`tool`); `golem.yaml` binds
`tools: { clank: {} }` under `Greeter` and adds `dependencies.tools: [clank:agent/clank]` so the
integration test can also import the generated `clank_tool_guest_client::ClankClient` and assert
that its canonical record for `clank("pwd")` equals the shim's. The marker file the `clank` tool
provisions (`fixtures/clank-marker`) is a one-line file in the host repo — any content, the path
only has to exist at deploy. The greeter also enables snapshotting (`golem-custom-snapshot`) as
the reference for keeping a shell-heavy host's replay bounded (ticket 10's oplog note).

Because the sidecar evaluates tool discovery as the **owner** agent type, a shell inside Greeter
sees Greeter's own bound tools on `$PATH` — binding `echo-tool` to `Greeter` too makes
`echo-tool greet Ada` work from `agent shell 'Greeter("g1")'`, which ticket 12's corpus relies on.

**`docs/EMBEDDING.md`** — the host-facing guide: the two features, the manifest lines
(`tools.clank` with `release:` or `component:`, the marker `files:`, `agents.<Type>.tools.clank`),
the three methods `agent shell` expects, the behaviour notes from ticket 10, and the snapshotting
recommendation. README links to it under "Adding a shell to your agent".

**Tests** — `cargo check -p greeter-agent --target wasm32-wasip2` with the default features and
with `--no-default-features --features full,providers`; `golem agent shell 'Greeter("g1")'` with
the patched CLI runs `pwd`, `cd src && ls`, a pipeline and a `prompt-user` with the select menu
against the `tool` build; the existing greeter e2e (wRPC round trip, log sink) is green on both
feature sets; the greeter source changes only in its manifest and `Cargo.toml`.

**Done when:** `agent shell` works end to end against a host that never compiled the shell in.

---

### 12. Conformance `embedded-tool` backend, parity gate, trap gate

The definition of "behaviourally identical": the same `.clank` corpus green on `ClankAgent`,
`Greeter(full)` and `Greeter(tool)`; plus the hosting-safety gate for traps.

**`crates/clank-conformance/src/backend/mod.rs`** — `BackendKind` gains `EmbeddedTool`
(`"embedded-tool"`); **`backend/golem.rs`** — `GolemBackend::new(name)` takes the agent type
from `CLANK_CONFORMANCE_AGENT_TYPE` (default `ClankAgent`); the `EmbeddedTool` kind sets
`Greeter` and requires `CLANK_CONFORMANCE_EMBEDDED=1`. **`tests/embedded_tool.rs`** — the
eleven-line entry point calling `harness::main(BackendKind::EmbeddedTool)`.
`scripts/conformance-golem.sh --backend embedded-tool` deploys the greeter built with the
requested feature (`--features full|tool`) and runs the binary.

**Scenario tags** — `bg-jobs.clank` becomes `@only golem` with the reason "jobs do not outlive a
tool invocation"; the AGENT-kind `--schedule` and self-invoke scenarios get an `embedded-tool`
variant asserting the honest exit-4 message from ticket 10's behaviour notes; the `tools` tier
scenarios run on the embedded backend too (`echo-tool` is bound to `Greeter`, ticket 11); any
other scenario that differs between the three runs is fixed or tagged with a documented reason.
`durability-state.clank` must pass unchanged on `Greeter(tool)` — it is the acceptance test for
the shell-state script in the session file.

**Trap gate** — a `test-hooks`-feature-only builtin `__clank_test_panic` in clank-core; a
golem-e2e assertion runs it through `Greeter(tool)`: the host is interrupted, Golem's retry
recovers it, and the host's next `eval` succeeds. `cargo clippy -p clank-core -p clank-embed
-p clank-agent -- -W clippy::unwrap_used -W clippy::expect_used` reports zero hits in
`golem/tool/`, `session/tool.rs`, `session/persist.rs`, `clank_tool.rs`, `tool_shim.rs`.

**`.github/workflows/conformance.yml`** — the ticket 2 job gains a matrix over
`golem`/`embedded-tool(full)`/`embedded-tool(tool)`.

**Done when:** the corpus is green on all three, the trap assertion passes, and the clippy gate
is clean.

---

### 13. Optional: stream-typed `shell` method

Deferred until stock-CLI access matters. `agent shell` stays primary.

**`crates/clank-agent/src/clank_agent.rs`** (and the shim) — one more agent method:

```rust
async fn shell(&mut self, input: AgentStream<u8>) -> AgentStream<u8>
```

A line loop inside one invocation: read up to `\n`, `eval`, write `stdout`/`stderr` and a
`$ ` prompt; when `pending_prompt` is set, write the question (and choices) and treat the next
line as `answer_prompt`; EOF on input ends the invocation. Driven by
`golem agent invoke 'ClankAgent("x")' shell - --stdin-format raw --stdout-format raw`.

**Tests** — the stock 1.6 CLI runs a session with two lines and a prompt; the method is
documented as monopolizing the agent for its duration.

**Done when:** the session runs with a stock binary, or the ticket is closed as not needed.
