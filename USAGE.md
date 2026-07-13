# clank.sh — Usage Reference

clank.sh is an **AI-native shell**: a fork of the [Brush](https://github.com/reubeno/brush)
bash-compatible interpreter, compiled to `wasm32-wasip2` and deployed as a **durable
[Golem](https://www.golem.cloud/) agent**. It looks and drives like bash — pipes, `$(...)`,
redirects, here-docs, `&&`/`||`, job control — but every capability is a CLI command on `$PATH`
with a manifest, an authorization policy, and `--help`. The AI (`ask`) sees and operates the exact
same command surface a human does.

This document is a practical, code-grounded reference to **every** command and subsystem. Flags and
subcommands here are drawn directly from the dispatch ladder in
`crates/clank-shell/src/session/mod.rs` and the per-command classifier modules — nothing is invented.

> **Environment note.** clank runs as a single WebAssembly component. There is no `fork`, no `exec`,
> no real OS processes, and no Unix signal kernel. PIDs are synthetic handles on internal async work
> and remote invocations. Anything requiring a live Golem cluster (`ask`, `mcp add`, `grease
> install`, `golem`, agent invocations) degrades to an *informative error* on the native/no-cluster
> build rather than crashing. See README `## How It Works` and `## Non-Goals`.

---

## Running clank on a Golem agent

clank ships as the `clank:agent` Golem component (`crates/clank-agent`, wired in `golem.yaml`). You
**build** it to wasm, **deploy** it to a running Golem server, and then **invoke** its methods to run
shell commands inside the durable agent. All commands below run from the repo root (the directory
containing `golem.yaml`).

**Prerequisites:** the [`golem` CLI](https://learn.golem.cloud) (≥ 1.5), `cargo` with a Rust toolchain
(pinned by `rust-toolchain.toml`), and — for scripting the JSON output — `jq`.

### Build, serve, deploy

```sh
# 1. Compile crates/clank-agent to the wasm32-wasip2 component.
#    -Y auto-confirms the golem-managed AGENTS.md section update (needed in non-interactive shells).
golem -Y build

# 2. Start a local Golem server (binds the default router port 9881; the CLI's active
#    profile talks to it). Leave this running in its own terminal, or add --detach.
golem server run

# 3. Deploy the built component to that server.
golem -Y deploy
```

> **`ask` and the API key.** The `ask` command's durable LLM provider reads `ANTHROPIC_API_KEY` from
> the agent env, which `golem.yaml` passes through at deploy time (strict Jinja substitution — the host
> var must be defined). Export it before `golem deploy` to use `ask`; leave it unset and `ask` cleanly
> reports "not configured" (exit 4) while every other command works. **Never** hard-code the key.
>
> A stale `target/` is the usual cause of a `golem deploy` "failed to parse WebAssembly module" — run
> `cargo clean -p clank-agent -p clank-shell --target wasm32-wasip2` and rebuild.

### Driving the agent

The agent is addressed by **type + a constructor name**: `ClankAgent("<name>")`. The name *is* the
agent's identity — distinct names are fully isolated durable instances, each with its own shell state,
transcript, and filesystem; the **same** name across invocations resumes the same live session (state
persists — that is the durability).

Every command goes through a single entry point, **`eval "<cmd>"`**, which runs one command line and
returns a structured record: `stdout`, `stderr`, `exit_code`, and `pending_prompt` (set when the line
surfaced a `prompt-user`/authorization pause). `eval` takes **one** argument, a **WIT string literal**,
so the command must be wrapped in quotes *inside* the shell-quoted argument (`'"…"'`):

```sh
# Run a command; golem pretty-prints the returned record (stdout / stderr / exit_code / pending_prompt).
golem agent invoke 'ClankAgent("dev")' eval '"mkdir -p /tmp/work; echo hi > /tmp/work/a; cat /tmp/work/a"'

# Same instance, later invocation — the file is still there (durable state).
golem agent invoke 'ClankAgent("dev")' eval '"cat /tmp/work/a"'

# Just the text: `-n` suppresses the live invocation-log stream so stdout is a single JSON document
# (without it, the `[INVOKE]`/`[STREAM]` log lines print first and choke jq), then pull a field.
golem agent invoke -q -n --format json 'ClankAgent("dev")' eval '"ls /bin"' | jq -r '.result_json.value.stdout'
```

**Interactive pauses** (`prompt-user`, and the authorization gate on destructive/outbound commands
like `rm`, `curl`, `ask`) never block the agent. `eval` returns immediately with `pending_prompt` set;
you deliver the human's answer on a **separate** invocation:

```sh
# A bare `rm` (sudo-only) surfaces a confirmation instead of deleting — eval returns pending_prompt.
golem agent invoke -q -n --format json 'ClankAgent("dev")' eval '"rm /tmp/work/a"' | jq '.result_json.value.pending_prompt'

# Approve it (or "no" to deny, exit 5) on a follow-up call:
golem agent invoke 'ClankAgent("dev")' answer_prompt '"yes"'
# …or abort the outstanding prompt (Ctrl-C convention, exit 130):
golem agent invoke 'ClankAgent("dev")' abort_prompt
```

Prefix a command with `sudo` to pre-authorize it and skip the pause (`sudo rm …`, `sudo curl …`,
`sudo ask …`). Everything after this point in the document is a command you run *through* the agent
this way — via `eval`.

### Interactive REPL

Typing `golem agent invoke … eval …` per command gets old fast. `scripts/clank-repl.sh` wraps it in a
readline loop against one durable agent: you type a command, it prints the result, and it drives the
confirmation pauses for you (surfacing the question, reading your answer, resolving it). The session —
files, cwd, transcript — persists across lines and across REPL restarts (same `--name` resumes it).

```sh
scripts/clank-repl.sh                 # attach to an already-running server + deployed clank
scripts/clank-repl.sh --deploy        # or build + serve + deploy a throwaway one first
scripts/clank-repl.sh --name demo     # pick the durable agent instance (default: "repl")
echo 'ls /bin' | scripts/clank-repl.sh   # non-interactive: pipe commands in as a scriptable driver
```

At a confirmation pause it accepts `y`/`n`/`a` (or the full `yes`/`no`/`all`); type `:abort` to cancel
one. Quit with `exit`, `:q`, or Ctrl-D.

> **Worked end-to-end example.** `scripts/golem-e2e.sh` builds, serves, deploys, and exercises the
> whole command surface against a throwaway agent, then tears it down — read it as an executable
> reference for every invocation shape above (add `--with-llm` for the live-`ask` assertions,
> `--with-grease` for a signed local package registry, `--keep` to leave the agent up to poke at).
>
> **Iterating on the Rust?** `golem build` may report the component `[UP-TO-DATE]` after you edit
> `clank-shell` (it tracks the `clank-agent` component dir, not its path-dependency), deploying a
> stale wasm. Force a fresh build with `cargo clean -p clank-shell -p clank-agent --target wasm32-wasip2`.

---

## Table of contents

0. [Running clank on a Golem agent](#running-clank-on-a-golem-agent)

1. [`ask` — the AI interface](#1-ask--the-ai-interface)
2. [`context` — the session transcript](#2-context--the-session-transcript)
3. [`model` — models and providers](#3-model--models-and-providers)
4. [`prompt-user` — human-in-the-loop](#4-prompt-user--human-in-the-loop)
5. [`mcp` — MCP servers, tools, sessions, resources](#5-mcp--mcp-servers-tools-sessions-resources)
6. [`grease` — the package manager](#6-grease--the-package-manager)
7. [`golem` — the cluster command](#7-golem--the-cluster-command)
8. [Golem agent executables](#8-golem-agent-executables)
9. [`kill` — cancel in-shell work](#9-kill--cancel-in-shell-work)
10. [`curl` / `wget` — HTTP](#10-curl--wget--http)
11. [Registered builtins (coreutils, text tools, and friends)](#11-registered-builtins)
12. [Filesystem layout](#12-filesystem-layout)
13. [Authorization model](#13-authorization-model)
14. [Exit codes & shell language](#14-exit-codes--shell-language)

---

## 1. `ask` — the AI interface

`ask` is clank's primary human↔AI interface. It sends the shell's sliding-window **transcript as the
model's context** (the exact bytes `context show` renders — "the AI reads exactly what you see")
plus your prompt to an LLM, runs an agentic tool-calling loop, and prints the reply.

`ask` is an **outbound-HTTP** command, so its policy is `confirm`: a bare `ask` pauses for your
approval (using the same mechanism as `prompt-user`); `sudo ask` pre-approves it *and* pre-approves
the `[confirm]`-tier shell commands the model calls during the loop. `sudo` never satisfies a
`sudo-only` command — destructive tool calls still refuse. (README `### ask`.)

```
ask [--fresh|--no-transcript] [--inherit] [--json] [--model <id>] <prompt>
```

| Flag | Effect |
|---|---|
| `--fresh`, `--no-transcript` | Send **no** transcript context — the model sees only your prompt (and any piped stdin). |
| `--inherit` | Default. Send the full transcript window. Accepted for explicitness. |
| `--json` | The model's final answer must be a single valid JSON value. Valid JSON prints on stdout (exit 0); otherwise exit **6** with the raw response on stderr. |
| `--model <id>` | Override the model for this call. Resolution order: `--model` > `ask.toml` default > built-in `claude-haiku-4-5-20251001`. |

Unrecognized `--flags` are treated as prompt text (kept deliberately permissive). The default model
is the lightest/cheapest (haiku); opt into a bigger model explicitly.

```sh
# Basic question (pauses for confirmation because ask does outbound HTTP)
ask "what does this repo build?"

# Pre-approve with sudo; also pre-approves the [confirm] commands the model runs
sudo ask "find every TODO in the tree and summarize the themes"

# No prior context, structured output
ask --fresh --json "list three risks as an array of strings"

# Pick a stronger model for one call
ask --model claude-sonnet-4-5 "review the diff for correctness bugs"
```

### Piped stdin as context

`ask` may be the **final** stage of a pipeline; the upstream's stdout is captured and appended to
the first user message as a supplementary block *after* the transcript and prompt. Only a literal
`|` counts — `||`, `;`, `&&` do **not** make `ask` a pipe tail. The LLM call can't run inside
Brush's pipeline machinery, so the session pre-extracts the upstream, runs it, and dispatches the
`ask` tail at the session layer.

```sh
cat error.log | ask "what is the root cause?"
cat a.txt | grep FATAL | ask --json "group these failures by service"
cat secrets-report.md | sudo ask "any concerns?"   # sudo on the tail pre-approves
```

### `ask repl` — interactive AI session (native-only)

```
ask repl [--fresh|--inherit] [--model <id>]
```

Starts an interactive AI session with its **own isolated transcript** (separate from the main shell
transcript). `--fresh` (default) seeds an empty transcript; `--inherit` copies the current shell
transcript in as the starting context. Inside the REPL, lines starting with `:` are meta-commands;
everything else is a prompt sent to the model (conversational — no shell tools in the REPL loop):

| Meta-command | Effect |
|---|---|
| `:exit`, `:quit` (or Ctrl-D) | Leave the REPL; the session content is printed as rendered output. |
| `:new-session` | Reset the REPL's isolated transcript to empty. |
| `:model [<id>]` | With an id, switch the REPL's model; with none, print the current model. |

```sh
ask repl --inherit --model claude-sonnet-4-5
```

> **On the durable agent, `ask repl` is an honest stub.** An interactive REPL needs a terminal and a
> blocking read loop, which the durable agent cannot own (Golem serializes invocations per agent — it
> can't park mid-loop waiting for a human). Running `ask repl` on the agent returns a pointer telling
> you to drive a conversation with repeated `ask` calls (each call is one turn). The interactive REPL
> is a native-terminal feature. (README `### ask repl`.)

**Notes.** If no model provider is configured (e.g. the native build), `ask` returns exit **4**
("no model provider configured"). The model's tool surface grows automatically: every `grease
install` (prompts) and `mcp add` (tools) adds tools the model can call — see the system prompt live
at `/proc/clank/system-prompt`.

---

## 2. `context` — the session transcript

The transcript is a first-class value: a sliding window of the commands you ran and their output,
which is exactly what `ask` sends the model. `context` manages it.

```
context [show | clear | budget [n] | trim <n> | summarize]
```

| Subcommand | Effect |
|---|---|
| `show` (or bare `context`) | Print the transcript. **Not** recorded back into itself. |
| `clear` | Discard the whole transcript. |
| `budget` | Print the current sliding-window token budget. |
| `budget <n>` | Set the token budget to `<n>` and re-enforce it immediately. |
| `trim <n>` | Drop the oldest `<n>` entries. Pure/synchronous — composes everywhere (pipes, `$()`). |
| `summarize` | Print an **AI summary** of the transcript (one LLM turn). Needs the model. |

```sh
context show                 # what the model will see
context show | grep FATAL    # composes like any command
context budget               # e.g. -> 8000
context budget 16000         # widen the window
context trim 5               # drop the 5 oldest entries
S=$(context show)            # capture it (works via the nested-context builtin)
context summarize            # top-level only; confirms unless run with sudo
```

`context summarize` is **inspection-only**: it prints a summary but never mutates or records the
transcript. It requires the model, so it can only run as a **top-level** command — inside `$(...)`,
a pipe, `xargs`, or `eval` it returns an honest pointer error instead of a summary (the LLM call
can't run in Brush's nested runtime). Because it's outbound HTTP it is `confirm`-gated; `sudo
context summarize` pre-approves.

### Auto-compaction

When recording a new line evicts old entries to stay under the token budget, clank asynchronously
replaces the leading "dropped count" marker with a **model-generated summary block** rendered as
`[summary of N earlier entries] …`. This keeps the context legible instead of just truncating. It is
a no-op when nothing was dropped or when no provider is configured. (README `### context builtin`.)

---

## 3. `model` — models and providers

`model` lists and configures the model catalog and the default model. It's an ordinary builtin
(no HTTP, no session state) — it only reads/writes `~/.config/ask/ask.toml`, so it's free inside
pipes and `$(...)`. The `ask` path re-reads that file every invocation, so the file is the single
source of truth.

```
model list | default <id> | info [<id>] | add <provider> [--key K] | remove <provider>
```

| Subcommand | Effect |
|---|---|
| `list` (default) | Print the built-in catalog; the current default is marked `*`. |
| `default <id>` | Set the default model (writes `ask.toml`). Bare ids get the `anthropic/` prefix; an unknown provider prefix is rejected. Off-catalog ids warn but are still saved and passed through. |
| `info [<id>]` | Show provider / catalog-membership / default status for a model (or the current default). |
| `add <provider>` | **Honest stub.** Provider keys are never stored — points you at `ANTHROPIC_API_KEY`. |
| `remove <provider>` | **Honest stub.** `anthropic` is built in and cannot be removed. |

Built-in catalog: `anthropic/claude-haiku-4-5-20251001` (default), `anthropic/claude-sonnet-4-5`,
`anthropic/claude-opus-4-8`. The catalog is a curated subset — the provider accepts other ids,
passed through as-is.

```sh
model list
model default claude-sonnet-4-5      # canonicalizes to anthropic/claude-sonnet-4-5
model info                           # details for the current default
model add anthropic --key sk-...     # error: keys come from the environment, never echoed
```

> **Keys are never stored.** `anthropic` is built in; the agent reads `ANTHROPIC_API_KEY` from its
> environment (delivered via `golem.yaml` / Golem secrets). `model add --key …` never echoes the key
> value, even on the error path. (README `### Models and providers`.)

---

## 4. `prompt-user` — human-in-the-loop

`prompt-user` pauses the current process, presents a question to the human, and returns their reply
on stdout. It is the model→human back-channel and the human-confirmation primitive.

```
prompt-user <question> [--choices a,b,c] [--confirm] [--secret]
```

| Flag | Effect |
|---|---|
| `--choices a,b,c` | Constrain the answer to one of a comma-separated list. A non-matching answer re-asks. |
| `--confirm` | Shorthand for `--choices yes,no`. |
| `--secret` | Mark the response as secret (not echoed). |

Exactly one non-flag word is the question (quote it — every real question has spaces). Markdown piped
on stdin is prepended to the question verbatim before it's shown.

```sh
prompt-user "Which environment?" --choices staging,production,development
prompt-user "Approve this deploy?" --confirm
prompt-user "Enter the API key:" --secret
echo "# Deploy report\n\nAll checks passed." | prompt-user "Proceed?"
```

> **The pause never hangs the shell.** `prompt-user` does not block. It records a durable pending
> prompt, flips the process to the **`P`** (paused) state, and returns immediately — surfacing the
> question to whoever invoked the session (the native REPL, the AI via `ask`, an external Golem
> caller). That caller delivers the answer on the *next* eval call. Exit **0** on an answer, exit
> **130** on abort (the Ctrl-C convention). (README `### prompt-user`.)

Inside `ask`, the model reaches the human via the built-in `prompt_user` **tool**, not a
`prompt-user` shell line.

---

## 5. `mcp` — MCP servers, tools, sessions, resources

`mcp` manages [Model Context Protocol](https://modelcontextprotocol.io/) servers. Its
network-touching subcommands run at the session layer under the live WASI-HTTP reactor. MCP-lite
targets HTTPS servers.

```
mcp list
mcp add <name> <url> [--auth-env VAR] [--auth-header HEADER]
mcp remove <name> | rm <name>
mcp reload [<name>]
mcp tools <server>
mcp session list | open <server> | close <id> | info <id>
mcp watch <uri>
mcp resource info <path>
```

| Subcommand | Policy | Effect |
|---|---|---|
| `list` (default) | allow | Configured servers: url / enabled / install status / tool count or error. |
| `add <name> <url>` | confirm | Install a server (outbound HTTP). `--auth-env VAR` reads a bearer token from env var `VAR`; `--auth-header HEADER` names the header to send it in. |
| `remove` / `rm <name>` | confirm | Remove a server and its generated command stub. |
| `reload [<name>]` | confirm | Re-read config and re-install (outbound HTTP). |
| `tools <server>` | allow | List a server's tools. |
| `session list` | allow | List open sessions. |
| `session open <server>` | allow | Open a session; returns a session id. |
| `session close <id>` | allow | Close a session. |
| `session info <id>` | allow | Session details. |
| `watch <uri>` | allow | **Bounded** poll of a resource's changes (the durable agent can't hold a push stream, so this polls a fixed number of times). |
| `resource info <path>` | allow | The full annotation set for a mounted `/mnt/mcp/...` resource. |

```sh
mcp add github https://api.example/mcp --auth-env GH_TOKEN --auth-header Authorization
mcp list
mcp tools github
mcp session open github          # -> session id, e.g. s1
mcp watch "github://repo/issues" # bounded poll
mcp resource info /mnt/mcp/github/repo/README.md
```

### How installed MCP artifacts appear

Installing an MCP server (via `mcp add`, or `grease install` for a grease-packaged one) surfaces up
to three artifact types into the ordinary command surface:

- **Tools** become `<server> <tool>` commands. A generated stub lands at `/usr/lib/mcp/bin/<server>`
  (already on `$PATH`). Invocation grammar:
  ```
  <server> <tool> [--key value] [--flag] [--args '<json>'] [--json] [--session-id <id>] [--help]
  ```
  `--args '<json>'` is a raw escape hatch (single-quote it — inner double-quotes survive);
  `--session-id` binds the call to an open session; a bare `<server>` or `<server> --help` prints
  help. MCP tool calls are outbound HTTP, so they're `confirm`-gated. They also become
  `mcp__<server>__<tool>` tools the model can call from `ask`.
  ```sh
  github search --query rust --limit 5
  github call --args '{"path":"README.md"}' --json --session-id s1
  ```
- **Prompts** become `$PATH` prompt commands (they run through the model — see [§6 prompt
  packages](#6-grease--the-package-manager)).
- **Resources** mount at `/mnt/mcp/<server>/` as a virtual read-only filesystem: static files
  (`cat`/`grep`/`ls` them), **dynamic** resources fetched live on a top-level `cat`, and **resource
  templates** exposed as executables that substitute their arguments into a URI template and read the
  constructed resource.
  ```sh
  ls /mnt/mcp/github/repo
  cat /mnt/mcp/github/repo/README.md          # static
  cat /mnt/mcp/metrics/current/cpu-usage      # dynamic (fetched live at top level)
  ```

---

## 6. `grease` — the package manager

`grease` installs capability packages from configured registries. Its `install`/`update`/`search`
subcommands do outbound HTTP.

```
grease registry add <url> [--key <ed25519-pubkey>] | list | remove <url>
grease install <name> [--tools] [--prompts] [--resources]   (alias: add)
grease remove <name> | rm <name> | uninstall <name>
grease list | ls
grease search <query>
grease info <name>
grease update [<name>] | upgrade [<name>]
```

| Subcommand | Policy | Effect |
|---|---|---|
| `registry add/list/remove` | allow | Manage registries. `--key` attaches a trusted base64 ed25519 signing key to a registry. |
| `install <name>` | confirm | Install a package (outbound HTTP). The registry declares the package kind. |
| `remove <name>` | confirm | Uninstall. |
| `update [<name>]` | confirm | Re-fetch installed packages (outbound HTTP). |
| `list` | allow | Installed packages. |
| `search <query>` | allow | Search the registries. |
| `info <name>` | allow | A package's metadata / generated help. |

The `--tools` / `--prompts` / `--resources` selectors on `install` choose which artifact types of an
**MCP** package to install (ignored for other kinds). No selector = install all three.

```sh
grease registry add https://reg.example --key BASE64ED25519PUBKEY
grease search review
grease install code-review           # kind declared by the registry
grease install github --tools --resources
grease info code-review
grease list
grease update
```

### The six package kinds

The registry declares each package's kind; `grease` installs it accordingly (README `### Package
taxonomy`):

1. **prompt** — installs as an executable on `$PATH` (`/usr/lib/prompts/bin/<name>`). Running it
   fills the stored prompt body from its arguments and sends it through the model (like `ask`).
   `confirm`-gated; `sudo` pre-approves.
2. **mcp** — an MCP server; its tools/prompts/resources land as described in [§5](#5-mcp--mcp-servers-tools-sessions-resources).
3. **agent** — a Golem agent; becomes an executable at `/usr/lib/agents/bin/<name>` with the
   invocation grammar of [§8](#8-golem-agent-executables).
4. **script** — a shell script executable at `/usr/bin/<name>` that runs local shell source.
5. **skill** — a capability-context package under `/usr/share/skills/<name>/` (documents + bundled
   `$PATH` scripts). A skill is **not itself a callable command** — it's context the model consults.
6. **+wRPC** — reserved on the roadmap (README only); not yet an installable kind.

### Integrity: signing, content-addressing, transparency log

Package payloads are **content-addressed** (sha256) and can be **ed25519-signed**. A registry added
with `--key` has its packages' signatures verified against that trusted key. clank additionally
verifies **RFC-6962 transparency-log inclusion proofs**, so a signed package is provably present in
the registry's append-only log. (All hand-rolled on `sha2`; no external crypto dependency.)

### Authoring a prompt as YAML-frontmatter `.md`

A registry can serve a prompt as a `<name>.md` file with a `---` YAML frontmatter block (metadata:
name, description, authorization, etc.) followed by the Markdown prompt body. `grease` converts the
verified `.md` into an installed prompt command. This is the human-friendly authoring form for
prompt packages.

```markdown
---
name: standup
description: Draft a standup update from the transcript
authorization: confirm
---
Summarize what I did in this session as a three-bullet standup update.
```

---

## 7. `golem` — the cluster command

`golem` exposes the Golem **runtime** API subset — talking to a running cluster. It deliberately
**excludes** build/push/deploy/upload (clank is not a development tool). Needs a configured cluster;
otherwise every subcommand returns an honest error.

```
golem agent list
golem agent oplog --type <t> [--<ctor> v ...] [-n <count>]
golem agent status --type <t> [--<ctor> v ...]
golem agent interrupt <pid>
golem agent resume <pid>
golem connect <identity>
golem oplog
golem rollback
golem fork
```

| Subcommand | Policy | Status |
|---|---|---|
| `agent list` | allow | List running agents. |
| `agent oplog --type <t> …` | allow | An agent's oplog (`-n` limits entries; `--<ctor> v` selects the instance). |
| `agent status --type <t> …` | allow | An agent's status/metadata. |
| `agent interrupt <pid>` | — | **Honest stub** — no guest host binding for this control-plane op. |
| `agent resume <pid>` | — | **Honest stub** — same reason. |
| `connect <identity>` | allow | Inspect a running agent by identity (oplog/files/status). |
| `oplog` | allow | The shell instance's own oplog. |
| `rollback` | confirm | Rewind the shell instance's state. |
| `fork` | confirm | Fork the current shell instance. |

```sh
golem agent list
golem agent status --type ShoppingCart --userid jd
golem agent oplog --type ShoppingCart --userid jd -n 50
golem connect shopping-cart:jd
golem oplog
```

> `golem agent interrupt` / `resume` are distinct from `kill` — they act on the remote agent
> instance, not on invocations of it. Both are v1 honest stubs (no host primitive). (README `### The
> golem command`.)

---

## 8. Golem agent executables

A grease-installed Golem **agent** package becomes an executable at `/usr/lib/agents/bin/<name>`.
Running it builds an invocation and dispatches it through the injected wRPC invoker in the selected
mode. Constructor parameters are flags; methods are subcommands.

```
<agent> [--<ctor> value ...] [--trigger | --schedule <iso8601> | --phantom <uuid> | --revision <n>] <method> [-- --<arg> value ...]
```

**Grammar.** Before the method word: wrapper flags (below) and any `--<ctor>` flag that matches a
declared constructor parameter. The first bare word is the **method** (kebab-case). An explicit `--`
separates the method from its `--<arg> value` arguments. Constructor and argument order is preserved
(Golem's agent identity is the ordered constructor tuple).

| Wrapper flag | Mode / effect |
|---|---|
| *(none)* | **await** (default) — invoke and return the rendered result. |
| `--trigger` | **fire-and-forget** — returns a handle + a PID row; `kill <pid>` cancels it if still queued. |
| `--schedule <iso8601>` | **deferred** — run at a future ISO-8601 time; returns a cancelable handle + PID row. |
| `--phantom <uuid>` | Address a phantom instance by UUID. |
| `--revision <n>` | **Honest stub** — component-revision targeting is a `golem:api` concern with no wasm-rpc slot; returns exit 2. The invocation always targets the running revision. |

**Reserved subcommands** (cannot be method names):

- `oplog`, `status` — routed through the cluster seam (need a configured cluster). Not available for
  an *ephemeral* agent type (exit 2).
- `stream`, `repl` — long-lived/interactive; **honest stub** on the durable agent (Golem serializes
  invocations, can't park on a stream/REPL). Use await/`--trigger`/`--schedule` instead.

A bare `<agent>` or `<agent> --help` prints the agent's generated help.

```sh
# Await (default)
shopping-cart --userid jd add-item -- --sku ABC123 --qty 2

# Fire-and-forget; keep the PID to cancel
sudo shopping-cart --userid jd --trigger recompute-totals
#   -> [<pid>] shopping-cart: triggered
kill <pid>

# Schedule for later
sudo shopping-cart --userid jd --schedule 2026-06-01T09:00:00Z send-reminder

# Phantom instance
shopping-cart --userid jd --phantom 550e8400-e29b-41d4-a716-446655440000 status
```

Without a configured cluster, agent invocation returns exit 4 ("requires a configured cluster").
Every invocation emits a structured audit event (agent type, ordered constructor params, method,
mode, phantom UUID) to `/var/log/mcp.log`. `/proc/<pid>/status` exposes `agent-type`,
`agent-params`, `agent-revision`, `phantom-uuid`, and `idempotency-key` for a live invocation.

> **Constructor parameters must never carry secrets** — they are permanently visible in `cmdline`,
> logs, and provider manifests. Secrets belong in Golem's secrets API. (README `### Executable CLI
> grammar`.)

---

## 9. `kill` — cancel in-shell work

There are no OS processes and no signals in the sandbox — `kill` cancels **in-shell** work: a
background job (its future is aborted) or a `P`-state prompt-paused process (same as aborting the
prompt) or a queued/scheduled agent invocation.

```
kill [-s SIG | --signal SIG | -SIG | -N] (<pid> | %<jobspec>)...
```

- Signal specifiers (`-9`, `-KILL`, `-SIGTERM`, `-s TERM`, `--signal TERM`, `-n N`) are **parsed and
  ignored** — every kill is the synthetic terminate.
- `-l` / `-L` / `--list` return an **honest error** (signal listing is not supported; signals aren't
  mapped).
- Job specs are Brush's: `%1`, `%%`, `%+`, `%-`.

Exit **0** when every target was cancelled; **1** on any miss. A killed background job reports as
`Killed`. A killed queued/scheduled agent invocation reports `cancelled`; a killed fire-and-forget
one reports it was already dispatched (local tracking cleared, remote cancel not guaranteed).

```sh
sleep 100 &          # [1] 12345
jobs
kill %1              # cancel by job spec
kill 12345           # cancel by synthetic PID
kill -9 12345        # the -9 is ignored; still a synthetic terminate
```

---

## 10. `curl` / `wget` — HTTP

`curl` and `wget` are real outbound-HTTP commands (dispatched to the `wcurl`/`waget` crates so the
request runs under the live WASI-HTTP reactor). They are `confirm`-gated. Only a small, deliberate
flag set is supported — **unknown flags are an error** so a typo isn't swallowed as a URL.

### `curl`

```
curl [-X METHOD] [-H "Key: Value"]... [-d DATA] [-o FILE] [-s|--silent] <url>
```

| Flag | Effect |
|---|---|
| `<url>` | Positional target (required). |
| `-X, --request <METHOD>` | HTTP method. Invalid method → error. |
| `-H, --header "K: V"` | Add a header. Repeatable. No colon → empty-valued header. |
| `-d, --data <DATA>` | Request body. Implies `POST` unless `-X` is given. |
| `-o <FILE>` | Write the body to `FILE` instead of stdout. |
| `-s, --silent` | Suppress the non-2xx status message on stderr. |

```sh
curl https://example.com/health
curl -s -H "Accept: application/json" https://api.example/v1/status
curl -X POST -d '{"k":1}' -H "Content-Type: application/json" https://api.example/ingest
curl -o out.json https://api.example/data
```

### `wget`

```
wget [-O FILE | -O -] [-q|--quiet] <url>
```

| Flag | Effect |
|---|---|
| `<url>` | Positional target (required). GET only. |
| `-O, --output-document <FILE>` | Write to `FILE`. `-O -` streams to stdout. |
| `-q, --quiet` | Suppress progress/saved/status messages. |

Default output (no `-O`) is a file named after the URL's last path segment, falling back to
`index.html` for a host-only URL.

```sh
wget https://example.com/dir/report.txt      # -> report.txt
wget -O - https://example.com/data | wc -l   # stream to stdout
wget -q -O out.bin https://example.com/blob
```

---

## 11. Registered builtins

clank registers its own builtins beside Brush's defaults. Each has a manifest, `--help`, an
authorization policy, and composes via pipes. `type` is the authoritative resolver.

### Coreutils (uutils-backed)

| Command | Synopsis | Notes |
|---|---|---|
| `cat` | concatenate files and print to stdout | Serves the virtual `/proc` and `/bin` namespaces. |
| `ls` | list directory contents | Serves virtual namespaces. |
| `wc` | count lines, words, and bytes | |
| `head` | print the first lines of a file | |
| `tail` | print the last lines of a file | |
| `sort` | sort lines of text | |
| `uniq` | report or omit repeated lines | |
| `cut` | select fields from each line | |
| `tr` | translate or delete characters | |
| `mkdir` | create directories | |
| `mv` | move or rename files | |
| `cp` | copy files and directories | |
| `rm` | remove files and directories | **`sudo-only`** — destructive. Needs `sudo`. |
| `env` | print the environment | |
| `tee` | copy stdin to stdout and files | |
| `touch` | create files or update timestamps | |
| `sleep` | pause for a duration | |
| `printf` | format and print data | `%s %d %x %f`, `\n` escapes. `printf -v VAR` is **not** supported. |

```sh
ls -l /usr/bin | grep review
cat error.log | tail -20 | sort -u
sudo rm stale.tmp          # rm is sudo-only
printf '%s=%d\n' count 42
```

### Text tools

| Command | Synopsis |
|---|---|
| `jq` | filter and transform JSON |
| `grep` | search files for a pattern |
| `sed` | stream editor (`s///`, `d`, `p`, `q`; line/regex addresses) |
| `awk` | pattern scanning and text processing |
| `diff` | compare files line by line |
| `patch` | apply a diff to a file |
| `file` | identify file type |

```sh
curl -s https://api.example/data | jq '.items[].name'
grep -rn TODO . | awk -F: '{print $1}' | sort | uniq -c
diff old.txt new.txt | patch old.txt
```

### Shell / meta utilities

| Command | Synopsis / usage |
|---|---|
| `ps` | report process status. `ps aux` / `ps -ef` give standard columns incl. PPID; `%CPU`/`%MEM` show `-` (not available in WASM). |
| `which` | locate a file-backed command on `$PATH` (file-backed only — use `type` for everything). |
| `man` | display command documentation (renders a command's manifest help). |
| `stat` | display file status. `-c FORMAT` directives: `%n` name, `%s` size, `%F` type, `%y/%Y` mtime, `%x/%X` atime, `%w/%W` birth, `%%` literal; unknown → `?`. Virtual paths (`/bin`, `/proc`) report as read-only virtual entries. |
| `find` | `find [PATH...] [-name GLOB] [-iname GLOB] [-path GLOB] [-type f\|d] [-maxdepth N] [-mindepth N]` — implicit `-print`; predicates AND together. **No `-exec`** — pipe into `xargs`. |
| `xargs` | `xargs [-n MAX-ARGS] [-I REPLACE] [-d DELIM] [COMMAND [ARG...]]` — read tokens from stdin and run COMMAND (default `echo`) in batches; `-I` runs once per token with REPLACE substituted. Runs commands as in-shell builtins (no exec). |

```sh
ps aux
type ask                       # authoritative resolution
which grep
man stat
stat -c '%s %F' README.md
find . -name '*.rs' -type f -maxdepth 2
find . -name '*.txt' | xargs wc -l
find . -name '*.log' | xargs -I{} sh -c 'echo {}'
```

---

## 12. Filesystem layout

A virtual, LLM-legible directory structure. Package bin directories are already on `$PATH`.

| Path | Contents |
|---|---|
| `/usr/lib/prompts/bin/<name>` | grease-installed **prompt** command stubs. |
| `/usr/lib/mcp/bin/<server>` | generated **MCP server** command stubs. |
| `/usr/lib/agents/bin/<name>` | grease-installed **Golem agent** executables. |
| `/usr/bin/<name>` | grease-installed **script** executables. |
| `/usr/share/skills/<name>/` | grease-installed **skills** (documents + `<name>/bin/` scripts on `$PATH`). |
| `/mnt/mcp/<server>/` | mounted MCP **resources** (static files, dynamic reads, template executables). |
| `/proc/` | virtual read-only process namespace (not file-backed). |
| `/proc/<pid>/` | `cmdline`, `status`, `environ` per process. Agent invocations add `agent-type`, `agent-params`, `agent-revision`, `phantom-uuid`, `idempotency-key` to `status`. |
| `/proc/clank/system-prompt` | the current system prompt as it would be sent to the model on the next `ask` — computed on read from installed tools/skills/config. `cat`-able, `grep`-able. |
| `/var/log/shell.log` | executed shell lines. |
| `/var/log/http.log` | outbound HTTP (curl/wget/LLM turns — model + tool count + outcome, never bodies). |
| `/var/log/mcp.log` | MCP tool calls + Golem agent-invoke audit events. |
| `/var/log/ops.log` | destructive/`sudo-only` operation attempts with authorization outcome (recorded even when denied). |
| `/var/lib/grease/` | the versioned grease payload store. |
| `/etc/grease/` | `registries.toml` + one `<name>.toml` per installed package. |
| `/etc/mcp/` | per-server MCP config. |
| `~/.config/ask/ask.toml` | the default-model config (`model` writes it, `ask` reads it). |

```sh
cat /proc/clank/system-prompt | grep -i mcp
ps aux; cat /proc/<pid>/status
cat /var/log/ops.log         # who tried what destructive op, and whether it was authorized
ls /mnt/mcp
```

Most of these honor `$CLANK_*` env overrides (e.g. `$CLANK_LOG_DIR`, `$CLANK_GREASE_ETC`,
`$CLANK_MCP_BIN`) so native tests are hermetic; the agent uses the defaults above.

---

## 13. Authorization model

Every command carries an **authorization policy** in its manifest. The leading command's policy is
enforced at a gate before it runs (README `### Policy model`):

| Policy | Behavior |
|---|---|
| `allow` | Runs immediately (read-only / low-risk: `ls`, `cat`, `grep`, `context show`, `mcp list`, `grease list`, `golem oplog`, …). |
| `confirm` | Pauses for the human's approval before running (outbound HTTP and state-changing ops: `ask`, `curl`, `wget`, `mcp add`, `grease install`, MCP tool calls, agent invocations, `golem rollback`/`fork`, …). Reuses the `prompt-user` mechanism. |
| `sudo-only` | Refused unless invoked with `sudo` (destructive: `rm`, overwrites). |

**`sudo` = human authorization**, not a real executable. A leading `sudo` token is stripped before
dispatch; it marks the line as human-authorized:

- `sudo <cmd>` satisfies a `confirm` or `sudo-only` command without a pause.
- `sudo ask "…"` additionally grants the agentic loop **blanket confirm-tier** authorization — the
  model's `[confirm]` tool calls run without a per-call pause. It never satisfies `sudo-only` (the
  model still can't `rm` without an explicit `sudo` in the tool call).

Commands with subcommands (`mcp`, `grease`, `golem`) are gated **per-subcommand** — the top level is
`allow` and only the mutating subcommands carry `confirm`.

A `confirm` that isn't pre-authorized surfaces a confirmation and moves the process to the **`P`
(paused)** state — first-class and visible in `ps`, `jobs`, and `/proc/<pid>/status` — until you
approve or deny. Denied → exit **5** (authorization failure). Every `sudo-only` attempt is logged to
`/var/log/ops.log` with its outcome (`authorized` / `denied` / `confirm-required`), recorded even
when blocked.

```sh
rm build.tmp             # refused: rm is sudo-only
sudo rm build.tmp        # authorized
ask "clean the repo"     # pauses for confirmation (outbound HTTP)
sudo ask "clean the repo"  # pre-approves ask + its [confirm] tool calls (not sudo-only ones)
```

> Sensitive environment variables must not be exposed; agent constructor parameters must never be
> secret-bearing (they're permanently visible). Secrets live in Golem's secrets API. (README `###
> Sensitive environment variables`.)

---

## 14. Exit codes & shell language

### Exit codes

From README `### Exit codes`:

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | General error |
| `2` | Invalid usage / bad arguments |
| `3` | Timeout — model call, agent invocation, or MCP tool call exceeded the time limit |
| `4` | Remote call failed — HTTP error / connection failure from the model provider or MCP server (also: no provider / no cluster configured) |
| `5` | Authorization failure — approval denied, insufficient privilege, or `sudo-only` invoked without authorization |
| `6` | Malformed JSON (when `--json` output expected) — the raw model response goes to stderr |
| `7` | Golem not available |
| `126` | Command not executable |
| `127` | Command not found |
| `130` | Interrupted (Ctrl-C) — includes a `prompt-user` abort |

### Shell language

The scripting language is **bash-compatible**, derived from Brush's POSIX/bash implementation
(validated against ~1400 integration cases using bash as an oracle). Supported as you'd expect:

- Pipes (`|`), command substitution (`$(...)` and backticks), redirections (`>`, `>>`, `<`, `2>`,
  `&>`, `2>&1`).
- Logical operators `&&` / `||`, sequencing `;`.
- Here-documents: `<<EOF` (expansion active), `<<'EOF'` (literal), `<<-EOF` (strips leading tabs).
- Variables, `export`, `cd`, functions, globbing, quoting.
- Synthetic **job control** over clank processes: `&`, `jobs`, `fg`, `bg`, `wait`, `kill`.

**Known gaps.** Brush upstream limitations (README `### Known scripting gaps`): `coproc`, `select`,
`ERR` traps, and some `set`/`shopt` flag behavior. Plus one wasm-specific gap: **process substitution**
(`<(...)`, `>(...)`) is not supported on the agent — it needs a concurrent producer/consumer connected
by an OS pipe exposed as a `/dev/fd` path, which wasip2 lacks (unlike pipes, `$(...)`, and here-docs,
whose sequential shape maps onto clank's in-memory stream). Scripts relying on any of these need
adaptation. There is also no POSIX process model — no `fork`/`exec`, no real OS processes, no Unix
signals.

```sh
for f in $(find . -name '*.rs'); do wc -l "$f"; done
grep -c FATAL error.log && echo "has fatals" || echo "clean"
cat <<'EOF' > note.txt
literal $HOME, no expansion
EOF
sleep 30 & jobs; wait
```

---

*Cross-references point at section headings in `README.md`. Flags and subcommands in this document
are sourced from `crates/clank-shell/src/session/mod.rs` (the `run_command` dispatch ladder) and the
per-command `cmd.rs`/`classify` modules (`mcp/`, `grease/`, `golem/`, `ai/`, `tools/`, `builtins/`),
plus `crates/wcurl` and `crates/waget` for HTTP.*
