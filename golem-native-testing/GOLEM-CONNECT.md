# GOLEM-CONNECT.md — live-testing the clank shell over `golem agent stream`

Audience: someone who wants to **drive the real clank shell inside a real Golem agent** and see it
work — typing `echo hi`, `ask "…"`, `prompt-user "q?"` at an interactive prompt over the wire.

This is the *live* path. It is not `cargo test` (see `scripts/golem-e2e.sh` for the scripted e2e, which
asserts the same surface non-interactively). Everything here runs against a **local, throwaway** Golem
server and a **version-matched** all-in-one binary built from `golem-stuff/golem`.

Scripts in this directory are the ones to use — **do not** re-derive these commands ad hoc:

| Script | Does |
|---|---|
| `./setup.sh` | key-gate → build → server on :9881 → `deploy --reset` → verify the agent's key → print the connect command |
| `./probe.sh '<cmd>'` | run one command non-interactively via `agent invoke eval` (for debugging, no TTY) |
| `./teardown.sh` | kill the server, wipe the data dir + `golem-temp` |

---

## What this even is

`golem agent stream` tails an agent's logs, one-way. A **patch in `golem-stuff/golem`**
(branch `clank-connect-patch`) adds a sibling command, **`golem agent shell`**: a **readline loop** —
read a line → invoke the agent's `eval` method → render `stdout`/`stderr`/exit → resolve any
human-in-the-loop pause → repeat. You get a shell prompt talking to a durable agent.

It is **not** clank-specific: `agent shell` works with any agent exposing `eval(string)`,
`answer_prompt(string)` and `abort_prompt()`, each returning
`{stdout, stderr, exit-code, pending-prompt}`. clank's agent happens to present exactly that surface.
`stream` is untouched and log-streams exactly as it always did.

**Why a sibling command, not `stream --interactive`** (which is what this patch did first): `stream`'s
contract is *"live stream its standard output, error and log channels"* — strictly one-way — and this
never streams those channels; it invokes the agent. The tell was that `--interactive` had to declare
`conflicts_with_all` against every other flag on `StreamArgs`. A flag incompatible with its entire host
command belongs somewhere else. The CLI already demonstrates the split: `repl-stream` is a hidden
*sibling* of `stream` serving the Bridge SDK REPL, rather than a mode of it.

## Prerequisites

- The clone at `golem-stuff/golem` (it is the dev SDK *and* the patched CLI).
- An `ANTHROPIC_API_KEY` — **only** needed for `ask`. Everything else works without one.

## Quick start

```bash
export ANTHROPIC_API_KEY=sk-ant-…      # in THIS shell; see "The key trap" below
cd golem-native-testing
./setup.sh                             # builds, deploys, prints the connect command
```

Then connect (the command `setup.sh` prints):

```bash
../golem-stuff/golem/target/debug/golem agent shell 'ClankAgent("demo")'
```

You should get:

```
Connecting to clank shell clank:agent/ClankAgent("demo") (Ctrl-D or `exit` to leave)

clank$
```

### What to try

```
echo hello from connect          # proves eval → render round-trip
pwd                              # the agent's own filesystem
ls /
echo persisted > /tmp/f && cat /tmp/f     # durable FS
prompt-user "your name?"         # → pauses; answer at the ⏸ prompt
ask "in one word, what is a shell"        # → permission pause → answer yes → real model reply
exit                             # or Ctrl-D
```

**`prompt-user`** and **`ask`** are the interesting ones — they exercise the pause/resolve loop:

```
clank$ prompt-user "your name?"
⏸  your name?
  answer (`:abort` to cancel)> Aditya
Aditya
```

### The second implementer: a shell into GREETER's sandbox

`GreeterAgent` (the trivial wRPC fixture) also implements the shell surface — via the `clank-embed`
crate, log-sink-only tier (no providers, no env vars). Same server, different agent, different
prompt label, **different filesystem** (one agent instance = one worker = one isolated VFS):

```bash
../golem-stuff/golem/target/debug/golem agent shell 'GreeterAgent("demo")'
```

```
greeter$ pwd
/
greeter$ echo mine > /tmp/proof && cat /tmp/proof     # lives in GREETER's VFS —
greeter$ ask hi                                       # honest: no model provider configured
```

Files you created in the clank session do not exist here, and vice versa. This is the proof that
`agent shell` is a contract (`eval`/`answer_prompt`/`abort_prompt` → eval-result), not a clank
feature — any agent embedding `clank-embed` (or hand-implementing the three methods) gets it.

When done: `./teardown.sh`.

---

## The traps (each of these cost real debugging time — read before you file a bug)

### 1. `--reset` is mandatory when the key changes

`setup.sh` uses `golem deploy --reset` deliberately. A plain `golem deploy` reports
**"no changes required"** and will **not** re-provision env into agents that already materialized. The
symptom is `ask` reporting *"model provider not configured"* forever, no matter how many times you
re-deploy with the key exported.

`--redeploy-agents` is **not** enough either — it recreates instances from the *currently deployed*
(possibly stale) config. `--reset` deletes the agents **and the environment** and deploys from scratch.

### 2. The key must be NON-EMPTY in the shell that runs `deploy`

`golem.yaml` passes it through with `ANTHROPIC_API_KEY: "{{ ANTHROPIC_API_KEY }}"`. Substitution reads
the **`golem deploy` process's** environment and is *strict*: a **missing** var **fails** the deploy
loudly, but an **empty-but-present** one **silently** lands as `len=0`. `setup.sh` hard-gates on this so
it cannot happen again.

### 3. `<masked-secret:HASH>` in the deploy diff proves NOTHING

The diff masks any env value whose **key name** matches `KEY|SECRET|TOKEN|PASS|…`. It is cosmetic. The
hash is `blake3(value)` — and `blake3("")` is a perfectly stable, non-empty hash. **A changed hash does
not mean a real value was stored.** Don't infer success from it; check the agent instead (`probe.sh`).

### 4. Do NOT debug env with `$VAR` inside the clank shell

clank's `$VAR` / `${#VAR}` expansion currently reads **empty even when the variable is present**. This
produced a completely false "the key is empty!" investigation. Use `env` — it shows the truth:

```bash
./probe.sh 'env | grep -c ANTHROPIC_API_KEY'   # ✅ reliable
./probe.sh 'echo $ANTHROPIC_API_KEY'           # ❌ lies (prints empty)
```
`ask` is unaffected — it reads the key from Rust (`std::env::var`), not through shell expansion.

### 5. A long `ask` is NOT a hang

`ask` runs a multi-turn agentic loop — several Anthropic round-trips, commonly **~30s**. The connect
client blocks on that single `eval` RPC and prints **nothing** until it returns. It looks frozen. It
isn't. **Wait.**

Worse: **Golem serializes invocations per agent instance.** While that `eval` runs, *every* other call
to the same agent — including `abort_prompt`, including `probe.sh` — **queues behind it**. So firing
probes at a "stuck" agent actively makes it worse, and a tear-down is almost never the right move. The
server log tells the truth:

```bash
grep 'elapsed_ms' /tmp/clank-server.log | tail   # a 32000+ ms eval = it ran and finished
```

### 6. Ctrl-C mid-`ask` used to wedge the next session

It no longer does (clank `4bd6fa2`), but know the shape: Ctrl-C dropped the client while the agent still
held a pending prompt; the next session's first command was rejected with *"a prompt-user question is
awaiting a response"* and — because the rejection carried no prompt — the client couldn't resolve it, so
typed answers went to `eval` and got rejected again, forever. The fix makes the rejection **re-surface**
the prompt, so a fresh session picks it up and can answer it. Worth re-testing after connect changes:

```
# session A:  ask "…"  → Ctrl-C at the permission prompt
# session B:  <any command>  → must render "⏸ <the question>", NOT loop on "answer it first"
```

### 7. Version skew

Use the binary from `golem-stuff/golem/target/debug/golem` (what `setup.sh` uses). A dev CLI against a
**release** server fails with `missing field accountEmail`. If `deploy` says *"failed to parse
WebAssembly module"*, suspect a stale `target/` → `cargo clean`.

### 8. The dev binary rewrites clank's tracked files

`golem build`/`deploy` auto-syncs coding-agent skills and **has rewritten `AGENTS.md`** (and once
`golem.yaml` + Cargo manifests). After any run:

```bash
git status                       # from the clank root
git checkout -- AGENTS.md        # if it got rewritten
```

---

## Reading results by hand

If you invoke the agent directly rather than through `probe.sh`, note the result JSON is a **positional
schema-value-tree** (field names live in the type graph, not the value) — so `.value.stdout` returns
nothing. The readable escape hatch is the top-level `"result"` string (the Rust `Debug` of the record):

```bash
GOLEM=../golem-stuff/golem/target/debug/golem
$GOLEM agent invoke -q --format json 'ClankAgent("demo")' eval '"echo hi"' \
  | grep '"agent.invoke"' \
  | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['result'])"
# → EvalResult { stdout: "hi\n", stderr: "", exit_code: 0, pending_prompt: None }
```

**Never** `grep -o '"result":"[^"]*"'` — it truncates at the first escaped quote and looks like an empty
result. That trap produced a bogus "the shell returns nothing" conclusion.

## Related

- `scripts/golem-e2e.sh` — the scripted, assert-y e2e (`--with-llm` gates the network assertions).
- `scripts/clank-repl.sh` — the bash reference implementation of this same readline loop.
- `scripts/golem-probe.sh` — ad-hoc prober.
- `DEV_SDK_CHANGES.md` — what changed to run clank on the dev SDK at all.
