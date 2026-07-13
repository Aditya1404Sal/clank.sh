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

The shell's transcript is both the human's session history and the AI's context window. Every command typed, every output rendered to the terminal, every AI response is appended to a sliding window. When you invoke `ask`, the model receives that window — no curation step, no context-population phase, no synchronization problem. The window compacts automatically at the leading edge when it approaches the token budget: the oldest portion is summarized and replaced with a visible summary block, keeping the boundary between summarized and live history explicit. Inside Golem, the full uncompacted record is preserved in the component's oplog. Content piped directly into `ask` via stdin arrives as supplementary input alongside the transcript, on a separate channel — it was never rendered to the terminal and is not part of the shared window.

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

The shell targets both `wasm32-wasip2` and native Rust. The Rust standard library covers most of both targets without abstraction (filesystem, env vars, etc.). Seams appear where crate support diverges — primarily HTTP clients (`reqwest` on native, `wstd` or equivalent on wasm-wasi). Conditional compilation handles these seams, backed by a small trait with two implementations where needed.

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

## Transcript and Context

The shell maintains a sliding-window transcript of everything rendered to the terminal: every command typed, every output produced, every AI response. This is what `ask` operates on. It is not a separate AI context that must be populated — it is the shell's own history, extended to include AI exchanges and treated as a first-class value.

The transcript is a sliding window. When it approaches the token budget, the shell compacts the leading edge: the oldest portion is summarized and replaced with a visible summary block, so the boundary between summarized and live history is always explicit rather than silently rewritten. The window then looks like:

```
[summary of prior shell transcript]
... recent history ...
```

Compaction behavior is configurable. Inside Golem, the full uncompacted record is preserved in the component's oplog regardless of what the window holds.

Redaction rules apply at all times. Anything governed by a `redaction-rules` entry in a command manifest never enters the transcript — not through direct output, not through summarization. Secrets do not leak into the AI's view of the session. Note that redaction applies to shell-managed channels: user-authored commands that deliberately echo sensitive values are outside the scope of automatic redaction.

The terminal emulator renders the transcript; it does not own it. `context clear` is an operation on a shell-owned value, not a terminal UI command. Inside Golem, the transcript is durable component state — it is as durable as anything else in the running instance.

### `context` builtin

`context` manages the transcript as a first-class value. All subcommands are composable: they read from or write to the transcript, producing output on stdout like any other command.

```
context show          # print current transcript to stdout
context clear         # discard transcript (AI starts fresh on next ask)
context summarize     # print a summary of the current transcript to stdout
context trim <n>      # drop oldest n entries from transcript
```

`context show` and `context summarize` are transcript-inspection commands: their output is written to stdout but is **not recorded back into the transcript**, regardless of whether that output reaches the terminal. This prevents the transcript from duplicating itself on inspection.

`context summarize` outputs a summary — it does not mutate the transcript in place. This makes manual compaction a composable idiom using existing primitives:

```bash
SUMMARY=$(context summarize) && context clear && echo "$SUMMARY"
```

This example also demonstrates that the `context` primitives are sufficient for any transcript management need: summarize, clear, and re-emit covers compaction; `context trim` covers pruning; `context clear` covers a clean slate.

---

## AI Integration

### `ask` and shell-state mutation

`ask` is a subprocess. It cannot mutate parent shell state: no change to cwd, no change to parent env vars, no effect on `$?` except through its own exit code.

`ask` receives a copy of the current transcript as its context. If the AI invokes `context` from inside `ask`, it operates on that copy — the same way a subprocess gets its own working directory. The parent transcript is unaffected.

The AI tool surface available to `ask` consists of `subprocess`-scoped commands only. `parent-shell` and `shell-internal` commands are not exposed as autonomous tools — they operate on shell state that `ask` cannot access as a subprocess. An agent that needs shell-state effects (changing directory, setting variables) writes a script and invokes it in a subshell, which is how any Linux process does the same thing.

`source` is a special builtin that runs in the parent shell's context. Agents cannot invoke `source` without `sudo`-level authorization.

### System prompt

`ask` constructs a system prompt automatically. The system prompt is a first-class concern: it tells the model where it is running, gives it a map of the filesystem layout and execution environment, describes its available tools and their semantics, and gives particular attention to critical tools like `prompt-user`. The exact content is a prompt engineering problem whose solution will evolve — developers working on the project should treat it as such. The current system prompt is always inspectable at `/proc/clank/system-prompt`.

### `ask`

`ask` lives at `/usr/bin/ask`. Prompt files that invoke `ask` via shebang use `#!/usr/bin/env ask` for PATH-portable resolution.

The default model is configured in `~/.config/ask/ask.toml`. The `model default` command updates it. A model specified with `--model` on the command line always takes precedence.

`ask` is a regular process. No bimodal shell, no syntax disambiguation, no mode detection.

```
ask "What's wrong with this config?"
cat error.log | ask "summarize this"
ask --model sonnet-4.6 "explain this output"
ask --json "list the top 5 causes"
sudo ask "clean up the build artifacts"
```

When content is piped to `ask`, it arrives on `ask`'s stdin as supplementary input. The model receives the transcript window first, followed by piped stdin — the transcript is the base context, stdin is appended after it. Both channels are visible to the model; neither displaces the other.

`--json` is a real output contract: valid JSON on stdout or nonzero exit (`6`). When JSON parsing fails, the raw model response is emitted to stderr so it is not lost. All other side-channel material — approvals, tool traces, warnings — also goes to stderr. This makes `ask --json | jq ...` reliable.

`sudo ask` grants the agent broad authorization for that invocation.

#### Context control flags

By default, `ask` uses the current sliding-window transcript as context. For scripting and automation where ambient conversational carry-over is undesirable, context can be controlled explicitly:

| Flag | Meaning |
|---|---|
| `--fresh` | Invoke with no transcript context; model sees only the current prompt |
| `--no-transcript` | Alias for `--fresh` |
| `--inherit` | Explicitly use the full current transcript (the default; useful to make intent clear in scripts) |

These flags compose with all other `ask` flags and with piped stdin. To use a file as context, pipe it: `cat context.txt | ask --fresh "..."`.

### `ask repl`

`ask repl` starts a REPL subprocess with its own isolated transcript. The prompt displays the active model:

```
[sonnet-4.6]>
```

Meta-commands:

```
:new-session       # discard local transcript, start fresh
:model gpt-4o      # switch model for this session
:exit              # exit the REPL
```

`Ctrl-C`: first cancels the current in-flight model turn; second exits the REPL. `Ctrl-D` and `:exit` exit cleanly.

When the REPL exits, it prints its session content to stdout like any other subprocess. The parent shell captures that output through normal terminal rendering — the printed session content enters the parent transcript once, as rendered output, with no duplication.

Multiple REPL sessions can be backgrounded with `&` and switched between using `jobs`/`fg`/`bg`.

Transcript inheritance on start:

```
ask repl              # default: summary of parent transcript injected
ask repl --fresh      # empty transcript
ask repl --inherit    # full parent transcript inherited as-is
```

The default is summary injection, not full inheritance. The transcript is a sliding window maintained at roughly half token-budget capacity; inheriting it in full by default would start the REPL already half-consumed. Summary injection gives the model useful orientation without burning context. `--inherit` is available for cases where the full detail genuinely matters.

### Models and providers

Notation: `provider/model`. Unambiguous model names can omit the provider prefix.

```
model list
model add anthropic --key $KEY
model remove anthropic
model default sonnet-4.6
model info sonnet-4.6
```

`model default` updates `~/.config/ask/ask.toml`. Provider API keys are stored in `~/.config/ask/ask.toml` on native or in Golem's secrets API when running inside Golem.

### Tool surface available to `ask`

Every model provider receives the `subprocess`-scoped command surface — installed scripts, installed prompts, MCP tool executables, MCP resource template executables, and Golem agent executables — plus installed skills. The provider packages this into whatever format its model API requires using each command's manifest.

Every `grease install` automatically expands what `ask` can do. `shell-internal` and `parent-shell` commands are excluded from the provider surface because they cannot be meaningfully invoked by a subprocess. The exception is `prompt-user`: although `shell-internal`, it is explicitly exposed to the model as a tool because it is the mechanism by which the model communicates back to the human during a task.

Skills have a shell-level metadata envelope (name, description, intended use, provider compatibility hints) even though their payload format is provider-specific.

---

## Human-in-the-Loop Workflows

### `prompt-user`

`prompt-user` is a `shell-internal` builtin that pauses the current process, presents a prompt to the human user, and returns the response to the caller — whether that caller is the model (via tool invocation) or a shell script.

`prompt-user` accepts **Markdown on stdin**. The terminal renders it as readable text — tables, emphasis, links — giving the model a rich channel for presenting context to the user before asking a question. A model can pipe a diff, a summary table, or a formatted report into `prompt-user` so the user sees exactly what they need to make a decision. Future interfaces (GUI, web, mobile) can render the same Markdown more richly with no protocol changes.

```
prompt-user "Which environment should I deploy to?"
prompt-user "Which environment?" --choices staging,production,development
prompt-user "Enter the API key for this deployment:" --secret
prompt-user "Approve this operation?" --confirm
git diff HEAD | prompt-user --confirm "Approve these changes?"
cat report.md | prompt-user "Any concerns before I proceed?"
```

Flags:

| Flag | Meaning |
|---|---|
| `--choices <a,b,...>` | Constrain response to one of the listed options; presented as a menu |
| `--confirm` | Shorthand for `--choices yes,no` with y/n rendering |
| `--secret` | Suppress echo; redact response from transcript and logs |

Behavior:

- The invoking process enters the `P` state while awaiting response
- Response is written to stdout, available as a tool result to the calling model
- Exit `0` on response; exit `130` on Ctrl-C (user abort — consistent with interrupted convention)
- stdin content is rendered as Markdown before the question text is shown
- `--secret` responses are never entered into the transcript, logs, or completion caches

`prompt-user` is clank's answer to MCP elicitation: a shell-native mechanism that composes with pipelines rather than requiring a protocol-level negotiation. MCP *sampling* remains unaddressed in v1.

### Parameterized prompts and `ask`

When a model invokes `ask` to execute a parameterized prompt, `ask`'s system prompt equips it to handle missing parameters automatically: the model recognizes unfilled `{{variable}}` tokens and uses `prompt-user` to collect them from the user before proceeding. This means parameterized prompts work out of the box without any scaffolding.

For users who want a clean scripting interface, `grease install` generates a shell script for parameterized prompts rather than installing the raw `.md` file as an executable. The generated script handles argument parsing (`getopts` or equivalent), validates required parameters, and invokes `ask` with the assembled prompt. Users can inspect and modify this script; it is the deliverable, not the source. A well-designed prompt may also use `prompt-user` internally to fall back interactively when a parameter is not supplied on the command line.

Non-parameterized prompts may be executed directly via shebang:

```bash
#!/usr/bin/env ask
Summarize the contents of this transcript clearly and concisely.
```

---

## Authorization

### Policy model

Authorization is controlled by the `authorization-policy` field in each command's manifest. There are exactly three levels:

| Policy | Meaning |
|---|---|
| `allow` | Agent may invoke freely |
| `confirm` | Agent invocation pauses for user confirmation |
| `sudo-only` | Only explicitly `sudo`-authorized invocations permitted |

This is intentionally simple. A fine-grained effect taxonomy (read-only, write, network, etc.) without policy machinery to consume it adds complexity with no benefit. The three levels map directly to what the shell can enforce, and every command's manifest entry is the sole source of truth for its authorization requirement.

| Operation | Policy |
|---|---|
| Read filesystem | `allow` |
| Write to `/tmp/` | `allow` |
| Write to `~` | `confirm` |
| Destructive ops (`rm`, overwrite) | `sudo-only` |
| Outbound HTTP | `confirm` |
| Spawn new agent | `confirm` |
| Invoke `source` | `sudo-only` |
| Modify `/etc/`, `/usr/bin/`, etc. | `sudo-only` |

When a process pauses awaiting confirmation, it enters the `P` state in `ps` and `jobs`:

```
ask has requested permission to delete /usr/bin/old-tool. (y)es, (n)o, (a)ll
```

### `sudo`

`sudo` means conscious human authorization, not Unix credentials. There is a single user. No `/etc/sudoers`, no uid 0.

Agents cannot use `sudo`. An agent that needs elevation must pause and surface a confirmation request.

`sudo ask "..."` grants the agent broad authorization for that invocation. Golem instances can be rewound, limiting blast radius for filesystem operations. HTTP calls and MCP tool invocations are not reversible by rollback.

WASI file permission bits (`chmod`, `chown`, rwx) are not implemented.

### Sensitive environment variables

`export --secret KEY=value` marks a variable as sensitive. Available to agents via the environment, but never echoed in `env`, never written to logs, never shown in `ps`, and never entered into the transcript.

---

## Filesystem

```
/
├── bin/                        # Virtual read-only namespace for special builtins
│                               # Not file-backed. Like /proc, exists for convention.
├── usr/
│   ├── local/
│   │   └── bin/                # User-local executables (highest PATH priority)
│   ├── bin/                    # User scripts and installed executables
│   │   └── ask                 # AI invocation command
│   ├── lib/
│   │   ├── mcp/bin/            # MCP tool executables and resource template executables
│   │   ├── agents/bin/         # Golem agent executables
│   │   └── prompts/bin/        # Standalone and MCP-sourced prompt executables
│   └── share/
│       ├── skills/             # Installed skills
│       │   └── <n>/            # Per-skill directory
│       │       ├── *.md        # Reference documents (AI-accessible context)
│       │       └── bin/        # Skill scripts (on $PATH)
│       ├── man/                # Man pages
│       └── doc/                # Package documentation
├── etc/
│   ├── clank/
│   │   └── config.toml
│   └── mcp/                    # MCP server configs (one file per server)
├── mnt/
│   └── mcp/
│       └── <server>/           # MCP resource mount points (see MCP Resources)
├── var/
│   ├── log/
│   └── run/
├── proc/                       # Virtual read-only namespace: process table + shell state
│   └── clank/
│       └── system-prompt       # Current system prompt; computed on read
├── tmp/
└── home/
    └── user/
        ├── .config/
        │   └── ask/
        │       └── ask.toml    # Default model, provider keys (native only)
        └── .local/
            └── share/
                └── clank/      # Native-only: shell history, local session state
```

Default `$PATH`:

```
/usr/local/bin:/usr/bin:/usr/lib/mcp/bin:/usr/lib/agents/bin:/usr/lib/prompts/bin:/usr/share/skills/*/bin
```

Scripts in `/usr/bin/` naturally shadow MCP executables with the same name by PATH order — intentional, following the same convention as `/usr/local/bin` shadowing `/usr/bin` on standard Unix systems. Installing two packages of the same type with the same name into the same directory is an error. Shadowing across directories via PATH priority is not an error; it is user-configurable via `~/.profile`.

`/bin/` is a virtual read-only namespace for special builtins. `type` is the authoritative resolver. `which` finds file-backed commands only.

`/dev/null`, `/dev/stdin`, `/dev/stdout`, `/dev/stderr` are supported.

---

## Logging

Three distinct layers:

**Human-readable process logs** — in `/var/log/`. All builtins and installed package executables emit at minimum: start event, end event, exit code, any authorization pause, and agent identity/revision for Golem invocations.

```
/var/log/
├── shell.log
├── http.log       # Outbound HTTP calls (secrets redacted)
├── mcp.log        # MCP tool invocations and responses
└── ops.log        # Destructive operations
```

**Structured audit events** — machine-readable, addressable by PID and PPID. Golem invocation entries include agent type, agent parameters, revision, phantom UUID (if present), and idempotency key.

**Golem oplog** — Golem's persistent operation journal. Not a log file. Accessed via `golem oplog`. Not in `/var/log/`. For agent types, the agent's own oplog is accessible directly:

```
shopping-cart --userid "jdegoes" oplog -n 100
```

---

## Package System (`grease`)

`grease` is the package manager. It installs, removes, and manages packages via registry HTTP services.

```
grease registry add <url>
grease registry list
grease registry remove <url>
```

Registries are identified by URL. A local registry is a localhost URL. Security principles (non-negotiable): content-addressed integrity for all package payloads; signed and transparency-auditable metadata.

`grease install <package>` discloses capability requests before completing. Installed payloads and manifests are stored in a versioned internal store. Executables in their respective directories are always derived from the store — never the source of truth.

### Package taxonomy

`grease` installs six types of packages:

**1. Standalone prompts** — `.md` files. Non-parameterized prompts are installed with `#!/usr/bin/env ask` and are directly executable. Parameterized prompts are installed as generated shell scripts in `/usr/lib/prompts/bin/` that parse arguments and invoke `ask`.

**2. MCP server artifacts** — An MCP server is a source of up to three installable artifact types, selectable at install time:

```
grease install github                      # install all artifact types (default)
grease install github --tools              # MCP tools only
grease install github --prompts            # MCP prompts only
grease install github --resources          # MCP resources only
grease install github --tools --resources  # any combination
```

- **Tools** → executables in `/usr/lib/mcp/bin/` (server name = command, tools = subcommands)
- **Prompts** → executables in `/usr/lib/prompts/bin/` — indistinguishable from standalone prompts after install
- **Resources** → mounted under `/mnt/mcp/<server>/`; resource template executables in `/usr/lib/mcp/bin/`

**3. Golem agent types** — Deployed to the configured Golem cluster; executable in `/usr/lib/agents/bin/`.

**4. Shell scripts** — Executable in `/usr/bin/`.

**5. Skills** — Installed under `/usr/share/skills/<n>/`. May contain reference documents (deposited to `/usr/share/skills/<n>/`) and shell scripts (deposited to `/usr/share/skills/<n>/bin/` and added to `$PATH`). Not a top-level command; the skill as a whole is a capability context package, but its scripts are ordinary executables.

**6. Future: wRPC WASM components** — Roadmap. Will slot in as another implementation of the internal process trait.

Every package must provide enough metadata to produce a command manifest. Packages that cannot are rejected at install time.

```
grease install <package>
grease remove <package>
grease search <query>
grease list
grease update
grease info <package>
```

### Naming convention

All executables generated by `grease` use kebab-case, matching Linux CLI convention, independent of how upstream systems name things internally.

### Name collisions

Each package type installs to its designated directory. Installing two packages of the same type with the same name into the same directory is an error. Cross-directory shadowing via PATH priority is intentional and user-configurable.

---

## MCP Server Interaction

### Transport

Only HTTPS MCP servers are supported. This is a deliberate product decision for cross-target consistency: the native and Golem targets behave identically, and the WASM target makes stdio transports impossible (no process spawning). There are no local MCP servers and no stdio transports in clank.sh.

Authentication:
- **API key auth** — handled inside clank.sh via environment variables or Golem's secrets API
- **OIDC / OAuth flows** — handled at the Golem host level when running inside Golem; requires external configuration on the native target

MCP sampling is not addressed in v1. MCP elicitation is addressed by `prompt-user`.

### Sessions

MCP server sessions have lifecycle state. Persistent sessions are required when the server advertises notifications or subscriptions. For stateless servers, session reuse is an optimization. Sessions are first-class values: `--session-id <id>` on any MCP tool invocation controls session affinity.

Session lifecycle is managed via the `mcp` command:

```
mcp session list                  # list all active MCP sessions
mcp session open <server>         # open a session explicitly; prints session ID
mcp session close <id>            # close a specific session
mcp session info <id>             # show session details and server metadata
```

`mcp session close` sends an HTTP DELETE to the server's MCP endpoint with the `Mcp-Session-Id` header, as specified by the MCP protocol. If the server responds with HTTP 405, the close is rejected by the server and clank.sh reports the refusal clearly. Sessions are not processes and do not appear in the process table.

### Tools

When an MCP server's tools are installed, the server name becomes a command in `/usr/lib/mcp/bin/`. Each tool becomes a subcommand. Executables are generated from the MCP `inputSchema` at install time.

```
github create-issue --title "Fix login bug" --body "..."
github list-prs main
github list-prs --json
github list-prs --text
github list-prs --session-id <id>   # explicit session affinity
```

### Prompts

MCP prompts are installed to `/usr/lib/prompts/bin/` — the same location as standalone prompts. After installation they are ordinary executable commands on `$PATH`, callable by human or AI without distinction. The fact that they originated from an MCP server is an installation-time detail, not a runtime property.

```
github summarize-diff --pr 42
```

### Resources

MCP resources are mounted under `/mnt/mcp/<server>/` according to their type:

**Static resources** — fetched at install/refresh time and written as regular files. The AI reads them with `cat`, `grep`, `head`, or any other standard tool. No MCP awareness required.

```
cat /mnt/mcp/github/repo/README.md
grep "TODO" /mnt/mcp/github/repo/src/main.rs
```

**Dynamic resources** — exposed as virtual files whose read handler invokes `resources/read` on the MCP server in real time. From the AI's perspective they are indistinguishable from regular files. The virtual filesystem driver is implemented at the shell level — the same mechanism used by `/proc/`. No FUSE dependency; no OS support required.

```
cat /mnt/mcp/metrics/current/cpu-usage    # fetches live data on read
ls /mnt/mcp/logs/recent/                  # directory listing from server
```

**Binary resources** — written as regular files with appropriate extensions (`image.png`, `report.pdf`), allowing content type to be inferred naturally by tools.

**Resource templates** — parameterized URI patterns that cannot be enumerated. Installed as executables in `/usr/lib/mcp/bin/` (and on `$PATH`). Template parameters become command arguments. The executable invokes `resources/read` with the constructed URI and prints to stdout.

```
github-file-lookup src/main.rs
github-search-code "TODO" --repo myrepo
```

Resource templates also appear in the `/mnt/mcp/<server>/` directory as stubs, so `ls /mnt/mcp/github/` reveals them alongside regular resource files.

**Subscriptions** — for resources that support change notifications, `mcp watch` provides a stream interface. The filesystem metaphor does not cover subscriptions honestly, so they are not represented as files.

```
mcp watch github://repo/issues
```

### Resource metadata

MCP resources carry annotations — `lastModified`, audience, priority, static vs dynamic — that are surfaced through standard Linux conventions rather than custom file formats. `stat` on any mounted resource reflects MCP metadata where available: modification time maps to `lastModified`, and resource type is reflected in the file mode. For MCP-specific fields that `stat` cannot express, `mcp resource info <path>` provides the full annotation set.

```
stat /mnt/mcp/github/repo/README.md
mcp resource info /mnt/mcp/metrics/current/cpu-usage
```

The filesystem mount gives the AI a uniform interface: everything is a file or an executable. The AI never needs to know whether content was pre-stored, fetched live, or computed on demand.

---

## Golem Agent Interaction

### Installation

`grease install golem:shopping-cart` deploys the ShoppingCart agent type to the configured Golem cluster, registers its reflected metadata, and creates an executable at `/usr/lib/agents/bin/shopping-cart`.

### Upsert invocation model

Agent identity in Golem is: agent type + constructor parameter values + optional phantom UUID. Invoking a method either finds the existing agent with that identity or creates it transparently on first call. The caller never distinguishes creation from subsequent invocation.

This is the only invocation model exposed by the installed executable. There is deliberately no `new` subcommand on the installed executable: the value of upsert semantics is that the AI never needs to reason about agent lifecycle. Introducing an explicit creation command alongside method invocation would require the AI to decide when to use it — defeating the purpose. Explicit creation with environment variables is available via `golem agent new` for human operators who need it.

Ephemerality is an agent type property, not a per-invocation choice. The installed executable works identically for durable and ephemeral types. For ephemeral types, each invocation runs on a fresh instance; the constructor parameters identify the agent type, but no state survives between calls. The upsert model and CLI grammar are the same in both cases. Manifests for ephemeral agent executables omit reserved subcommands that require persistent state (`oplog`, `status`, `repl`) since those operations have no meaning for agents with no persistent identity.

`--phantom <uuid>` allows multiple agents of the same type with the same constructor parameters to coexist, each distinguished by UUID. The canonical agent for given constructor parameters is the one without a phantom UUID.

### Executable CLI grammar

```
<agent> [<constructor-flags>] [<wrapper-flags>] <method> [--] [<method-args>]
```

- **Constructor flags** — named, kebab-case, any order; identify the agent instance
- **Wrapper flags** — reserved; control invocation behavior; always before the method
- **Method** — kebab-case subcommand naming the agent method
- `--` — explicit parse boundary (optional but unambiguous)
- **Method args** — named or positional in declaration order

Reserved wrapper flags:

| Flag | Meaning |
|---|---|
| `--revision <n>` | Target a specific agent component revision |
| `--phantom <uuid>` | Address or create a phantom agent instance |
| `--trigger` | Fire-and-forget invocation mode |
| `--schedule <iso8601>` | Schedule for future execution (backed by `schedule-cancelable-invocation` in `golem:agent@1.5.0`) |

Reserved subcommands (cannot be used as method names):

| Subcommand | Durable | Ephemeral |
|---|---|---|
| `oplog` | ✅ | ❌ |
| `stream` | ✅ | ✅ |
| `repl` | ✅ | ❌ |
| `status` | ✅ | ❌ |
| `help` | ✅ | ✅ |

### Invocation modes

```
shopping-cart --userid "jdegoes" add-item -- --sku "abc123"                           # await (default)
shopping-cart --userid "jdegoes" --trigger add-item -- --sku "abc123"                 # fire-and-forget
shopping-cart --userid "jdegoes" --schedule "2026-06-01T09:00:00Z" add-item -- --sku "abc123"
```

All three modes are backed by idempotency keys internally. All three return a PID. `kill <pid>` uses the idempotency key to cancel via Golem's pending-invocation cancellation API — effective for queued and scheduled invocations; fails with a precise error for in-progress or completed ones.

PIDs for completed invocations are lazily reaped.

```
USER   PID  PPID  STAT  COMMAND
user   204  1     R     shopping-cart --userid jdegoes add-item --sku abc123
```

### Version resolution

Default: target the agent's currently running revision. If the agent does not exist, create on the latest deployed revision. To target a specific revision:

```
shopping-cart --userid "jdegoes" --revision 3 add-item -- --sku "abc123"
```

If the agent exists and its running revision is incompatible with the executable's generated metadata, the invocation fails with a precise error identifying the mismatch.

### Constructor parameter constraints

Constructor parameters are permanently visible in `cmdline`, `ps`, logs, and provider manifests. They must be non-secret and printable. Secrets belong in Golem's secrets API. Agents obtain their own secrets from Golem's secrets mechanism at runtime.

### The `golem` command

`golem` exposes the Golem runtime API subset: the operations that talk to a running Golem cluster. This covers agent listing, creation, invocation, oplog access, interrupt/resume, status inspection, and connecting to running agents. It explicitly excludes build, push, deploy, and component upload operations — those belong in local development tooling, not in a constrained shell environment.

`golem connect` is the primary inspection mechanism: it connects to a running agent and exposes its oplog, file listing, and other runtime state. It does not expose method invocation — that is done via the installed agent executable.

The `golem` command in clank.sh shares semantics and familiarity with the Golem CLI that runs on developer machines. LLMs trained on Golem documentation should be able to operate it with minimal surprise. For the full Golem command reference, consult the Golem documentation.

```
golem agent list
golem agent new --type shopping-cart --userid "jdegoes" [--env KEY=VALUE ...]
golem agent oplog --type shopping-cart --userid "jdegoes" -n 100
golem agent interrupt <pid>
golem agent resume <pid>
golem connect <agent-identity>  # inspect a running agent: oplog, files, status
golem oplog                     # query the clank shell instance's own oplog
golem rollback                  # rewind clank shell instance state
golem fork                      # fork current shell instance
```

`golem agent interrupt` and `golem agent resume` operate at the agent level, not the invocation level. Distinct from `kill`.

Outside Golem, or without a configured cluster, all `golem` subcommands fail with a consistent, informative error.

---

## Prompts

Prompts are `.md` files with optional YAML frontmatter. Installed to `/usr/lib/prompts/bin/`. Callable by human or AI like any other command on `$PATH`. May be standalone or sourced from an MCP server; indistinguishable after installation.

A prompt is a logical package type with two runtime forms. Non-parameterized prompts are installed as shebang executables invoked directly by `ask`; parameterized prompts are installed as generated shell scripts that parse arguments and invoke `ask`. Both forms are ordinary executables from the shell's perspective — `type`, `which`, and `file` behave accordingly.

**Non-parameterized prompts** are installed as-is and made executable with `#!/usr/bin/env ask`:

```bash
#!/usr/bin/env ask
Summarize the contents of this transcript clearly and concisely.
```

**Parameterized prompts** declare their parameters in YAML frontmatter:

```markdown
---
name: summarize
description: Summarize a file with configurable output length
model: sonnet-4.6
arguments:
  - name: file
    description: Path to the file to summarize
    required: true
  - name: length
    description: "short | medium | long"
    required: false
    default: medium
---

Please summarize the contents of {{file}}.
Target length: {{length}}.
```

When `grease install` processes a parameterized prompt, it generates a shell script in `/usr/lib/prompts/bin/`. The script handles argument parsing and invokes `ask` with the assembled prompt. The generated script is inspectable and modifiable by the user.

When `ask` executes a parameterized prompt directly and parameters are missing, its system prompt equips the model to use `prompt-user` to collect them interactively. This works out of the box — the generated shell script is an enhancement for scripting ergonomics, not a requirement for basic use.

---

## Standard Utilities

**Special builtins** (`execution-scope: parent-shell` — affect shell state; POSIX-defined; cannot be overridden):

`cd`, `exec`, `exit`, `export`, `source`, `unset`

**Ordinary builtins** (`execution-scope: shell-internal` — operate on shell-internal tables; cannot run as subprocesses):

`alias`, `context`, `fg`, `bg`, `history`, `jobs`, `prompt-user`, `read`, `type`, `wait`, `which`

**Core commands** (`execution-scope: subprocess` — resolved internally; no shell state involved):

- Filesystem: `ls`, `pwd`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `touch`, `find`, `grep`
- Text: `sed`, `awk`, `sort`, `uniq`, `wc`, `head`, `tail`, `cut`, `tr`, `xargs`, `diff`, `patch`, `tee`, `printf`
- Test: `test`, `[`, `true`, `false`
- I/O: `echo`
- Time: `sleep`
- Data: `jq`
- Network: `curl`, `wget`
- Environment: `env`
- Process: `ps`, `kill`
- Metadata: `stat`, `file`
- Help: `man`

`--help` works on every command and every installed package executable.

**AI and platform commands** (`execution-scope: subprocess` unless noted — clank-specific, no Unix analog):

| Command | Scope | Purpose |
|---|---|---|
| `ask` | `subprocess` | Invoke the AI model; compose with stdin and transcript |
| `ask repl` | `subprocess` | Start an interactive AI session with its own isolated transcript |
| `model` | `subprocess` | List, add, remove, and configure model providers |
| `mcp` | `subprocess` | Manage MCP sessions (`mcp session …`) and subscriptions (`mcp watch`) |
| `mcp resource info` | `subprocess` | Show MCP-specific annotations for a mounted resource |
| `golem` | `subprocess` | Interact with the Golem cluster (runtime API subset) |
| `grease` | `subprocess` | Install, remove, and manage packages |
| `context` | `shell-internal` | Manage the transcript as a first-class value |
| `prompt-user` | `shell-internal` | Pause and collect input from the human user |

---

## Shell Language

The shell scripting language is bash-compatible, derived from Brush's POSIX/bash implementation. The features below are illustrative; the full bash language is supported except where explicitly noted as unavailable.

- `$VAR`, `${VAR}` — variable expansion
- `$(command)` — command substitution
- `>`, `>>`, `<`, `2>`, `2>&1` — redirections
- `|` — piping
- `&&`, `||`, `;` — sequencing and conditionals
- `$?`, `$0`, `$1`, `$@` — exit code and positional parameters
- `&` — background execution
- `!!`, `!n` — history recall
- Aliases, persistent to `~/.profile`
- Tab completion for all commands, installed prompts, MCP tools, agent methods and constructor parameters

### Quoting and here-documents

- `'...'` — single quotes; literal, no expansion
- `"..."` — double quotes; `$VAR` and `$(cmd)` expand; no word splitting
- `\` — escape next character
- `<<EOF` — here-document; variable and command substitution active
- `<<'EOF'` — here-document; literal, no expansion
- `<<-EOF` — here-document; strips leading tabs

Here-documents are particularly useful for multiline prompts:

```
ask <<EOF
You are reviewing this config file:
$(cat config.toml)
Identify any security issues.
EOF
```

### Known scripting gaps

Brush's bash compatibility is broad but not total. The following constructs are not supported in v1: `coproc`, `select`, `ERR` traps, and some `set`/`shopt` flag behavior (Brush upstream gaps), plus **process substitution** (`<(...)`, `>(...)`), which is unsupported on the wasm agent — it needs a concurrent producer/consumer over an OS pipe exposed as a `/dev/fd` path, which wasip2 lacks (pipes, `$(...)`, and here-documents work because their sequential shape maps onto clank's in-memory stream). Scripts relying on any of these will need adaptation. All other standard bash scripting constructs work as expected.

---

## TTY and Terminal

WASI has limited terminal support. In v1, clank.sh uses stdin/stdout only. The shell defines a Rust abstraction for basic stdin/stdout (WASI-compatible) and a separate one for full TUI. On native, the TUI abstraction is backed by real terminal APIs; on WASM, it degrades to stdin/stdout until Golem adds host-side terminal extensions. The native implementation defines the target experience.

---

## Compatibility Reference

| Feature | Native, no cluster | Native + cluster | Inside Golem |
|---|---|---|---|
| Filesystem, env, pipes, redirections | ✅ | ✅ | ✅ |
| `ask`, `ask repl`, `context` | ✅ | ✅ | ✅ |
| Durable | ❌ | ❌ | ✅ |
| MCP server tools (HTTPS only) | ✅ | ✅ | ✅ |
| MCP OIDC auth | external config | external config | ✅ host-managed |
| `grease install` (any registry URL) | ✅ | ✅ | ✅ |
| Golem agent executables | ❌ | ✅ | ✅ |
| `golem` command | ❌ | ✅ | ✅ |
| `rollback`, `golem fork` | ❌ | ❌ | ✅ |
| `golem oplog` (shell instance) | ❌ | ❌ | ✅ |
| Full TUI / `Ctrl-Z` | ✅ native terminal | ✅ | ⏳ pending TTY extensions |

| Feature | Classification |
|---|---|
| `ls`, `cd`, `pwd`, `cat`, `grep`, etc. | Unix-like — faithful |
| Pipes, redirections, `$?`, `&&`, `\|\|` | Unix-like — faithful |
| `/proc/`, `ps aux`, `/dev/null` | Synthetic but familiar |
| `%CPU`, `%MEM` in `ps` | Subset — shown as `-` |
| Job control (`&`, `jobs`, `fg`, `bg`) | Synthetic — over internal processes |
| `kill` | Subset — cancels/terminates; signal numbers not mapped |
| `Ctrl-Z`, process groups | Native-only in v1 |
| `sudo` | Synthetic — human authorization intent, not Unix credentials |
| `chmod`, `chown`, rwx bits | Unsupported |
| `rollback`, `golem fork` | Golem-only |
| Golem agent executables | clank-specific — no Unix analog |
| `ask`, `ask repl`, `context` | clank-specific — no Unix analog |
| `prompt-user` | clank-specific — no Unix analog |
| `/bin/` | Virtual read-only namespace — not file-backed |
| `/proc/clank/system-prompt` | clank-specific — virtual file, computed on read |
| MCP stdio transports | Unsupported — deliberate product decision |
| MCP resources as filesystem | clank-specific — virtual FS driver, no FUSE |
| MCP elicitation | Addressed by `prompt-user` |
| MCP sampling | Not addressed in v1 |
| `coproc`, `select`, `ERR` traps | Not supported in v1 — Brush upstream gaps |
| Process substitution (`<(...)`, `>(...)`) | Not supported on wasm — needs concurrent OS pipes |
