# main-rc-1 — running the Golem agent shell

This is the **golem 1.6 track** (branch `main-rc-1`), checked out in this worktree
(`~/Desktop/clank-spike`). It builds against the dev golem-rust SDK in the golem clone. These steps
drive the durable Golem agent interactively via `golem agent shell`.

For the automated end-to-end assertions instead, see [the e2e section](#automated-e2e-alternative) at
the bottom — that spins up its own throwaway server and must NOT share the interactive cluster's port.

---

## Prereqs — in every terminal

Point at the dev golem binary and the local server, once per shell:

```bash
export GOLEM=~/Desktop/clank.sh/golem-stuff/golem/target/debug/golem
export GOLEM_BUILTIN_LOCAL_URL=http://localhost:9891
```

Optionally add the `GOLEM` line to `~/.zshrc` so it's always set.

---

## Terminal A — start the dev cluster

```bash
cd ~/Desktop/clank-spike
$GOLEM server run --router-port 9891 --custom-request-port 9016 --mcp-port 9017 --data-dir /tmp/clank-rc1-devdata
```

Leave it running.

> **Fresh data dir on purpose.** `/tmp/clank-rc1-devdata` is distinct from the released-demo's
> `/tmp/clank-demo-devdata`, so this cluster never inherits a stale component revision built from the
> `main` (1.5.x) line. If you're ever unsure which build a cluster holds, use a new data-dir name or
> wipe the old one.

---

## Terminal B — build & deploy the agent

```bash
cd ~/Desktop/clank-spike
export ANTHROPIC_API_KEY=<your key>        # needed at DEPLOY time for `ask` to work on the agent
$GOLEM -Y build
$GOLEM deploy --yes
git checkout -- AGENTS.md                  # the dev CLI rewrites this on every build/deploy
```

~20s.

> The dev CLI also sometimes rewrites `golem.yaml` (bumping `manifestVersion`). After deploy, run
> `git status` — if `golem.yaml` shows as modified, `git checkout -- golem.yaml`.

---

## Terminal C — drive the agent shell

```bash
cd ~/Desktop/clank-spike
$GOLEM agent shell 'ClankAgent("rc1-live1")'
```

The prompt renders `clank$`.

**Two rules that keep the session clean:**

1. **Use a fresh agent name every run** (`rc1-live1`, `rc1-live2`, …). An agent pins to the component
   revision it was created at, so a reused name silently runs old code. A new name = clean shell,
   and it's instant and free.
2. **Never type while `ask` is running.** There's no visible prompt during a long invocation, so the
   terminal queues your keystrokes in cooked mode and replays them into the next prompt — which makes
   output look reordered. The type-ahead drain mitigates this, but don't lean on it: wait for each
   answer to print.

First command each session should be cheap (`pwd`) — it absorbs the ~15–30s cold start of the 210MB
wasm.

### A few things to try

```sh
pwd
echo $((6*7))
ls /bin | wc -l
sudo curl -sI https://example.com
sudo ask "how many files are in /bin and what are the first three?"
```

