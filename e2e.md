# e2e.md — running every test in clank.sh

The single, self-contained guide to testing clank end to end: the **native** binary, the durable
**Golem agent**, and the **conformance suite** — across **both** tracks (`main` and `main-rc-1`) and
both execution targets (native + wasm). If you read one testing doc, read this one. The others are
focused slices of what's collected here:

- [NATIVE_RC_TESTING.md](NATIVE_RC_TESTING.md) — bring up + poke the native binary (rc-1)
- [RC_TESTING.md](RC_TESTING.md) — bring up + poke the durable agent (rc-1)
- [PREREQUISITES.md](PREREQUISITES.md) — the background servers/registry the agent path needs
- [GREASE_REGISTRY.md](GREASE_REGISTRY.md) — authoring signed grease packages
- [docs/TESTING.md](docs/TESTING.md) — the two one-liner script entry points

Everything below was verified green on 2026-07-23.

---

## 0. Two tracks, two worktrees — and the one thing that trips everyone

clank lives in **two git worktrees of the same repo**, one per track:

| Track | Worktree | What it is | golem-rust SDK | Which `golem` CLI to use |
|-------|----------|-----------|----------------|--------------------------|
| **`main`** | `~/Desktop/clank.sh` | the released **1.5.x** line | published `2.1.0` (crates.io) | the released **`golem` 1.5.1** already on your `PATH` |
| **`main-rc-1`** | `~/Desktop/clank-spike` | the **golem 1.6** track / main driver | the **unreleased dev SDK** vendored at `~/Desktop/clank.sh/golem-stuff/golem` (absolute path-deps) | the **dev `golem` binary** at `~/Desktop/clank.sh/golem-stuff/golem/target/debug/golem` |

> **The #1 mistake on `main-rc-1`: driving it with the 1.5.1 `golem` on your `PATH`.** rc-1's agent
> component is built against the 1.6 dev SDK; deploying it with the 1.5.1 CLI/server can fail with a
> version/parse mismatch. **On rc-1 you must use the dev binary.** Two of the scripts
> (`golem-e2e.sh`, `clank-repl.sh`) call *bare* `golem`, so the reliable move is to put the dev
> binary first on `PATH`; `conformance-golem.sh` additionally honours `GOLEM_BIN=`.

**Native tests need no golem at all** — they run the pure Rust engine on the host. Only the agent /
component / conformance-golem layers touch the CLI.

Crate layout (both trees, after the R1 restructure — old docs said `clank-shell`, now renamed):

```
crates/clank-core          # the engine: Session, transcript, all builtins (pure rlib)
crates/clank-cli           # the native `clank` binary (thin wrapper over clank-core)
crates/clank-agent         # the Golem component (cdylib, wasm32-wasip2 only)
crates/clank-conformance    # the .clank scenario corpus + native/golem test harnesses
crates/clank-embed         # rc-1 ONLY: the shell surface as an embeddable library
utilities/{whttp,wcurl,waget}   # the HTTP tools
dev-tools/grease-tool       # the grease package builder
fixtures/greeter-agent      # e2e fixture: the 2nd implementer of the embed surface (wasm only)
```

---

## 1. Prerequisites (once)

```bash
# Rust toolchain is pinned by rust-toolchain.toml — rustup honours it automatically.
rustup target add wasm32-wasip2          # required for the component/agent builds
cargo --version                          # any recent stable is fine for the native path

# For the Golem agent + conformance-golem layers:
#   • jq        (golem-e2e.sh parses invoke JSON)
#   • python3   (grease registry HTTP serving; --with-grease generates+serves it for you)
command -v jq python3

# main track — the released CLI:
golem --version                          # expect: golem 1.5.1

# main-rc-1 track — the dev SDK CLI. Point PATH at it FIRST, so bare `golem` = dev binary:
export DEVGOLEM=~/Desktop/clank.sh/golem-stuff/golem/target/debug
export PATH="$DEVGOLEM:$PATH"
golem --version                          # expect: a git hash (e.g. 43605c8df…), NOT 1.5.1
```

> If the dev binary is missing, build it once inside the vendored clone:
> `cd ~/Desktop/clank.sh/golem-stuff/golem && cargo build -p golem-cli` (see that repo's own README).

`ANTHROPIC_API_KEY` is only needed for the **live-model** assertions (`--with-llm`, and `ask` in
interactive sessions). Everything else runs without a key.

---

## 2. Quick reference (copy-paste per track)

### `main-rc-1` (worktree `~/Desktop/clank-spike`)

```bash
cd ~/Desktop/clank-spike
export DEVGOLEM=~/Desktop/clank.sh/golem-stuff/golem/target/debug
export PATH="$DEVGOLEM:$PATH"                                   # dev golem for the agent layers

# 1 · native unit/integration tests (the engine)         → 599 passed; 0 failed; 44 ignored
cargo test --workspace -- --test-threads=1

# 2 · component builds compile for wasm                   → Finished (exit 0)
cargo build -p clank-agent -p greeter-agent --target wasm32-wasip2

# 3 · native conformance tier alone                       → 34 passed; 4 ignored
cargo test -p clank-conformance --test native

# 4 · golem conformance tier (throwaway server, 38 scenarios)
GOLEM_BIN="$DEVGOLEM/golem" scripts/conformance-golem.sh --takeover

# 5 · full golem agent e2e (deploy + README command asserts)
scripts/golem-e2e.sh --takeover                                # add --with-llm --with-grease --with-mcp for the full surface

# 6 · lints (should be ZERO warnings)
cargo clippy --workspace --all-targets
```

### `main` (worktree `~/Desktop/clank.sh`)

```bash
cd ~/Desktop/clank.sh
# golem = the 1.5.1 on PATH; no DEVGOLEM, no golem clone needed.

cargo test --workspace -- --test-threads=1                     # → 589 passed; 0 failed; 42 ignored
cargo build -p clank-agent -p greeter-agent --target wasm32-wasip2
cargo test -p clank-conformance --test native                  # → 34 passed; 4 ignored
scripts/conformance-golem.sh --takeover                         # golem tier (uses PATH golem)
scripts/golem-e2e.sh --takeover                                 # full agent e2e
cargo clippy --workspace --all-targets
```

---

## 3. Layer 1 — native tests (the engine)  ·  **start here**

This is the fast, deterministic core: `clank-core`'s unit + integration tests, the utilities, the
grease-tool, `clank-embed` (rc-1), and the conformance harness's own unit tests. No server, no
network, no key.

```bash
cargo test --workspace -- --test-threads=1
```

Expected (as of 2026-07-23):

| Track | passed | failed | ignored |
|-------|-------:|-------:|--------:|
| `main-rc-1` | **599** | 0 | 44 |
| `main` | **589** | 0 | 42 |

- The **`--test-threads=1` is load-bearing** — see [§8, the SIGPIPE flake](#8-the-sigpipe-flake-why---test-threads1). Plain `cargo test --workspace` intermittently dies with exit 101 / `SIGPIPE`. It is **not** a product bug.
- `clank-agent` and `greeter-agent` are in `--workspace` but are wasm-only cdylibs; natively they link a test harness with **0 tests** (they don't fail the native run, they just contribute nothing).
- The **44/42 ignored** are the golem-tier conformance scenarios (run separately in Layer 3) plus a few platform/network-gated unit tests.
- Redirect to a file, never a pipe: `cargo test … > /tmp/t.log 2>&1`. Piping the harness's stdout into another process can itself trigger the same SIGPIPE in the fd-swap tests.

Narrower slices while iterating:

```bash
cargo test -p clank-core                       # the bulk of the engine tests
cargo test -p clank-core -- --test-threads=1   # if the SIGPIPE flake bites a single crate
cargo test -p clank-conformance --test native  # native conformance tier only (34 passed; 4 ignored)
cargo test -p whttp -p wcurl -p waget          # the HTTP utilities
```

---

## 4. Layer 2 — the wasm component build

The agent (`clank-agent`) and the e2e fixture (`greeter-agent`) only link for `wasm32-wasip2`. A
plain compile check:

```bash
cargo build -p clank-agent -p greeter-agent --target wasm32-wasip2
```

Should finish clean (exit 0). This is a *compile* check; the full component build + WIT-world
packaging happens inside `golem -Y build` (Layer 4). On `main-rc-1` this pulls the absolute
golem-rust path-deps from the vendored dev SDK.

> If you hit `failed to parse WebAssembly module` at deploy time, it's almost always a **stale
> `target/`** from a different SDK line, not a toolchain regression — `cargo clean -p clank-agent
> -p clank-core --target wasm32-wasip2` and rebuild.

The native `clank` binary (for interactive use, Layer 6):

```bash
cargo build -p clank-cli --bin clank          # → ./target/debug/clank   (or just `cargo build`)
```

> Historical note: before the R1 restructure this was `cargo build -p clank-shell --bin clank`.
> `clank-shell` was renamed `clank-core` (now a pure library); the binary moved to `clank-cli`. The
> old command errors with *"package ID specification `clank-shell` did not match any packages."*

---

## 5. Layer 3 — the conformance suite

One `.clank` scenario corpus (`crates/clank-conformance/scenarios/*.clank`), run against **two
targets**: the native `Session` and a live Golem agent. Same assertions, both surfaces — this is how
we prove native/agent parity.

### Native tier — no server

```bash
cargo test -p clank-conformance --test native      # → 34 passed; 4 ignored
```

The 4 ignored are scenarios tagged for the golem target only (e.g. fresh-agent filesystem
bootstrap) or gated on network.

### Golem tier — throwaway server, one fresh agent per scenario

`scripts/conformance-golem.sh` stands up a local server on **port 9881**, deploys clank, and runs
every scenario through `golem agent invoke` (each scenario gets its own fresh, isolated agent
instance). The 38 scenarios that show as `ignored` in Layer 1 run **here**.

```bash
# main-rc-1 — pass the dev binary explicitly (this script honours GOLEM_BIN):
GOLEM_BIN=~/Desktop/clank.sh/golem-stuff/golem/target/debug/golem \
  scripts/conformance-golem.sh --takeover

# main — uses the PATH golem:
scripts/conformance-golem.sh --takeover
```

Expected: **37 passed; 0 failed; 1 ignored** (`curl-pipeline` is network-gated). Takes ~1 min — each
scenario deploys work to its own fresh agent.

Flags: `--takeover` (kill any server already on 9881), `--keep` (leave it up for post-mortems:
`golem agent list`), `-- <libtest args>` (e.g. `-- pipelines` for one scenario). Env: `GOLEM_BIN`,
`JOBS` (default 1 → `--test-threads`), `CLANK_CONFORMANCE_STEP_TIMEOUT_SECS` (default 60).

> **`main-rc-1` needs the dev SDK's invoke shape.** The dev golem CLI emits `resultJson` (camelCase,
> positional value-tree); the released CLI emits `result_json` (named fields). The harness's
> `decode_invoke` handles **both** — but if you see every scenario fail with *"no
> `result_json`/`resultJson` document … is clank deployed?"*, you're on a build predating that fix.

> Under the hood the test binary is gated by `CLANK_CONFORMANCE_GOLEM=1` (the script sets it). Run
> bare `cargo test -p clank-conformance --test golem` and the 38 trials report `ignored`.

---

## 6. Layer 4 — the full Golem agent e2e

`scripts/golem-e2e.sh` is the end-to-end smoke test: it builds + deploys the `clank:agent`
component to a **throwaway** server on **port 9881**, invokes `eval` for the README's
file-management / shell / HTTP command set, asserts each result, then tears everything down.

```bash
# main-rc-1 — bare `golem` inside the script must resolve to the dev binary, so prepend PATH:
export PATH=~/Desktop/clank.sh/golem-stuff/golem/target/debug:$PATH
scripts/golem-e2e.sh --takeover

# main — the PATH golem (1.5.1) is correct:
scripts/golem-e2e.sh --takeover
```

Opt-in surfaces (compose freely):

| Flag | Adds | Needs |
|------|------|-------|
| `--with-llm` | live-model assertions (`ask` runs the real agentic loop) | `ANTHROPIC_API_KEY` exported — **real Anthropic calls, costs credits** |
| `--with-grease` | builds + serves a signed, transparency-logged grease registry and runs the full install surface (prompt/script/skill/mcp/agent) | nothing (the script generates + serves it) |
| `--with-mcp` | live MCP assertions against a public no-auth server (DeepWiki) | network |
| `--keep` | leaves the server + data dir up afterwards | — |
| `--takeover` | frees port 9881 if something's already on it | — |

The maximal run (real key required):

```bash
export ANTHROPIC_API_KEY=<your key>
scripts/golem-e2e.sh --takeover --with-llm --with-grease --with-mcp
```

> **Cost guard:** the `ask` / model assertions fire only when **both** a real key is present **and**
> `--with-llm` is passed. A plain `--takeover` run makes zero paid calls (it still verifies clean
> exit-4 degradation when no key is configured). Default model is the cheapest (`haiku`).

> **Ports:** this script and `conformance-golem.sh` both use **9881** (throwaway). The *interactive*
> cluster in [RC_TESTING.md](RC_TESTING.md) uses **9891** on purpose — never point both at the same
> port, and give the interactive cluster a fresh `--data-dir` so it can't inherit a stale revision.

---

## 7. Layer 5 — lints (part of a clean bill of health)

Both trees are at **zero** clippy warnings under the full audit lint set (`[workspace.lints]` +
`clippy.toml`). Keep them there:

```bash
cargo clippy --workspace --all-targets         # native — expect zero warnings
cargo clippy -p clank-agent --target wasm32-wasip2   # the wasm crate lints on its real target
cargo fmt --check
```

---

## 8. The SIGPIPE flake — why `--test-threads=1`

Several tests exercise **native pipelines and fd-swaps** (`curl | grep`, `pipeline-exit-codes`, the
redirect / `$(...)` / here-doc tests). Brush — the shell core — resets `SIGPIPE` to `SIG_DFL` for
POSIX semantics (so `yes | head` terminates correctly). Consequently a write to a pipe whose reader
has already closed **kills the whole test process with signal 13** (`SIGPIPE: write on a pipe with
no one to read`, cargo reports exit 101) instead of returning `EPIPE`.

Under libtest's default thread-pool these tests race each other, so plain `cargo test --workspace`
fails **intermittently** — and it reproduces even for a single crate (`cargo test -p clank-core`),
which proves it's an **intra-binary** thread race, not the cross-crate cwd collision older notes
described. Serializing with `--test-threads=1` removes the race entirely and is **deterministically
green** (verified across repeated runs: rc-1 599/0/44, main 589/0/42).

This is a **test-harness artifact, not a product bug** — the same pipelines work correctly in the
running shell, where each command's producer completes and drops its writer before the consumer is
dropped. If you see exit 101 with `signal: 13`, you forgot `--test-threads=1` (or you piped the
harness's stdout into another process). Re-run serialized.

---

## 9. Interactive smoke tests (drive it by hand)

Not automated, but the fastest way to feel the two targets. Full walkthroughs live in the focused
docs; the essentials:

### Native binary

```bash
cd ~/Desktop/clank-spike          # or ~/Desktop/clank.sh
cargo build -p clank-cli --bin clank
mkdir -p ~/.clank/mcp ~/.clank/mcp-bin
export CLANK_MCP_ETC=~/.clank/mcp CLANK_MCP_BIN=~/.clank/mcp-bin   # native-only: /etc isn't writable on macOS
./target/debug/clank
```

Then: `pwd`, `echo $((6*7))`, `ls /bin | wc -l`, `sudo curl -sI https://example.com`,
`sudo curl -s https://example.com | grep -c '<'`. Full script: [NATIVE_RC_TESTING.md](NATIVE_RC_TESTING.md).

### Durable Golem agent (interactive)

Uses the dev golem + a persistent cluster on **9891** (distinct from the e2e's 9881). Full flow:
[RC_TESTING.md](RC_TESTING.md) (Terminal A server, B build+deploy, C `golem agent shell`). Rules:
fresh agent name each run, never type while `ask` runs, first command cheap (`pwd`) to absorb the
~15–30s cold start.

### Scripted REPL against a deployed agent

`scripts/clank-repl.sh` sends each typed line to a durable agent's `eval` and prints the result;
state persists across lines and restarts (same `--name` resumes the same instance). It transparently
drives clank's confirmation pauses.

```bash
# main-rc-1 (dev golem on PATH); --deploy stands up its own throwaway server + deploys first:
export PATH=~/Desktop/clank.sh/golem-stuff/golem/target/debug:$PATH
scripts/clank-repl.sh --deploy --takeover           # `exit` / `:q` / Ctrl-D to leave

# non-interactive (scriptable): pipe commands in
echo 'ls /bin | wc -l' | scripts/clank-repl.sh --deploy --takeover
```

---

## 10. Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `package ID specification 'clank-shell' did not match any packages` | doc/command predates the R1 rename | use `clank-core` (library) / `clank-cli` (binary) |
| native tests exit 101, `signal: 13 SIGPIPE` | the intra-binary pipeline race | add `-- --test-threads=1`; don't pipe the harness stdout |
| `golem deploy` → `failed to parse WebAssembly module` | stale `target/` from a different SDK line | `cargo clean -p clank-agent -p clank-core --target wasm32-wasip2`, rebuild |
| rc-1 deploy fails / weird version errors | using the 1.5.1 `golem` on `PATH` against a 1.6 component | put the dev binary first on `PATH` (or `GOLEM_BIN=` for `conformance-golem.sh`) |
| `A golem server is already on port 9881` | a previous run's server is still up | re-run with `--takeover` |
| `golem.yaml` / `AGENTS.md` show as modified after a build | the dev CLI rewrites them on every build/deploy | `git checkout -- golem.yaml AGENTS.md` |
| `ask` returns exit 4 / "not configured" | no `ANTHROPIC_API_KEY` (or no `--with-llm`) | export the key; pass `--with-llm` to the e2e script |
