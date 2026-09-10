---
title: "Agent tools integration: clank as the CLI projection of Golem tools, and as a tool"
date: 2026-09-10
author: agent
---

# Agent tools integration

Design for integrating clank with Golem's agent-tools feature in both directions. Written as
intent after a brainstorm with Aditya on 2026-09-10; every decision below was made or confirmed in
that session. Facts about Golem refer to upstream `main` at `8d0fd1f3c` (2026-09-10) unless a
GOL-nnn ticket is named; facts about clank refer to `main-rc-1` at `126991f`. The implementation
plan (`dev-docs/plans/proposed/agent-tools-integration.md`) is the single source of truth for
sequencing and acceptance; this document records intent and decisions.

## Overview

One integration model, two directions, one shared core:

```
                 ┌──────────────────────────────────────────────┐
                 │ clank-core: the CLI projection of tool metadata│
                 │  wire Tool → command-tree walk → argv parser  │
                 │  → canonical typed-schema-value → ToolInvoker │
                 │  ← stdout stream / result / error-case → exit │
                 └───────────────┬──────────────┬───────────────┘
   PHASE 1 (consumer)            │              │        PHASE 2 (provider)
   bound tools appear ambiently  │              │  clank's component also exports
   under /usr/lib/tools/bin/;    │              │  a `clank` tool (FS-capable);
   ask sees them as commands;    │              │  clank-embed = `tool` shim (default)
   authz from annotations;       │              │  or `full` in-process (feature);
   grease binds at deploy time   │              │  session state lives in owner FS
```

**Phase 1 — consumer.** Every tool bound to the hosting agent becomes a shell command with full
CLI fidelity (subcommands, short/long options, negatable and count flags, repeatables, positionals
with a `--` tail, stdin/stdout, exit-coded errors, `--help` at any depth). clank implements the
projection that the tool WIT (`golem-common/wit/deps/golem-tool/common.wit:9-14`) names as a
future deliverable.

**Phase 2 — provider.** clank's own component additionally exports a `clank` tool. A third-party
agent binds it in `golem.yaml` and includes a thin `clank-embed` shim, after which `golem agent
shell` works against that agent — the "shell inside any agent" end-state — without compiling the
shell into it.

**Target matrix.**

| | native `clank` | `ClankAgent` on Golem | third-party agent + clank |
|---|---|---|---|
| invoke agent tools | exit 4 "needs a Golem host"; local runner is roadmap | yes, ambient | yes, via either embed feature |
| interactive session | local TTY | `agent shell` (primary); per-line `eval` | `agent shell` through the shim |
| policy | clank authz | clank authz; runtime middleware once GOL-39 lands | manifest binding (+ middleware later) |

## Decisions (with the alternatives rejected)

1. **Both directions, consumer-first.** Phase 1 has zero upstream dependencies beyond what is on
   `main`; phase 2 depends on a runtime shape (fresh Store, filesystem lane) that is settled but
   young. Rejected: consumer-only (abandons the "any agent" vision), provider-only (hardest part
   first, no shared core to build on).
2. **Bound tools are ambient; `grease install` is the deploy-time verb.** A binding is compiled
   per agent type at deploy (`cli/golem-cli/src/model/app_raw.rs:370` `ToolBinding`; no
   environment-level bindings since #3788), so nothing inside a running agent can create one.
   Tools the agent is bound to therefore appear on `$PATH` with no ceremony, and
   `grease install tool:<name>` runs on the native side, edits `golem.yaml`, and deploys.
   Rejected: explicit runtime `grease install` per tool (ceremony for a choice the operator already
   made); grease read-only (drops the "grease installs agent tools" direction).
3. **Native target is honest-unavailable.** Nothing outside a guest can invoke a tool: no REST
   route (`openapi/golem-worker-service.yaml`), no `golem tool invoke` (only
   `list/get/release/grant`, `cli/golem-cli/src/command.rs:1197`), MCP exports agent methods only.
   Native clank registers exit-4 stubs from the local manifest. A wasmtime-embedded local runner is
   the explicit roadmap item. Rejected for v1: local runner (large, new runtime dependency); proxy
   to a cluster-side clank (tools would run in a remote sandbox with confusing cwd semantics).
4. **clank-embed offers `full` and `tool` as cargo features, default `tool`.** `full` is today's
   in-process `Session`; `tool` forwards to the bound `clank` tool. Two code paths are accepted
   because the conformance corpus keeps them behaviourally identical (§Testing). Rejected:
   shim-only (no fallback for hosts that cannot bind tools yet); full-only (every host carries the
   shell, updates require host redeploys, no manifest-level policy).
5. **Approach A — clank is the CLI projection.** A metadata-driven argv parser over the wire
   `Tool`. Rejected: **B** generated typed clients per tool (tools would have to be known when
   clank is built, contradicting ambience; N adapters; no generic help); **C** MCP-style generic
   `tool sub --k v` calls (discards exactly the CLI shape the model exists for; degraded help;
   loses the LLM-familiarity win). C's schema-driven coercion survives only for value types a
   command line cannot spell (record-typed option values accept a JSON literal).
6. **`agent shell` remains the primary interactive client.** golemcloud/golem#3700 was auto-closed
   by the vouching bot, not reviewed; it validates any agent exposing
   `eval`/`answer_prompt`/`abort_prompt` against the reflected schema and renders pending prompts as
   a select menu. A stream-typed `shell` agent method driven by the stock CLI
   (`golem agent invoke 'X()' shell - --stdin-format raw --stdout-format raw`, the
   `/v1/agents/invoke-agent-session` WebSocket) is optional and deferred (§Phase 2, 4.5).

## Prerequisites (not design choices)

- Rebase `clank-connect-patch` (the SDK + CLI clone at `~/Desktop/clank.sh/golem-stuff/golem`)
  onto upstream `main` ≥ `8d0fd1f3c`. It is 91 commits behind and lacks the ambient tool runtime
  (`sdks/rust/golem-rust/src/agentic/ambient_tool_rpc.rs`, `agent_stream.rs`). The
  `clank-connect-patch` CLI commits stay on top for the patched `golem` binary.
- Fix the two wasip3 breakages already present in `crates/clank-embed/src/agent_invoker.rs:96`
  (`invoke-and-await` now returns `invocation-result-with-metadata`; use `.result`) and `:123`
  (`golem_rust::wasip2::clocks::wall_clock::Datetime` → `golem_rust::ScheduledTime`).
- Probe a raw tool call from `ClankAgent` against upstream's `test-components/tool-streaming`
  provider before writing any projection code.

## Phase 1 — bound tools become shell commands

### 1.1 Discovery → ambient `$PATH`

At `Session::new` (and whenever the discovery version bumps) the injected `ToolInvoker::discover()`
returns the wire `Tool` records the hosting agent is bound to (`golem:tool/host` `get-all-tools`,
which already filters to the caller's bindings). Each is projected into a `Manifest`
(`crates/clank-core/src/manifest.rs:91`):

- `name` = tool root name; `subcommands` recursively from `command-tree`;
- `input_schema` = `ParamSpec`s from body params plus inherited globals;
- `execution_scope = Subprocess`; `authorization_policy` from annotations (§3.2);
- `help_text` rendered from `doc` (summary, description, examples).

Manifests register in the dynamic registry (`crates/clank-core/src/runtime/dynreg.rs`) beside MCP
and grease manifests, cached by a discovery version alongside the existing `(mcp, grease)` cache
key. A real stub file is written to a new `/usr/lib/tools/bin/` on `$PATH` so `which`, Brush
resolution and `type` all see the command; the virtual `/bin` (`runtime/binfs.rs`) stays
static-only. Discovery is a durable host call upstream (`GolemToolGetAllTools`, replay uses the
persisted result), so calling it at session start is replay-safe.

### 1.2 The projection (`crates/clank-core/src/golem/tool/{argv,help,manifest}.rs`)

Input: a wire `Tool` and Brush's already-split argv (quotes resolved). Steps:

1. **Command walk.** From the root, consume tokens while the current node has a child whose name or
   alias matches; stop at the first non-matching token. A resolved node without a body is a usage
   error (exit 2) listing its children.
2. **Argument parse** against the body plus inherited globals: `--long=v`, `--long v`, `-s v`,
   bundled short flags; `--no-<flag>` for negatable bool flags; count flags; `repeatable` options
   per their `repetition` (`repeated`, `delimited(c)`, `either(c)`); `optional-scalar` bare presence
   → default; fixed positionals then the tail positional with `min`/`max`, honouring `separator`
   (`--`) and `verbatim`; env-var fallback for options/flags that declare one; metadata defaults
   last.
3. **Constraints**: `requires-all`, `requires-any`, `all-or-none`, `mutex-groups`, `implies`,
   `forbids` → exit 2 with a message naming the offending arguments.
4. **Coercion** against `tool.schema` type nodes: scalars, enums, lists, `k=v` maps, option, path,
   url, datetime, duration, bytes; record-typed values accept a JSON literal.
5. **Canonical input record** — one field per positional, option, flag and inherited global, typed
   by the matching node, emitted as a `typed-schema-value`.

`--help` at any depth renders from `doc` plus synthesized usage, listing declared `error-case`s,
stream MIME constraints, formatter names and annotations.

**Wire-compatibility risk and mitigation.** The canonical record's field set and order must match
what the SDK's generated clients produce (`sdks/rust/golem-rust/src/agentic/extended_tool_type.rs`
`canonical_input_record_schema`, `build_canonical_input`). Those helpers operate on the SDK's
`ExtendedToolType`, and no wire→extended conversion exists (`pub type Tool = wire::Tool;`,
`extended_tool_type.rs:26`). Preferred mitigation: contribute `TryFrom<wire::Tool> for
ExtendedToolType` upstream, after which clank calls `canonical_input_model`, `build_canonical_input`
and `render_help` verbatim. Fallback: mirror the canonical rule in clank and pin it with golden
tests generated from the SDK's `tests/tool_canonical.rs` fixtures.

### 1.3 The `ToolInvoker` seam

A target-agnostic trait in `crates/clank-core/src/golem/tool/mod.rs`, beside `AgentInvoker`
(`golem/agent.rs:81`), injected through the same provider seam (`Session::set_agent_invoker`
pattern; native `native.rs:642 inject_native_providers`, wasm
`clank-embed/src/shell.rs:73 with_default_golem_providers`):

```rust
pub struct ToolCall {
    pub tool: String,
    pub command_path: Vec<String>,
    pub input: ToolValue,           // clank-owned value tree; no golem crate in clank-core
    pub stdin: Option<Vec<u8>>,     // staged pipeline bytes
    pub expects_stdout: bool,       // body declares a stdout stream-spec
}

#[async_trait::async_trait(?Send)]
pub trait ToolInvoker {
    async fn discover(&self) -> Result<Vec<RegisteredTool>, String>;
    async fn invoke(&self, call: ToolCall, sink: &mut dyn ToolOutputSink)
        -> Result<ToolOutcome, ToolFailure>;
}
```

`ToolValue` mirrors the `schema-value-node` arms clank needs; the wasm impl in `clank-embed`
encodes it to `TypedSchemaValue`, calls `AmbientToolRpc::new(name).invoke_and_await(path, input,
stdin, stdout)`, feeds stdout chunks (`ToolInvocationStdout::next`) into the sink — live for
filesystem-incapable tools, at the terminal for capable ones (`durable_host/tool/mod.rs:2833`) —
and supplies staged stdin through `create-stdin-from-stream` with a finite stream, so no producer
pump is needed. The native impl is the honest exit-4 provider. clank-core never depends on a golem
crate; native injects nothing, as today. The full type set is in the plan's Architecture section.

### 1.4 Pipeline integration

Tool commands are Session-layer `Subprocess` stages and follow the existing `curl`/`ask` rule
("one Session-layer stage per line"): a tool may be the **head** of a pipeline (`git log --oneline
| head`, via the existing head-split in `run_command`) or the **tail** (`cat f | fmt-tool`, stdin
pre-extracted as for `ask`), not the middle. This is the Wall-C limitation clank already carries,
recorded here, not introduced here.

Output rule: if the body declares a stdout stream, only the stream is printed; otherwise the
structured `result` is rendered — raw for strings, canonical JSON otherwise. Never both, so tools
that mirror their result to stdout do not double-print. Formatter names appear in `--help` but are
not applied in v1.

Exit codes and error mapping are specified in §Error handling.

## Phase 1 periphery

### 3.1 Model exposure

Bound tools reach the model through the existing `shell` tool plus the capability section of the
system prompt (`ai/ask.rs:144 build_system_prompt_with_capabilities`): name, summary, and a
"run `<tool> --help` for usage" hint. One path: the model's `git commit -m x` goes through the same
projection, authz gate and audit as a human-typed line. Structured `mcp__`-style tool definitions
(`mcp/state.rs:231 ask_tool_definitions`) are **not** added in v1 — they would duplicate the
projection as a JSON-Schema lowering plus a decode-back step. Revisit if measurement shows the
model misusing CLI syntax (§Open questions).

### 3.2 Authorization from annotations

Each command body's `command-annotations` derives its policy: `read-only` → `Allow`;
`destructive`, `open-world`, or annotations absent (MCP's untrusted default) → `Confirm`; never
`SudoOnly` automatically. Policies are per subcommand, which `authz::resolve`
(`authz.rs:108`, subcommand-aware at `:123`) already handles. The model gate
(`session/ask.rs:1038-1075`) sees `Subprocess` scope → allowed, then `authz::decide`. No new
override configuration in v1. When GOL-39 lands, manifest middleware is the operator's
bypass-resistant layer beneath this; clank's authz remains the interactive and model-facing layer.

### 3.3 grease

Inside a host: `grease list` shows bound tools as kind `tool` with component and version;
`grease info <tool>` shows annotations and the command tree; `grease install tool:<x>` inside a
host fails honestly ("bindings are compile-time — run from native clank"). On native:
`grease install tool:<name> [--release <account>/<name>/<version> | --component <local-id>]`
adds the top-level `tools:` declaration and the `agents.ClankAgent.tools.<name>: {}` binding to the
app manifest; `grease remove tool:<name>` reverses it; both print the deploy steps or run them
with `--deploy` — for a `--release` source that includes the environment grant
(`golem tool grant create`), since release-sourced bindings resolve against grants at deploy.
Integrity for tools is Golem's release registry (digest and version pin) —
grease's sha256/ed25519/transparency layer does not apply to this kind, and `grease info` says so.

### 3.4 help / type / which / man

`<tool> [sub…] --help` and `man <tool>` render projection help through the pre-authz dynamic-help
hook that `pkg_help_for` uses (`session/mod.rs:857`); `type <tool>` → "agent tool (bound;
component …)"; `which` finds the real stub.

### 3.5 Native honesty

Native clank reads the app manifest (`./golem.yaml`, or `CLANK_APP_MANIFEST`) for the
`ClankAgent` tool bindings and registers exit-4 stubs — "`git` is a Golem agent tool; run inside a
Golem host" — so `grease list` shows the same set on both targets and an unknown command is
distinguishable from an unbound tool. Metadata (`--help`) is host-only until the local runner.

### 3.6 Audit

Every invocation writes a `tool-invoke` record to `/var/log/ops.log` (tool, command path, exit,
duration) through the same path as `agent-invoke` (`session/agent.rs:185`); option values pass
through the existing flag-argument redaction. Golem's oplog records the call independently
(`GolemToolRpcInvokeAndAwait`, `GolemEntityInvoke`).

## Phase 2 — clank as a tool

### 4.1 The `clank` tool

A `#[tool_definition] trait Clank` in the clank-agent component beside `ClankAgent` — one
deployable, since the `golem-agentic` world exports both `golem:agent/guest` and `golem:tool/guest`
under `export_golem_agentic` (`sdks/rust/golem-rust/wit/golem-rust.wit:52-56`). Root body is the
implicit-body method `clank`, so `clank "ls /"` evaluates a line; subcommands `answer-prompt
<text>` and `abort-prompt` complete the surface. The result is today's `EvalResult` wire type
(`stdout`, `stderr`, `exit_code`, `pending_prompt`, `cwd`); no stdout stream in v1, because a
filesystem-capable tool's stream is published only at the terminal anyway, and a plain record is
what a host's raw `tool-rpc` call (or a generated client) wants. The pending-prompt two-call model
carries over unchanged.

### 4.2 Session state

A tool body runs in a fresh Store per invocation (gol-33 §Fresh instances; `worker/instance.rs:598`),
so the session lives in the owner's filesystem: `<owner root>/.clank/session.json` holding cwd,
`$?`, the Brush shell state as a re-sourceable script (`declare -p`, `declare -f`, `alias -p`,
`set +o`, `shopt -p` — so variables, functions, aliases and options survive exactly as in `full`
mode), the pending prompt, the cap-bounded transcript and the last tool discovery. It is loaded
at invocation start and rewritten whole-file at the end — the replay-safe pattern (append is
replay-unsafe; whole-file write is idempotent); grease and MCP state already persist this way.
Background jobs cannot outlive an invocation in tool mode, host APIs that outlive an invocation
(`--schedule`, promise waits) are refused honestly inside the tool, and a shell inside a host sees
the host's own bound tools.

**Per-line latency is the key risk**: `Session::new` plus state load on every line. Mitigations in
order: the `release` component preset, lazy subsystem initialisation, and upstream's planned
warm-instance cache (gol-33 §Future extensions). A measurement gate with an explicit threshold
(§Open questions) precedes making `tool` the practical default.

### 4.3 Filesystem grant

The tool must be filesystem-capable to be a shell at all. The manifest binding cannot express that
today: `ToolBindingInput` (`golem-common/src/base_model/tool.rs:134`) carries only version,
parameters, account and secret scopes; `ToolFilesystemAccess` (`:191`) exists only on the compiled
binding. Interim: declare one provisioned file on the tool —

```yaml
tools:
  clank:
    files:
      - sourcePath: fixtures/clank-marker
        targetPath: /.clank/MARKER
```

— which implies the grant per gol-33 §Filesystem capability classification ("provisioned files
imply the grant"). The plan includes a live probe of exactly this. Upstream ask: the GOL-29
manifest key, after which the marker file is removed.

### 4.4 The embed shim (`clank-embed`, feature `tool`, default)

`EmbeddedShell` keeps its API (`eval`, `answer(Option<String>)` → `EvalResult`; a host's
`eval`/`answer_prompt`/`abort_prompt` methods call these). Under `tool` it forwards to the bound
`clank` tool through `AmbientToolRpc` with the same encoder the consumer path uses — the canonical
record is a single string field, and the projection goldens guard the encoding. The
build-generated `ClankClient` is deliberately not used by the shim: `golem build` emits that crate
inside the *calling component's* directory
(`<component>/golem-temp/bridge-sdk/rust/internal/clank-tool-guest-client`), which a shared
library cannot depend on; the greeter integration test imports it only to cross-check the wire
shape. The host adds `tools: {clank: {}}` to its manifest. Under `full` it stays today's
in-process `Session`.

Consequence: `golem agent shell 'Host("x")'` works for **any** agent that includes the shim, with
the patched CLI that exists today. `fixtures/greeter-agent` becomes the reference host. Honest
notes: a guest trap inside the clank tool interrupts the *host* agent (`durable_host/tool/mod.rs:
2539-2593` fences siblings and interrupts the owner) exactly as a panic in `full` would, so clank's
panic hygiene becomes a hosting-safety property; a filesystem-capable tool body holds the owner
filesystem lane for the duration of one line, which is the same serialization a `full` embed has.

### 4.5 Optional stock-CLI route (deferred)

`shell(input: AgentStream<u8>) -> AgentStream<u8>` on `ClankAgent` and the shim: a line loop inside
one invocation, prompts rendered inline, driven by the stock CLI's stdin/stdout binding. It
monopolizes the agent for the session (per-line `eval` does not). Its only purpose is access for
people without the patched CLI; it ships only if that matters.

### 4.6 `agent shell` delivery

Keep `clank-connect-patch` rebased on upstream `main`; re-submit #3700 once vouched (a sponsor from
the Golem team is the realistic path). Until then RC_TESTING.md's patched binary is the documented
route.

## Data flow

**Trace A — consumer, inside `ClankAgent`:** `git commit -m fix --amend | tee log`

1. Brush tokenizes; the Session's head-split recognises `git` from the dynamic registry (scope
   `Subprocess`) and treats `| tee log` as the Brush tail.
2. Authz resolves the subcommand policy: `commit` is `destructive` → `Confirm` → pending prompt on
   the interactive path; `authz::decide` with the sudo grant on the model path.
3. The projection walks `git → commit`, parses `-m fix` and `--amend`, fills inherited globals
   from defaults and env (`git-dir=.git`, `paginate=true`), checks `reset-author ⇒ amend`, coerces,
   and emits the canonical record.
4. The embed `ToolInvoker` calls `AmbientToolRpc("git").invoke_and_await(["commit"], input, None,
   stdout)`. Host: durable `Start`; filesystem-capable, so the primary yields the owner lane; a
   fresh Store runs the tool against the shell's own working directory (same owner root); stdout is
   published at the terminal with the result.
5. clank feeds the stdout bytes into the in-memory pipe for `tee log`; exit 0; `tool-invoke`
   audit record; the transcript records line and output for `ask` context.

**Trace B — provider, via the shim:** `golem agent shell 'Greeter("g1")'`, then `cd src && ls`

1. The CLI invokes `Greeter.eval("cd src && ls")`, the surface it already validates on connect.
2. Greeter's `EmbeddedShell` (feature `tool`) calls the bound `clank` tool over `tool-rpc`
   (`AmbientToolRpc("clank").invoke_and_await([], {line})`). Host: `Start`; filesystem-capable via
   the provisioned marker; lane; fresh Store of clank's component.
3. Inside the tool: load `/.clank/session.json`, rebuild the Session, evaluate against Greeter's
   filesystem, rewrite `session.json`, return the `EvalResult` record. The next line's `pwd` sees
   `src` because the file carried the cwd.
4. A `prompt-user` in step 3 returns as `pending_prompt`; `agent shell` renders the select menu and
   delivers `answer_prompt`, which the shim maps to `clank answer-prompt <text>`.

## Error handling

| Source | Exit | Behaviour |
|---|---|---|
| clank-side usage / constraint / coercion failure | 2 | usage line on stderr; nothing invoked |
| `custom-error` matching a declared `error-case` | its `exit-code` (usage 2 / runtime 1 if unset) | `tool sub: <error-name>: <payload>` on stderr |
| host `invalid-input` / `constraint-violation` / `invalid-command-path` | 2 | projection bug or metadata drift; the message says so |
| rpc `denied` | 3 | permission wording; no retry |
| rpc `not-found` | 127 | "no longer bound" — deploy drift since discovery; re-discover on the next line |
| rpc `protocol-error` / `remote-internal-error` / `invalid-result` | 1 | verbatim |
| rpc `cancelled` | 130 | |
| `resource-exhausted` (16 MiB per-direction attachment cap, `max_tool_attachment_bytes`) | 1 | names the cap; suggests filtering |
| guest trap inside the tool | — | Golem interrupts and retries the owner; clank cannot catch it |

Provider side: a missing or corrupt `session.json` starts a fresh session with one stderr warning
(it is a cache of replay-reconstructed state, not truth); eval-while-pending keeps today's rule.
Background: `tool &` runs through the existing job machinery and `kill` **detaches** rather than
cancels — the synchronous host call completes and is oplogged; true cancellation needs
`async-invoke-and-await` and is roadmap.

## Testing

- **Unit, clank-core (native):** table-driven projection tests over hand-built wire `Tool` fixtures
  covering every construct in §1.2; golden canonical-record tests generated from the SDK's
  `tests/tool_canonical.rs` fixtures (Grep and Git) pinning field set and order; the error-mapping
  table; annotations → policy; native exit-4 stubs from a sample `golem.yaml`.
- **Unit, clank-embed:** `ToolValue → TypedSchemaValue` round-trips through golem-schema's decoder;
  both wasm crates compile.
- **Conformance corpus:** wire the currently unwired `@requires` mechanism
  (`crates/clank-conformance/src/harness.rs:113`) for a `tools` tier. A new `fixtures/echo-tool`
  (a small Rust tool: positionals, options, stdin, stdout, one `error-case`, plus a
  filesystem-capable variant that writes into the owner root) is deployed by the harness; scenarios
  cover invoke, pipe, exit codes and help, `@only golem`. Native gets the honest-exit-4 scenario.
- **Feature-parity gate:** a third conformance backend, `embedded-tool`, drives `greeter-agent`
  (shim) through the same `.clank` corpus via `golem agent invoke Greeter eval`. Green on `full`,
  `tool` and `ClankAgent` is the definition of "behaviourally identical".
- **Live probes (each go/no-go in the plan):** filesystem grant via the provisioned marker;
  per-line latency in tool mode under the release preset against the agreed threshold; session
  persistence (`cd` then `pwd` across invocations); pending-prompt round trip through the tool;
  crash-and-replay (`simulate-crash` mid-session → cwd and `session.json` reconstructed); trap
  blast radius (a deliberately panicking line must not wedge the host permanently).
- **e2e:** `scripts/golem-e2e.sh --with-tools` deploys clank, echo-tool and greeter and asserts the
  above. CI: the existing golem tier pins golem 1.5.1 and cannot run `main-rc-1`; the plan adds a
  build-from-clone job or keeps the tier manual until the SDK is released.

## Upstream asks

1. `TryFrom<wire::Tool> for ExtendedToolType` (or equivalent) so `render_help` and
   `canonical_input_model` work on discovered tools.
2. GOL-29: a manifest key for `filesystemAccess` on tool declarations/bindings.
3. Vouching for #3700 (`agent shell`) so it can be re-submitted.
4. Dependency only: GOL-39 middleware runtime (chain resolution and dispatch are absent on the
   host today; `underlying-tool` is unreachable).
5. Advocate for the warm-instance cache with the latency numbers from §Testing.

No external tool-invoke API is requested: the shim covers the "any agent" case.

## Risks, ranked

1. Canonical-record drift between clank's projection and SDK clients — goldens plus ask 1.
2. Per-line latency in tool mode — measurement gate before `tool` is the practical default.
3. Trap blast radius on hosts — panic hygiene becomes a release gate.
4. stdout only at the terminal for filesystem-capable tools — documented; per-line UX.
5. SDK rebase churn — 91 commits behind; the wasip3 breakage already exists.
6. `main-rc-1` has no CI golem tier — plan item.
7. Host oplog growth in tool mode — every shell line is a tool invocation re-executed on the
   host's replay; mitigated by host snapshotting, demonstrated by the reference host.

## Phasing

The plan's thirteen tickets map onto three phases:

- **P0 — prerequisites (tickets 1–2).** Rebase the SDK clone, fix `agent_invoker.rs`, probe a raw
  tool call; build the test infrastructure (fixture tool, conformance `tools` tier, e2e flag, CI
  job) everything else is accepted against.
- **P1 — consumer (tickets 3–9).** The seam and mirror model; discovery + `$PATH` + help; the
  projection; Session dispatch with pipelines, errors and audit; `ask` + authz; grease + docs;
  upstream contributions.
- **P2 — provider (tickets 10–13).** The `clank` tool with session persistence and the filesystem
  probe; the `tool`/`full` embed features with greeter as reference host; the `embedded-tool`
  conformance backend and the trap gate; the optional stream-typed `shell` route.
- **P3 — roadmap (not scheduled).** Local wasmtime runner; structured `ask` tool definitions; a
  `clank-guard` universal middleware once GOL-39 lands; `&`/cancel via `async-invoke-and-await`;
  secrets-typed arguments.

Branch mapping (per the one-issue/one-design/one-plan rule): P0 + P1 ship as the
`agent-tools-integration` branch with this issue, this design and its plan. P2 opens its own slug
(`clank-as-tool`) whose issue and design derive from §Phase 2 here; this document moves to
`designs/approved/` as-built when the P1 branch merges, and §Phase 2 stays the recorded intent the
P2 design starts from.

## Implementation plan

`dev-docs/plans/proposed/agent-tools-integration.md` — thirteen tickets in the org's
implementation-plan format (spec, architecture, then per-ticket file-level changes with code,
tests and a "done when"). It is the single source of truth for sequencing and acceptance.

## Out of scope

Component composition; MCP import/export of tools through Golem; TTY host imports; authoring tool
middleware in clank; changes to the wRPC/agent package kind.

## Open questions

- The per-line latency threshold for tool mode (a number, in ms, at the release preset) that gates
  making `tool` the practical default.
- Whether structured `ask` tool definitions are needed after measuring model behaviour with the
  `shell`-only path.
- Whether `grease install tool:` should run `golem deploy` by default or only with `--deploy`.
- Whether the optional stream-typed `shell` route ships at all.
- Confirmation from upstream of the canonical record's field order (inherited globals before body
  fields, declaration order within each) before the goldens are frozen.
