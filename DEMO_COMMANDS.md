# main-rc-1 — demo commands (native + agent)

Copy-paste workflows for **both** targets: native (`./target/debug/clank`) and the durable Golem
agent (`golem agent shell`). Commands that behave differently across targets are flagged inline; a
full parity table is at the bottom.

- Bring up native: [NATIVE_RC_TESTING.md](NATIVE_RC_TESTING.md)
- Bring up the agent: [RC_TESTING.md](RC_TESTING.md)
- Start the grease registry / golem cluster: [PREREQUISITES.md](PREREQUISITES.md)
- Author + publish grease packages: [GREASE_REGISTRY.md](GREASE_REGISTRY.md)

> On the **agent**: run a cheap command first (`pwd`) to absorb the ~15–30s cold start, and never
> type while `ask` is running (cooked-mode type-ahead replays into the next prompt).

---

# 1 · It's a real shell

```sh
pwd
echo $((6 * 7))
whoami
uname -a
ls /bin | wc -l
ls /bin | head -5
type curl                       # curl is a shell builtin
type ask
```

### Pipes, substitution, redirection

```sh
printf "banana\napple\ncherry\n" | sort
printf "banana\napple\ncherry\n" | sort | head -1
echo "there are $(ls /bin | wc -l) builtins"
echo "written to a file" > /tmp/note.txt && cat /tmp/note.txt
seq 1 5 | awk '{ sum += $1 } END { print "sum:", sum }'
```

### Text tooling (uutils-backed)

```sh
echo "the quick brown fox" | tr 'a-z' 'A-Z'
printf "a\nb\na\nc\nb\n" | sort | uniq -c
echo "2026-07-21T10:30:00" | cut -d'T' -f1
printf "one two three\n" | sed 's/two/TWO/'
grep -c '<' <(echo unused) 2>/dev/null || echo "note: procsubst is native-only"
```

### The transcript is first-class

```sh
echo alpha
echo beta
context show                    # replays the transcript with the clank$ prefix
context trim 1                  # drop the oldest entry
context show                    # now prints "[1 earlier entries dropped]"
```

---

# 2 · Real HTTP

`curl`/`wget` dispatch to real outbound HTTP. `confirm`-gated — `sudo` pre-authorizes.

```sh
sudo curl -sI https://example.com
sudo curl -s -o /dev/null -w 'status=%{http_code} size=%{size_download}\n' https://example.com
sudo curl -sL -o /dev/null -w 'final=%{url_effective}\n' http://github.com
```

The last line follows the redirect → `final=https://github.com/`.

### curl as a pipeline head

```sh
sudo curl -s https://example.com | grep -c '<div'
sudo curl -s https://api.github.com/repos/rust-lang/rust | grep '"stargazers_count"'
```

> curl/wget work as a **top-level** command or the **first** stage of a pipeline — never
> mid-pipeline, inside `$(...)`, `xargs`, or `eval`. `ask`, by contrast, is a pipeline **tail**.

---

# 3 · MCP workflows

### Connect to a live public MCP server

```sh
sudo mcp add deepwiki https://mcp.deepwiki.com/mcp
mcp list
mcp tools deepwiki                          # the real tool schemas
```

→ `installed MCP server 'deepwiki' (3 tools)`.

### Invoke an MCP tool directly

The server name becomes a command; its tools are subcommands.

```sh
sudo deepwiki read_wiki_structure --args '{"repoName":"rust-lang/rust"}'
sudo deepwiki ask_question --args '{"repoName":"facebook/react","question":"what is a hook?"}'
```

- **Native:** recognition, `mcp tools`, and live dispatch all work — including across a restart (the
  tool list is cached in the config and reconstructed, so dispatch needs no live session).
- **Agent:** dispatch resolves directly every time.
- `mcp reload` is only for picking up server-side changes (new/changed tools) or recovering a failed
  install — not a prerequisite for dispatching already-installed tools.

### MCP resources are a virtual filesystem

Servers that expose **resources** (deepwiki does not — it's tools-only) mount them under
`/mnt/mcp/<server>/`, fetched live on read. Against a resource-exposing server, and with a **real**
path (not the `<...>` placeholders below — angle brackets are shell redirection and won't parse):

```sh
mcp add filesys https://your-resource-server/mcp     # a server that exposes resources
ls /mnt/mcp/filesys/                                  # list the mounted resources
cat /mnt/mcp/filesys/some/resource                    # fetched live on read
mcp resource info /mnt/mcp/filesys/some/resource      # full annotation set (real path required)
```

> deepwiki has no resources, so `/mnt/mcp/deepwiki/` is empty — that's expected, not a failure.

### The model can call MCP tools

Installed MCP tools also become `mcp__<server>__<tool>` tools the model can call inside `ask`:

```sh
sudo ask "use the deepwiki tools to tell me what a Rust trait object is"
```

---

# 4 · grease — the package manager

Needs the registry running (see [PREREQUISITES.md](PREREQUISITES.md)) and, to author your own,
[GREASE_REGISTRY.md](GREASE_REGISTRY.md).

### Add a registry and trust its signing key

```sh
grease registry add http://localhost:8823 --key 6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=
grease registry list
grease search hello
```

Every install verifies **sha256 content-addressing + ed25519 signature + RFC-6962 transparency-log
inclusion proof** against that trusted key before anything lands.

### Install a prompt package (→ an `ask`-backed command)

```sh
grease info hello
grease install hello
grease list
sudo hello                                  # runs the stored prompt through the model
```

### Install a parameterized prompt (frontmatter-authored `.md`)

```sh
grease install greeting
sudo greeting who=Aditya                     # fills {{who}} then sends to the model
```

### Install a script package (→ `/usr/bin/<name>`)

```sh
grease install hostinfo
hostinfo label=demo                          # echoes "demo: <hostname>"
```

### Install a skill (context, not a command)

```sh
grease install reviewing
ls /usr/share/skills/reviewing/              # documents the model consults; NOT callable
```

### Install an agent package — AGENT ONLY

```sh
grease install greeter                       # installs /usr/lib/agents/bin/greeter
greeter --help                               # generated invocation help
greeter greet who=world                      # wRPC round-trip to the GreeterAgent
```

> On **native** the agent package installs and `--help` works, but invocation needs a configured
> cluster; on the **agent** it's a live wRPC call.

---

# 5 · ask — the agentic loop

`ask` sends the transcript (+ any piped stdin) to the model, which runs real shell commands in your
shell and answers from the output. Every command is gated; `sudo` pre-authorizes the whole loop.

### Straightforward question answered from real command output

```sh
sudo ask "how many files are in /bin, and what are the first three alphabetically?"
```

### ask with piped context (ask is the pipeline tail)

```sh
ls -la /tmp | sudo ask "summarize what's in this directory listing"
sudo curl -s https://api.github.com/repos/rust-lang/rust | sudo ask "what's the star count in this JSON?"
```

### Model selection

```sh
model                                        # current default
model list                                   # the catalog
model default anthropic/claude-sonnet-4-5
sudo ask --model anthropic/claude-haiku-4-5-20251001 "one-line summary of what clank is"
```

### JSON output contract

```sh
sudo ask --json "return {\"answer\": 42} and nothing else"
```

Valid JSON on stdout or a nonzero exit (`6`) — safe to pipe into `jq` downstream in your own tooling.

### Inspect the live system prompt

```sh
cat /proc/clank/system-prompt                # computed on read from installed tools/skills/config
```

### ask repl — NATIVE ONLY

```sh
ask repl                                     # interactive session; :model / :new-session / :exit
```

> On the durable agent this is an honest error (Golem serializes per-agent invocations and can't
> park mid-loop) — drive a conversation with repeated `ask` calls instead.

---

# 6 · A combined story (good closing beat)

Connect a tool, install a prompt, and let the model use both — end to end:

```sh
sudo mcp add deepwiki https://mcp.deepwiki.com/mcp
grease registry add http://localhost:8823 --key 6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=
grease install hello
context show
sudo ask "introduce yourself, then use deepwiki to explain what an oplog is"
```

---

# 7 · Durability — AGENT ONLY

On the agent, the whole session is durable: transcript, filesystem, and in-flight work survive, and
tool calls are exactly-once. Inspect the instance's own history:

```sh
golem oplog -n 20                             # this instance's oplog (agent only)
golem status                                  # agent-id / status / component revision
```

> Natively these are honest errors — there's no in-instance oplog to read.

---

## Native vs agent — quick reference

| Capability                         | Native                          | Agent (Golem)              |
|------------------------------------|---------------------------------|----------------------------|
| Shell / pipes / redir / `$()`      | ✅                              | ✅                         |
| Coreutils / text tools             | ✅                              | ✅                         |
| Process substitution `<(...)`      | ✅                              | ❌ (wasip2 has no OS pipe) |
| HTTP curl/wget + curl-head         | ✅                              | ✅                         |
| MCP add / list / tools             | ✅                              | ✅                         |
| MCP tool **invocation**            | ✅ (survives restart)           | ✅ direct                  |
| grease prompt / script / skill     | ✅                              | ✅                         |
| grease **agent** (greeter) invoke  | needs a cluster                 | ✅ wRPC                    |
| `ask` agentic loop                 | ✅ (key at launch)              | ✅ (key at **deploy**)     |
| `ask repl`                         | ✅                              | ❌ honest error            |
| `model add --key` (store key)      | ✅ (→ ask.toml)                 | ❌ (env-only)              |
| `context summarize` (top-level)    | ✅                              | ✅                         |
| `context show` inside `$()`        | ❌ (worker-thread)              | ✅ (inline)                |
| `golem oplog` / `status` / `fork`  | ❌                              | ✅                         |
| Durability / exactly-once          | ❌                              | ✅                         |

> Anything in the `curl`/`ask`/`mcp`/`grease`/`golem`/`context summarize` family is a **top-level**
> command on both targets — it runs at the session layer and can't be nested inside `$(...)`,
> `xargs`, or `eval` (the "Wall-C" constraint the wasm HTTP reactor forces).
