# clank.sh

The Unix shell is the most durable human-computer interface ever built. Decades of tooling, documentation, and institutional knowledge exist around it. AI systems, by contrast, are stateless, fragile, and hard to compose with existing workflows.

clank.sh is an AI-native shell that gives AI models a first-class, auditable, sandboxed operating environment — modeled on Linux, so every LLM is already an expert operator on day one. Prompts, MCP tools, and Golem agents are all installed as ordinary CLI commands: tab-completable, pipeable, scriptable, and governed by a single authorization model. The AI cannot reach outside what you have explicitly installed. Every capability is declared, every action is logged, and every tool is composable with the rest of the Unix toolkit.

When running on Golem, clank.sh instances become fully durable agents. The transcript, filesystem, and all state survive infrastructure failures transparently. Every tool invocation has exactly-once semantics. Idle instances cost nothing. Running natively, the same shell gives developers and teams a scriptable, extensible AI workflow environment that anyone can extend — writing a prompt requires nothing more than a Markdown file.

- **The AI reads exactly what you see.** The shell's session history *is* the model's context window — no setup, no synchronization, no curation step. Run a command, ask about it. It just works.
- **Every capability is a CLI command.** Prompts, MCP tools, and Golem agents all install as tab-completable executables on `$PATH`. LLMs are expert operators on day one — clank.sh is modeled on Linux, where every model already has billions of tokens of training data.
- **The sandbox is the security model.** clank.sh runs in a WASM component. The AI can only reach what you've explicitly installed. Every capability is declared, every action is logged, and authorization is per-command — allow, confirm, or sudo-only.
- **Anyone can extend it.** If you can write a markdown file, you can ship a new AI capability. Prompts, tools, and agents are distributed as signed packages through `grease`. One install command and they're part of the AI's tool surface.
- **On Golem, your shells are indestructible.** Each clank.sh instance is a durable agent — infrastructure failures are transparent, tool calls have exactly-once semantics, idle instances cost nothing, and any shell can be rewound to a previous state. Spin up thousands; pay only for what runs.

---

## How It Works

The shell's transcript is both the human's session history and the AI's context window. Every command typed, every output rendered to the terminal, every AI response is appended to a sliding window. When you invoke `ask`, the model receives that window — no curation step, no context-population phase, no synchronization problem. The window compacts automatically at the leading edge when it approaches its safety cap: the oldest portion is summarized and replaced with a visible summary block, keeping the boundary between summarized and live history explicit. Inside Golem, the full uncompacted record is preserved in the component's oplog. Content piped directly into `ask` via stdin arrives as supplementary input alongside the transcript, on a separate channel — it was never rendered to the terminal and is not part of the shared window.

Every capability in clank.sh is a CLI command. Prompts install as executables. MCP tools install as subcommands under their server name. Golem agents install as executables with their constructor parameters as flags and their methods as subcommands. Shell scripts are executables. All of them live on `$PATH`, all have `--help`, all have a command manifest that drives tab completion, authorization policy, and provider tool packaging, and all compose via pipes. The AI sees the same command surface the human does and operates it the same way. LLMs are immediately capable operators because this interface is modeled on Linux — an environment with billions of tokens of training data behind it.

clank.sh is a single WebAssembly component, not a traditional Unix shell with real OS processes. Everything that looks like a process is a synthetic abstraction inside that component: builtins, scripts, prompts, and Golem agent invocations are all distinct implementations of the same internal process trait. There is no fork, no exec, no Unix signal kernel. PIDs are synthetic handles on internal async work and remote invocations. The consequence is a true sandbox: the AI can only do what is installed, and cannot reach the underlying OS. The execution environment is WASM, but the interface is bash-compatible — the constraints are real, the familiarity is real, and neither is hidden from the user.

Running inside Golem, a clank.sh instance is a durable agent. Its entire state — transcript, filesystem, in-flight processes — is durable by virtue of the Golem runtime. Infrastructure failures are transparent. Exactly-once execution semantics apply to every tool call. Instances cost nothing at rest. The same shell binary running natively without a Golem cluster gives you everything except durability: all commands work, all tools work, all composition works. Features that require Golem identify themselves and fail with informative errors. The upgrade from native to Golem requires no application-level changes.

---

## Non-Goals

- Not a POSIX process model. No fork/exec, no real OS processes, no Unix signals. The shell scripting language and builtins are bash-compatible; the execution environment is not a Unix process kernel.
- Not a real process kernel. PIDs are synthetic handles on internal async work and remote invocations.
- Not a transparent Unix signal emulator. `kill` terminates or cancels; signal numbers are not mapped.
- Not a permission system emulator. No real `chmod`, `chown`, or rwx bits.
- Not a local AI runtime. All model inference is via HTTP API.

---

## Philosophy

**LLM-legibility as a first-class design constraint.** Every AI agent trained on Linux documentation should be able to operate clank.sh with minimal surprise. Deviation from Unix convention has a real, measurable cost in model capability — LLMs do not generalize well to invented interfaces, but they have trained on billions of tokens of shell usage. Brush's bash-compatible foundation — validated against ~1400 integration test cases using bash itself as an oracle — is the implementation basis for this guarantee at the scripting layer. Directory structure, command names, flags, exit codes, `/proc/`, `ps`, pipes — all behave as an LLM expects.

**Existing idioms over invented ones.** A new AI-native command syntax, a new context protocol, a new tool interface format — all were available design options. None were chosen. When an existing convention fits, we use it. When it doesn't fit, we deviate minimally and honestly, documenting the deviation. The bar for inventing something new is that the existing idiom would actively mislead — not merely that a new design would be cleaner.

**Everything is a command.** Prompts, MCP tools, Golem agents, and shell scripts are all installed on `$PATH` as CLI executables with manifests, `--help`, and tab completion. This unification is not cosmetic — it is why the authorization model, LLM tool awareness, the package system, and composition all work uniformly across the entire capability surface. Adding a new capability to clank.sh means writing a command. Anyone who can write a markdown file can do that.

**Honest constraints over false surfaces.** clank.sh runs in an environment with real constraints: no true OS processes, limited terminal support in WASM, no local model inference, no Unix permission system. None of these are hidden behind facades. Where something is unavailable, the shell says so, explains why, and where relevant explains what running inside Golem would change. Errors are the honest face of constraints, not implementation failures to be papered over.

**The transcript is the context.** The shell owns its transcript as a first-class value. The terminal emulator renders it; it does not own it. `context clear` is an operation on a shell-owned value, not a terminal UI side-effect. The AI reads from the same record the human has been looking at — no curation, no synchronization, no gap within the window. This is the central architectural choice that makes the entire AI integration story coherent.

**Golem as superpower, not requirement.** The same shell binary degrades gracefully to zero and upgrades to full durability with zero application-level changes. Outside Golem, you have a capable, composable, sandboxed AI shell. Inside Golem, every piece of state is durable, every tool call has exactly-once semantics, and the entire instance can be rewound, forked, or left idle at no cost. The upgrade is a deployment choice, not a rewrite.

---

## Glossary

### Shell Primitives

**Process** — A synthetic unit of execution inside the clank shell, tracked by a PID. May be a builtin command, script, prompt, or Golem agent invocation. Not an OS process.

**Job** — A process running in the background, managed via `&`, `jobs`, `fg`, `bg`.

**Transcript** — The shell's sliding-window record of everything rendered to the terminal in the current session: every command typed, every output produced, every AI response. Owned by the shell as a first-class value; used as the AI's context on each `ask` invocation.

### AI Concepts

**`ask`** — The subprocess that invokes the configured AI model with the current sliding-window transcript as context, plus any content piped to its stdin. The primary human-AI interface.

**Prompt** — A `.md` file with optional YAML frontmatter declaring parameters, intended to be passed to a model via `ask`. A prompt is a logical package type with two runtime forms: non-parameterized prompts are installed as shebang executables (`#!/usr/bin/env ask`); parameterized prompts are installed by `grease` as generated shell scripts that handle argument parsing and invoke `ask`. May be standalone or sourced from an MCP server; indistinguishable after installation.

**Skill** — A package installed to `/usr/share/skills/<name>/`. Not itself a top-level command, but may contain reference documents (context the AI reads to understand a domain or capability) and shell scripts (installed to `/usr/share/skills/<name>/bin/`). The reference documents are available to AI models as additional capability context; the scripts are executable by human or AI like any other command.

**Tool** — Any `subprocess`-scoped shell-resolvable command or installed skill that a model provider exposes to the AI for use during `ask`.

**Provider** — A model provider implementation. Receives the shell's `subprocess`-scoped command surface and skills, and packages them for a specific model's API format.

**Model** — A specific AI model instance (e.g. `anthropic/claude-sonnet-4-6`). Accessed via HTTP API through a provider.

### Golem Concepts

**Agent** — A Golem component — durable or ephemeral — running in the Golem cluster. Addressed by type, constructor parameters, and optional phantom UUID. Never local to the shell instance.

**Agent identity** — The combination of agent type, constructor parameter values, and optional phantom UUID that uniquely addresses an agent in the Golem cluster.

**Phantom agent** — An agent that coexists with other agents sharing the same type and constructor parameters, distinguished by a UUID. Phantom agents are still durable. The canonical agent for given constructor parameters is the one without a phantom UUID.

**Ephemeral agent** — An agent type whose state does not survive between invocations. Each call runs on a fresh instance. The installed executable works identically for ephemeral and durable types; the difference is a property of the agent type, not the invocation.

**Invocation handle** — The PID assigned to a specific method call on a remote agent. It represents the invocation, not the agent itself. There is no handle on an agent — only on invocations of it.

### Infrastructure

**Resource** — A URI-addressed MCP resource, mounted under `/mnt/mcp/<server>/` as a file or virtual file. Resource templates are executables.

**Command manifest** — The shell-owned metadata object for every resolvable command. Hierarchical (supports subcommand trees). Drives tab completion, `type`, `which`, `man`, provider tool packaging, and authorization policy.

**`grease`** — The shell's package manager. Installs prompts, MCP server artifacts, Golem agent types, shell scripts, and skills via signed, content-addressed registry packages. The unit of capability extension.

**MCP session** — A stateful connection to an MCP server, identified by session ID. Required when the server advertises notifications or subscriptions; an optimization for stateless servers. Managed via `mcp session`.

---

## Architecture

```
+--------------------------------------------------------------------------+
| clank.sh  (single wasm32-wasip2 component)                               |
|                                                                          |
|           [ transcript — sliding window of all terminal I/O ]            |
|                                                                          |
|   +------------------+    +------------------+    +------------------+   |
|   |   parent-shell   |    |  shell-internal  |    |    subprocess    |   |
|   |  ──────────────  |    |  ──────────────  |    |  ──────────────  |   |
|   |  cd  exec  exit  |    |  alias  context  |    |  ask  ls  grep   |   |
|   |  export  source  |    |  history  jobs   |    |  curl  jq  find  |   |
|   |      unset       |    |   prompt-user    |    | scripts  agents  |   |
|   |  mutates shell   |    |   shell tables   |    |     isolated     |   |
|   +------------------+    +------------------+    +------------------+   |
|                                                                          |
|                                     |                                    |
|                                     v                                    |
|               +------------------------------------------+               |
|               |                   ask                    |               |
|               |    transcript window  +  piped stdin     |               |
|               |        /proc/clank/system-prompt         |               |
|               +------------------------------------------+               |
|                                     |                                    |
|                                     v                                    |
|               +------------------------------------------+               |
|               |        model provider  (HTTP API)        |               |
|               | tool surface: subprocess $PATH + skills  |               |
|               +------------------------------------------+               |
|                                                                          |
|             |                       |                       |            |
|             v                       v                       v            |
|   +------------------+    +------------------+    +------------------+   |
|   |    /mnt/mcp/     |    |  Golem cluster   |    |      grease      |   |
|   |  MCP resources   |    |  durable agents  |    | package registry |   |
|   |    virtual FS    |    |   exactly-once   |    |   signed / c-a   |   |
|   +------------------+    +------------------+    +------------------+   |
|                                                                          |
+--------------------------------------------------------------------------+
```

### Single component, internal process abstraction

clank.sh is a single Golem component. Everything that appears to the user as a "process" is an abstraction internal to that component, modeled by an async Rust trait. Different process types — builtins, scripts, prompts, Golem agent invocations — are distinct implementations of that trait, not separate WASM components.

Multiple shell processes can make progress concurrently within the single clank instance via Golem's Wasmtime runtime, which has component-model async concurrency enabled. This concurrency is invisible to the user — it is what makes job control, background processes, and in-flight agent invocations work simultaneously without any threading model surfacing at the shell level.

### Compile targets

The shell targets both `wasm32-wasip2` and native Rust. The Rust standard library covers most of both targets without abstraction (filesystem, env vars, etc.). Seams appear where crate support diverges — primarily HTTP clients (`reqwest` on native, `wasi-fetch` over WASI-HTTP on wasm-wasi). Conditional compilation handles these seams, backed by a small trait with two implementations where needed.

A second compile seam arises from Brush's use of the `nix` crate for Unix process operations. Since clank replaces the entire process execution layer, `nix` usage is excluded at that boundary via conditional compilation. No `nix` code surfaces outside the process trait implementations being replaced.

### Golem adapter

A narrow trait covers Golem-specific operations: rollback, fork, oplog access, agent introspection, agent invocation. On native, this trait either delegates to the Golem HTTP API (when a cluster is configured) or returns clean errors, meaning all Golem-dependent features degrade to informative failures rather than undefined behavior. Inside Golem, it calls host functions directly. All Golem-specific features are surfaced under the `golem` command, so failures are predictable and localized.

### Golem cluster configuration

Golem cluster config is external to the shell — a concern only for the native binary, living outside the shell's filesystem.

### Scripting language

clank.sh is built on Brush (`brush-core`), an MIT-licensed, POSIX- and bash-compatible shell interpreter implemented in Rust, designed explicitly for embedding. Brush is decomposed into independently usable crates: `brush-parser` (AST and parser), `brush-core` (embeddable interpreter with a public API for registering custom builtins), `brush-builtins` (default builtin set, registered optionally), and `brush-interactive` (interactive readline layer). clank.sh adopts `brush-parser` and `brush-core` directly; it registers its own builtins via `brush-core`'s extension API, selectively adopting or overriding the defaults from `brush-builtins`; and it replaces `brush-interactive` with its own transcript-aware interactive layer. What is replaced entirely is the Unix process spawning and runtime model, substituted by the internal async process trait.

Brush's bash compatibility is broad but not total. Known gaps inherited from upstream include: `coproc`, `select`, `ERR` traps, and some `set`/`shopt` flag behavior. One further gap is wasm-specific: **process substitution** (`<(...)`, `>(...)`) is unsupported on the agent, since it requires a concurrent producer/consumer over an OS pipe that wasip2 lacks. Scripts relying on any of these constructs will need adaptation.

---

## Concurrency Model

Three distinct layers:

**1. In-shell synthetic processes.** Multiple processes run concurrently inside the single clank component via its internal async runtime. Job control (`&`, `jobs`, `fg`, `bg`) operates over these. This is what `ps` shows. This is what PIDs refer to.

**2. Remote Golem agents.** When the shell invokes a Golem agent method, the agent runs outside the clank instance, in the Golem cluster. The shell holds an invocation handle (PID) on the call. The agent is durable and continues to exist between invocations. There are no local agents — clank.sh itself is the only Golem instance running locally.

**3. Future external process plugins via wRPC.** Roadmap only. Will slot in as another implementation of the internal process trait.

---

## Process Model

### Command manifest

Every shell-resolvable command has a command manifest. This is the single artifact that drives tab completion, `type`, `which`, `man`, provider tool packaging, and authorization policy. The manifest is hierarchical — commands with subcommands carry nested manifests for each subcommand, enabling per-subcommand completion, flag schemas, and authorization classification.

Top-level manifest fields:

- `name` — kebab-case command name
- `synopsis` — one-line description
- `execution-scope` — one of three values (see Execution scope)
- `subcommands` — nested manifests, recursively structured
- `input-schema` — typed parameter definitions (names, types, required/optional, defaults)
- `output-schema` — optional; typed description of structured output
- `authorization-policy` — `allow`, `confirm`, or `sudo-only` (see Authorization)
- `redaction-rules` — parameters that must not appear in `ps`, logs, history, transcript, completion caches, or provider manifests
- `help-text` — full help content

For builtins, the manifest is defined in Rust. For prompts, derived from YAML frontmatter. For MCP tools, derived from `inputSchema`. For Golem agent executables, derived from reflected metadata. A package that cannot provide a manifest is rejected at install time.

### Internal process table

The shell maintains a process table. Each entry has: PID, PPID (owner PID), type tag, startup arguments, status, and start time.

Process types:

- Special builtins — `execution-scope: parent-shell`
- Ordinary builtins — `execution-scope: shell-internal`
- Core commands — `execution-scope: subprocess`
- Shell scripts — `execution-scope: subprocess`
- Prompts — `execution-scope: subprocess`
- Golem agent method invocations — `execution-scope: subprocess`

### Execution scope

Every command has an `execution-scope` in its manifest:

| Scope | Meaning | Examples |
|---|---|---|
| `parent-shell` | Runs in parent shell context; mutates shell state; cannot be overridden | `cd`, `exec`, `exit`, `export`, `source`, `unset` |
| `shell-internal` | Implemented in the shell; operates on shell-internal tables (job table, alias table, transcript, etc.); cannot run as a subprocess | `alias`, `context`, `fg`, `bg`, `history`, `jobs`, `prompt-user`, `read`, `type`, `wait`, `which` |
| `subprocess` | Runs as a subprocess; no access to parent shell state | `ls`, `grep`, `jq`, `ask`, installed scripts, prompts, agent executables |

`parent-shell` commands are POSIX-defined special builtins. `shell-internal` commands are shell-implemented builtins that operate on internal tables. Both categories are distinct from ordinary subprocess commands.

### PID lifetime and reuse

PIDs are monotonically increasing within a shell session and are never reused. They are not valid across forks of the shell instance. Durable references to remote agents use agent identity (type + constructor parameters + optional phantom UUID + revision), not PIDs. PIDs for completed invocations are lazily reaped — they remain visible until accessed or explicitly waited on, then transition to `Z` and are collected.

### Process states

| State | Meaning |
|---|---|
| `R` | Running / active |
| `S` | Sleeping / waiting on remote work |
| `T` | Suspended |
| `Z` | Completed, not yet reaped |
| `P` | Paused — awaiting user authorization or `prompt-user` input |

The `P` state is first-class and visible in `ps`, `jobs`, and `/proc/<pid>/status`.

### `ps` and `/proc/`

`ps aux` / `ps -ef` produce standard column output including PPID. `%CPU` and `%MEM` show `-` — not available in WASM.

`/proc/` is a virtual read-only namespace. Not file-backed. `type` is the authoritative resolver for all commands. `which` finds file-backed commands only.

`/proc/<pid>/` provides `cmdline`, `status`, and `environ` per process. For Golem agent invocations, `/proc/<pid>/status` additionally exposes:

- `agent-type` — the agent type name
- `agent-params` — constructor parameter values
- `agent-revision` — targeted component revision
- `phantom-uuid` — phantom UUID, if present
- `idempotency-key` — internal invocation key, for correlation with Golem cluster logs and audit events

Constructor parameters must never be secret-bearing — they are permanently visible in `cmdline`, logs, and provider manifests. Secrets belong in Golem's secrets API.

`/proc/clank/system-prompt` is a virtual read-only file containing the current system prompt as it would be sent to the model on the next `ask` invocation. It is computed on read from the current set of installed tools, skills, and shell configuration — it changes as packages are installed or removed. It is `cat`-able, `grep`-able, and composes with everything else. Any AI tool that constructs a system prompt from the shell environment should reflect its output here; this path is not owned by `ask` specifically.

### Job control

`&`, `jobs`, `fg`, `bg`, and `wait` provide **synthetic job control over clank processes**. Not supported in v1:

- `Ctrl-Z` (SIGTSTP) — requires real terminal signal handling; native-only until Golem adds TTY extensions
- Terminal process-group behavior
- Full-screen interactive tooling within a backgrounded job

### `kill`

`kill <pid>` semantics depend on process type.

For Golem agent invocations: the shell maps the PID to its associated idempotency key and uses it to cancel the invocation via Golem's pending-invocation cancellation API.

- **Queued or scheduled invocation** → cancelled successfully
- **In-progress invocation** → fails with a precise error; in-progress invocations cannot be cancelled via `kill`
- **Completed invocation** → fails with a precise error

There is no handle on the remote agent itself — only on invocations of it. Agent-level interrupt and resume are distinct operations exposed under `golem agent interrupt` and `golem agent resume`; they are not `kill`.

Unix signal numbers are not mapped.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | General error |
| `2` | Invalid usage / bad arguments |
| `3` | Timeout — model call, agent invocation, or MCP tool call exceeded time limit |
| `4` | Remote call failed — HTTP error or connection failure from model provider or MCP server |
| `5` | Authorization failure — approval denied, insufficient privilege, or `sudo-only` command invoked without authorization |
| `6` | Malformed JSON (when `--json` output expected); raw model response emitted to stderr |
| `7` | Golem not available |
| `126` | Command not executable |
| `127` | Command not found |
| `130` | Interrupted (Ctrl-C) — includes `prompt-user` Ctrl-C abort |

Every process type returns a meaningful exit code. `&&`, `||`, `;` chaining works correctly across all process types. `$?` is standard.

### `exec`

`exec` retains its standard shell meaning: replace the current process with a new one. Not overloaded for agent interaction.

---

## Full command reference

Everything below the pitch, the philosophy, the glossary, and this architecture summary — every
command, every flag, every subsystem (`ask`, `context`, `model`, `prompt-user`, `mcp`, `grease`,
`golem`, Golem agent executables, `kill`, `curl`/`wget`, the registered coreutils/text-tool builtins,
the filesystem layout, the authorization model, exit codes, and the shell language) — lives in one
place: **[`docs/USAGE.md`](docs/USAGE.md)**.

For the mechanisms behind four cross-cutting constraints that shape a lot of the above — why async
work (HTTP, the LLM call, MCP, agent invocation) can only run as a single top-level stage per line, why
filesystem writes on the durable agent must be whole-file rather than append, what wasip2's missing
primitives force, and how command/path resolution splits across `/bin`, `/proc`, `/mnt/mcp`, and
`$PATH` — see **[`docs/architecture/`](docs/architecture/)**.

For a contributor-facing walkthrough of how a line of input actually flows through the shell, see
**[`docs/ONBOARDING.md`](docs/ONBOARDING.md)**.
