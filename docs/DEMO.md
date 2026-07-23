# clank.sh — live demo

Copy commands one at a time. Everything below was run live and produced the output shown.

**Two rules that will save the demo:**
1. **Never type or paste while `ask` is running.** Wait for the answer to print. The shell has no
   prompt on screen during a long invocation, so the terminal queues your keystrokes and replays
   them into the next prompt — that's what makes output look "out of order."
2. **Use a fresh agent name every time** (`live1`, `live2`, …). An agent pins to the component
   revision it was created at, so an old name silently runs old code. New name = clean shell,
   instant, free.

---

# SETUP

## Terminal 1 — Golem cluster (spike build, has `agent shell`)

```bash
cd ~/Desktop/clank-spike
~/Desktop/clank.sh/golem-stuff/golem/target/debug/golem server run --router-port 9891 --custom-request-port 9016 --mcp-port 9017 --data-dir /tmp/clank-demo-devdata
```

## Terminal 2 — grease registry (signed + transparency-logged)

```bash
cd /tmp/clank-demo-registry && python3 -m http.server 8823
```

Signing key:
```
6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=
```

Regenerate it if needed (prints a new key):
```bash
cd ~/Desktop/clank.sh && cargo run -q --example grease-fixture -- /tmp/clank-demo-registry
```

Contents: `hello` [prompt] · `greeting` [prompt] · `hostinfo` [script] · `reviewing` [skill] · `greeter` [agent]

## Terminal 3 — deploy

Export your real key first — the agent's `ask` only works if the key is present **at deploy time**.

```bash
cd ~/Desktop/clank-spike
export ANTHROPIC_API_KEY=<your key>
export GOLEM_BUILTIN_LOCAL_URL=http://localhost:9891
~/Desktop/clank.sh/golem-stuff/golem/target/debug/golem -Y build
~/Desktop/clank.sh/golem-stuff/golem/target/debug/golem deploy --yes
```

~20s.

---

# DEMO 1 — clank running NATIVELY

No server. No Golem. One binary.

```bash
cd ~/Desktop/clank.sh
mkdir -p ~/.clank/mcp ~/.clank/mcp-bin
export CLANK_MCP_ETC=~/.clank/mcp CLANK_MCP_BIN=~/.clank/mcp-bin
export ANTHROPIC_API_KEY=<your key>
./target/debug/clank
```

> The `CLANK_MCP_*` exports are needed **only natively** — MCP config defaults to `/etc/mcp`, which
> isn't writable on macOS. On the agent it just works.

### A real shell

```sh
pwd
echo $((6*7))
ls /bin | wc -l          # 56
ls /bin | head -3        # alias, ask, awk
printf "c\nb\na\n" | sort | head -2
type curl                # curl is a shell builtin
type ask
```

*56 commands in a virtual `/bin`. `curl`, `ls`, `ask` are builtins — one static binary, never shells out.*

### Real HTTP

```sh
sudo curl -sI https://example.com
sudo curl -s -o /dev/null -w 'status=%{http_code} size=%{size_download}' https://example.com
sudo curl -sL -o /dev/null -w 'final=%{url_effective}' http://github.com
```
Last one prints `final=https://github.com/` — it followed the redirect.

### Live MCP against a real public server

```sh
sudo mcp add deepwiki https://mcp.deepwiki.com/mcp
mcp list
mcp tools deepwiki
```
→ `installed MCP server 'deepwiki' (3 tools)` and the three real tool schemas.

> Tool *invocation* works natively too — `sudo deepwiki <tool> --args '…'` dispatches directly
> (recognition routes at the Session layer, not via `$PATH`), and it survives a restart (the tool
> list is cached in the config and reconstructed). `mcp reload` is only for picking up server-side
> changes or recovering a failed install.

### The transcript is first-class

```sh
echo alpha
echo beta
context show
context trim 1
context show
```
`context show` replays the transcript using the same `clank$ ` prefix as the live prompt, so it looks
like scrollback. Run `context trim 1` between two `context show`s — the second prints
`[1 earlier entries dropped]`, which makes the change obvious.

### Model selection

```sh
model
model default anthropic/claude-sonnet-4-5
model
```

### ask — the agentic loop

```sh
sudo ask "how many files are in /bin and what are the first three?"
```
The model runs real shell commands in your shell and answers from the output. Every command is gated;
`sudo` pre-authorizes.

---

# DEMO 2 — the same shell as a durable Golem agent

```bash
cd ~/Desktop/clank-spike
export GOLEM_BUILTIN_LOCAL_URL=http://localhost:9891
~/Desktop/clank.sh/golem-stuff/golem/target/debug/golem agent shell 'ClankAgent("live1")'
```
Prompt renders `clank$`. **Use a new name each run.**

### Always first

```sh
mkdir -p /tmp
```
A fresh agent starts with an almost-empty filesystem. Anything that writes a file needs this.

### It's the same shell, on wasm

```sh
pwd
ls /bin | wc -l
printf "c\nb\na\n" | sort | head -2
```

### grease — signed package install

```sh
sudo grease registry add http://127.0.0.1:8823 --key 6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=
sudo grease install hello
type hello
hello --help
```
Real output:
```
installed hello [prompt] (sha256 9ac2d0dafc1e — verified, signed, in log)
hello is /usr/local/bin/hello
… [sha256 9ac2d0dafc1e (verified), signed by clank-fixture, in transparency log @0]
```
*sha256 + ed25519 signature + RFC-6962 transparency-log inclusion proof — all verified inside the
wasm sandbox.*

```sh
sudo hello
```
Runs the installed prompt against the model.

### A Golem AGENT installed as a command, then invoked over wRPC

```sh
sudo grease install greeter
greeter --help
sudo greeter --name g1 greet --who world
```
→ **`Hello, world! — from GreeterAgent(g1)`**

*That's a real wRPC `invoke_and_await` to a different Golem agent, exposed as an ordinary `$PATH`
command. Constructor args are flags, methods are subcommands.*

### MCP — add a server and call a tool

```sh
sudo mcp add deepwiki https://mcp.deepwiki.com/mcp
mcp tools deepwiki
sudo deepwiki read_wiki_structure --repoName golemcloud/golem
```
Returns the real DeepWiki page tree for the Golem repo.

### Model selection

```sh
model
model default anthropic/claude-sonnet-4-5
```

### ask, inside the durable agent

**Wait for each answer before typing anything else.**

```sh
sudo ask "how many files are in /bin?"
```

For anything needing the network, tell it not to pipe curl:

```sh
sudo ask "run: curl -s https://api.exchangerate-api.com/v4/latest/USD -o /tmp/r.json  then run: jq .rates.INR /tmp/r.json  — do not pipe curl"
```
→ both tool calls exit 0, real rate returned (`96.48`).

> **Why the hint is needed:** `curl` is intercepted as a builtin and the tokenizer drops pipeline
> operators, so `curl … | jq` passes `jq` to curl as a flag and exits 2. Good thing to name out loud:
> *"That's a real bug — watch what happens when I tell it to write to a file instead."*

### A second agent type, its own sandbox

```bash
~/Desktop/clank.sh/golem-stuff/golem/target/debug/golem agent shell 'GreeterAgent("g1")'
```
Prompt renders `greeter$`:
```sh
echo I am the greeter sandbox
cat /tmp/r.json      # No such file — different agent, different filesystem
exit
```

---

# GOTCHAS

Most of the original landmines were FIXED in the post-demo hardening pass (see the entries marked
✔). What remains:

1. ✔ **Type-ahead during `ask`** — fixed in the golem CLI (`agent shell` now drains queued
   keystrokes after every invocation and prints a busy line). On an older CLI build: don't type
   while `ask` runs.
2. **Fresh agent name every run.** Old names run old component revisions
   (`operation not supported on this platform` on every pipeline). This is inherent Golem
   versioning, not a bug.
3. ✔ **`/tmp` on a new agent** — fixed; the filesystem namespace is bootstrapped at session start.
4. ✔ **Piping curl** — fixed; `curl -s URL | jq .x` now works as the pipeline HEAD on both
   targets. curl mid-pipeline (or after `&&`) still declines with an honest error naming the
   supported form.
5. ✔ **Heredocs** — fixed twice over: the native REPL does real PS2 continuation (`> `), and an
   incomplete construct on the agent answers exit 2 instead of ending the session.
6. ✔ **`\n` inside `curl -w`** — fixed (quote-aware unquoting; single quotes are POSIX-literal).
7. **`export --secret NAME=VAL` works — but ONLY as a standalone line.** Operator forms
   (`&& echo ok`, a pipe, `$()`) now refuse with an honest error instead of silently bypassing
   the redaction.
8. **Avoid `ls -la`** (prints `ls-total`); `date`, `history`, `yes` don't exist on the agent.
9. **`grease`/`mcp` natively need `CLANK_GREASE_*`/`CLANK_MCP_*` env overrides** — defaults are
   `/etc/...`, unwritable on macOS. `$PATH` now honors the overrides, and MCP servers survive a
   native restart (tool lists are cached in the config).
10. **`ask` on the agent needs `ANTHROPIC_API_KEY` at deploy time**, not run time.

## Recovery

| Symptom | Fix |
|---|---|
| `operation not supported on this platform` | Old agent name — use a new one |
| `Cancelled deploying` | add `--yes` |
| deploy fails on a Jinja var | `export ANTHROPIC_API_KEY=""` |
| `Request failed after 3 retries` | Terminal 1 server isn't up |

Reconnecting is always safe — agent state is durable, nothing is lost.

## If asked "what doesn't work yet?"

- `curl` composes only as the pipeline HEAD (`curl … | jq`); mid-pipeline stays an honest decline —
  one Session-layer stage per line.
- Script-fed native sessions over-count uu pipelines (shared buffered stdin); interactive is correct.

**Test story:** one scenario corpus, two targets — `cargo test -p clank-conformance --test native`
(33/0) and the same corpus against a live agent, plus the golem-e2e suite. Workspace: 530+ passing.
