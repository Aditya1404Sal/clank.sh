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

> Tool *invocation* doesn't resolve natively (the generated command dir isn't on the native `$PATH`).
> Do that on the agent, in Demo 2.

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

1. **Don't type while `ask` runs.** No prompt is on screen during the invocation, so the terminal
   queues keystrokes and replays them into the next prompt. This is the single biggest source of
   "it's bugging out." Same reason: don't paste multi-line blocks into the agent shell.
2. **Fresh agent name every run.** Old names run old component revisions
   (`operation not supported on this platform` on every pipeline).
3. **`mkdir -p /tmp` before anything that writes a file** on a new agent.
4. **Never pipe curl.** `curl … | jq` → `wcurl: unknown option: jq`. Use `-o /tmp/f.json` then
   `jq . /tmp/f.json`.
5. **Never type a heredoc** (`cat <<EOF`) — no multi-line continuation; it errors *and ends the session*.
6. **No `\n` inside `curl -w`** — prints a literal `n`.
7. **`export --secret NAME=VAL` works — but ONLY as a standalone line.** It sets + exports the
   variable and redacts it from `env`/`ps`/the transcript. Appending anything (`&& echo ok`, a
   pipe, `$()`) silently bypasses the secret handling — keep it on its own line.
8. **Avoid `ls -la`** (prints `ls-total`); `date`, `history`, `yes` don't exist on the agent.
9. **`grease`/`mcp` natively need `CLANK_GREASE_ETC`/`CLANK_MCP_ETC`** — defaults are `/etc/...`.
10. **`ask` on the agent needs `ANTHROPIC_API_KEY` at deploy time**, not run time.

## Recovery

| Symptom | Fix |
|---|---|
| Output looks out of order / context "mixed up" | You typed during an `ask`. `Ctrl-D`, reconnect with a new name |
| `operation not supported on this platform` | Old agent name — use a new one |
| `No such file or directory (os error 44)` | `mkdir -p /tmp` |
| `wcurl: unknown option: jq` | Don't pipe curl — use `-o` |
| `Cancelled deploying` | add `--yes` |
| deploy fails on a Jinja var | `export ANTHROPIC_API_KEY=""` |
| `Request failed after 3 retries` | Terminal 1 server isn't up |

Reconnecting is always safe — agent state is durable, nothing is lost.

## If asked "what doesn't work yet?"

- `curl` can't be used in a pipeline (the intercept drops operators).
- `agent shell` accepts type-ahead during a long invocation instead of flushing stdin before
  re-prompting — makes slow `ask` runs look reordered. Small, contained fix in `interactive_shell.rs`.
- `export --secret` only takes effect as a standalone line; operator forms silently bypass it.
- Native MCP tool *invocation* doesn't resolve on `$PATH` (works on the agent).
- Script-fed native sessions over-count uu pipelines; interactive is correct.

**Test story:** one scenario corpus, two targets — `cargo test -p clank-conformance --test native`
(30/0) and the same corpus against a live agent (32/0). Workspace: 528 passing.
