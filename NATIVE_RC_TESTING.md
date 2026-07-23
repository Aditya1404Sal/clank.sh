# main-rc-1 — running clank natively

This is the **golem 1.6 track** (branch `main-rc-1`), checked out in this worktree
(`~/Desktop/clank-spike`). These steps build and run the shell as a plain native binary — no Golem
server, no deploy, one static binary. Durability is the Golem perk; the native path is the product's
"works outside Golem" promise.

For the durable Golem agent path instead, see [RC_TESTING.md](RC_TESTING.md).

---

## 1. Build the native binary

```bash
cd ~/Desktop/clank-spike
cargo build -p clank-shell --bin clank
```

Produces `./target/debug/clank`.

---

## 2. Run it

```bash
cd ~/Desktop/clank-spike
mkdir -p ~/.clank/mcp ~/.clank/mcp-bin
export CLANK_MCP_ETC=~/.clank/mcp CLANK_MCP_BIN=~/.clank/mcp-bin
export ANTHROPIC_API_KEY=<your key>        # needed for `ask`
./target/debug/clank
```

The prompt renders `clank$`.

> **The `CLANK_MCP_*` exports are native-only.** MCP config defaults to `/etc/mcp`, which isn't
> writable on macOS. On the agent it just works — these overrides only matter natively. `$PATH` now
> honors them too, so MCP/grease command dirs resolve under the overridden locations.

---

## A few things to try

### A real shell

```sh
pwd
echo $((6*7))
ls /bin | wc -l
ls /bin | head -3
printf "c\nb\na\n" | sort | head -2
type curl                # curl is a shell builtin
type ask
```

### Real HTTP

```sh
sudo curl -sI https://example.com
sudo curl -s -o /dev/null -w 'status=%{http_code} size=%{size_download}' https://example.com
sudo curl -sL -o /dev/null -w 'final=%{url_effective}' http://github.com
```

The last line follows the redirect and prints `final=https://github.com/`.

### curl as a pipeline head

```sh
sudo curl -s https://example.com | grep -c '<'
```

curl/wget work as a top-level command or as the **first** stage of a pipeline (not mid-pipeline,
inside `$(...)`, `xargs`, or `eval`).

### Live MCP against a public server

```sh
sudo mcp add deepwiki https://mcp.deepwiki.com/mcp
mcp list
mcp tools deepwiki
```

> Tool *invocation* works natively and survives a restart — the tool list is cached in the config
> and reconstructed, and dispatch is a stateless `tools/call` to the server (no live session needed).
> `mcp reload` is only for picking up server-side changes or recovering a failed install.

### The transcript is first-class

```sh
echo alpha
echo beta
context show
context trim 1
context show
```

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

## Running the native tests

```bash
cd ~/Desktop/clank-spike
cargo test --workspace
```

> **Known flake, not a bug.** `cargo test --workspace` runs each crate's test binary in parallel, and
> there is one global process cwd shared across every Session. A cwd-sensitive conformance scenario
> (e.g. `pipeline-exit-codes`) can rarely collide with another crate's `cd`/`pwd` and fail once.
> `CWD_TEST_LOCK` serializes it within the conformance crate but cannot reach across crate boundaries.
> Re-run the suite — it passes in isolation:
>
> ```bash
> cargo test -p clank-conformance --test native
> ```
