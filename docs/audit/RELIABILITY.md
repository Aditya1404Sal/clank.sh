# RELIABILITY.md — how clank fails, and how you can tell

Audience: anyone who needs to answer *"can this thing break in a way that requires a restart, and how
would I know?"* — before trusting a durable agent with real work, or after something has already gone
wrong.

This is not a general reliability essay. It is a record of a specific audit: what was found, what was
fixed, what was deliberately not fixed and why, and — most importantly — **the mechanism that keeps
answering the question after this document goes stale**. That mechanism is [the resilience conformance
tier](#the-resilience-tier--the-part-that-outlives-this-document), and it is the part to read if you
read only one section.

---

## The governing fact

clank runs two ways from one `Session` core: a native binary and a **durable Golem WASM agent**. The
durable half changes what "a bug" means.

```console
$ rustc -Z unstable-options --print target-spec-json --target wasm32-wasip2 | jq '."panic-strategy"'
"abort"
```

**`wasm32-wasip2` does not unwind.** A panic aborts the component before any landing pad runs. Golem
then does the correct thing for a durable system — it retries the invocation — and because the trap is
deterministic it traps again, and the worker parks in `Failed`. There is no restart path, no
supervisor that clears the state, no "just run it again".

So on the agent:

| Failure | Native cost | Agent cost |
|---|---|---|
| `panic!` / index out of bounds | one command dies | **instance wedged, permanently** |
| stack overflow | process dies, shell restarts | **instance wedged** (an abort — not even catchable) |
| OOM / huge allocation | OS kills, shell restarts | **instance wedged** (fixed linear memory) |
| unbounded wait | one command hangs | **every queued invocation hangs** — Golem serializes per instance |

That last row is easy to miss. Golem processes one invocation at a time per agent instance, so a
single black-holing HTTP peer does not stall one command; it stalls everything behind it, forever, on
a worker that never restarts.

**And the driver is increasingly a model, not a person.** An LLM emits malformed args, unknown flags,
absurd values and unusual command shapes far more often than a human does, it cannot see the terminal,
and **the exit code is its only machine-readable signal**. "A weird input someone would have to try on
purpose" is not a useful category here — the model tries them by accident, constantly.

Every finding below is scored against that table, not against ordinary-program intuition.

---

## What was found and fixed

### 1. Four traps reachable from ordinary input

**T1 — the log-tail trimmer sliced at a raw byte offset.** `logging::bound_tail` computed
`cut = buf.len() - max_bytes` and then sliced `buf[cut..]`, which panics whenever a multi-byte
codepoint straddles the offset.

This was the P0, for three compounding reasons:

- Its **only** caller is the agent's log sink — it ran *only* on the target where a panic is fatal.
- Reachability was total: `eval_line` writes every command line verbatim into `shell.log`, and
  `Record::render` passes non-control characters through unchanged.
- So once the buffer passed 256 KiB, **any** non-ASCII byte landing on the cut — an em-dash, an
  ellipsis, an emoji, an accented path — wedged the instance. And the offset is a deterministic
  function of what you have logged, so it can be aligned deliberately.

Its two existing tests were ASCII-only, so the case could not have been caught. The fix scans raw
bytes for `\n`: a newline is ASCII and can never be a UTF-8 continuation byte, so any index derived
from one is a char boundary by construction. The added test reproduces the original panic exactly
(`byte index 5 is not a char boundary; it is inside 'é'`) and fails without the fix.

**T2 — the same class, on a guest-writable field.** `&sha256[..12]` in two places sliced a digest read
back from a `<etc>/<name>.toml` marker — a file the shell itself writes, therefore not guaranteed
ASCII hex. Replaced with a shared char-based `sha_prefix`; byte-identical output for a well-formed
digest.

**T3 — silently dropped arguments changed wRPC arity.** `encode_args` used
`filter_map(|(_n, v)| v.clone().to_element_value().ok())`, dropping any argument that failed to
encode. A wrong-arity wRPC call is a **fatal host trap**, and `order_agent_params` exists specifically
to prevent it — so this re-introduced the exact failure the check upstream of it had just ruled out.
Now the encode error propagates, naming the argument.

**T4 — `--schedule` overflowed on unvalidated LLM input.** Year/month/day parsed from a model-supplied
string and fed into civil-calendar arithmetic where `days * 86400` overflows `i64`. `golem.yaml`
builds the local environment with the `debug` component preset, where **overflow checks are on** — so
this was a hard panic on every dev deployment. All six fields are now range-checked before the
arithmetic.

> `parse_epoch_secs` moved from `clank-agent` into `clank_core::golem::agent` as part of this. Not
> tidying: `clank-agent` is a wasm-only cdylib with **no test target at all**, so the function was
> untestable where it lived. It arrives in core with the tests it never had.

### 2. Unbounded work reachable from the model

Each of these turns an operation with no ceiling into a refusal. All four bounds now live in
`clank_core::config::limits`.

| Bound | Was | Trigger |
|---|---|---|
| `MAX_PRINTF_WIDTH` (64 KiB) | width parsed as a bare `usize`, drove a pad string that long | `awk 'BEGIN{printf "%9999999999d", 1}'` — a ~10 GB allocation from one format string |
| `MAX_AWK_PARSE_DEPTH` (200) | `parse_expr → … → parse_primary → parse_expr`, no limit | deep parenthesisation → stack overflow → **abort**, which cannot be caught at all |
| `MAX_COLUMNS` (1000) | `$COLUMNS` parsed as an unbounded `usize`, then `0..cols` | `COLUMNS=18446744073709551615` → an effectively infinite layout loop |
| `MAX_MCP_SESSIONS` (64) | no cap, nothing reaping it | ordinary use on an instance that never restarts |

Two judgement calls worth naming. The printf width is **refused, not clamped** — clamping hands back
output nobody asked for, and silently-wrong output is worse than an error for a caller that cannot see
the terminal. And `$COLUMNS` out-of-range now reads as *unset*, falling back to 80, because
`agent shell` clients send a literal `export COLUMNS=<w>` line, so the value arriving is arbitrary
text rather than the `u16` the internal setter suggests.

### 3. Timeouts — the highest severity-per-effort finding in the audit

`whttp` **did** have sensible defaults (connect 30 s, total 5 min) — applied inside the **native**
`fetch_once` only. The wasm path stayed unbounded unless a caller passed `-m`/`-T`. The mitigation had
landed on the target that doesn't have the problem, and the comment beside it named the hazard as
*"a durable agent serializes invocations, it wedges every request queued behind it"*.

Worse, the four transports that bypass `whttp` entirely each built a client with **no timeout at
all**. All now resolve from `clank_core::config::net`:

| Transport | Bound |
|---|---|
| `ai/anthropic_native.rs`, `ai/llm_native.rs` | connect 30 s, LLM 5 min |
| `mcp/http_native.rs`, `golem/rest_native.rs` | connect 30 s, request 2 min |
| `clank-agent/mcp_http.rs` (wstd, durable) | connect 30 s, first-byte 2 min |
| `whttp` | defaults now resolve in the **shared** `fetch`, so both targets inherit them |

### 4. Response bodies were read to EOF with no cap

And `clank-agent`'s MCP transport had a `MAX_BODY` constant it consulted **after** `.to_vec()` — so it
bounded the value returned, never peak allocation. The protection the module doc advertised did not
protect memory. On wasm, with fixed linear memory, exceeding it is a trap.

`whttp` now carries `max_body` (64 MiB default, per-request overridable) and a `BodyTooLarge` error,
enforced twice: an advertised `Content-Length` over the cap is rejected **before any read**, and the
native path then **streams** through `chunk()` with a running bound, so a peer that omits or lies
about `Content-Length` still cannot push past it. Two tests, both against a real localhost server —
one for the advertised-length reject, one driving a server that sends no `Content-Length` and streams
8 MiB against a 256 KiB cap, which is precisely the case a post-hoc check cannot catch.

### 5. Exit-code honesty — lying to the driver

The exit code is the only machine-readable channel a script, an outer agent, or the LLM reading a tool
result has. Four commands returned success for work that had not succeeded.

| Command | Was | Now |
|---|---|---|
| `grease update` | stringified per-package results, returned 0 unconditionally — success even if every package failed | worst per-package code wins, plus an "N package(s) failed" line |
| `grease search` | dead registry → `"no packages match '<query>'"` at exit 0 | unreadable registries named on stderr; **no** registry readable ⇒ exit 4 without claiming absence |
| `ask` | exit 0 after burning `ASK_MAX_ITERATIONS` | exit 1 — the model never reached a final answer, so the work is incomplete (`--json` already returned 6; the plain path was the outlier) |
| `list_tools` | `Ok` with a **silently truncated** list past `MAX_TOOL_PAGES` | an error — a truncated capability set is not a successful listing |

The `grease search` one is the sharpest: a model searching an unreachable registry concluded the
package did not exist and abandoned the task. The shell was not merely unhelpful, it was actively
misleading in a way that changed the agent's behaviour.

`list_tools` compounded similarly — grease **persisted** the truncated surface into the package payload
and rebuilt it on every boot, so the model was permanently told a server had fewer tools than it does.

### 6. Half-installed packages were hidden rather than reported

`GreaseState::load` skipped anything it could not load — six `.ok()?` in ten lines. For a crash
mid-install that produced the worst available symptom: the marker survived on disk but the package was
invisible to `grease list`, so the obvious recovery — `grease remove <name>` — answered *"is not
installed"*, and the orphan stayed forever with no way out short of hand-editing the VFS.

Now `load_one` returns a reason, `GreaseState` keeps a `broken` set beside `packages`, `grease list`
shows `<name>  [broken]  <reason>` and exits non-zero, and `grease remove` cleans up the marker and
store dir saying *"(was a half-installed package)"*. Also: `persist_package` now refuses to write an
**empty** payload — `to_json` was `to_string_pretty(..).unwrap_or_default()`, so a serialization
failure wrote a zero-byte file that installed "successfully" and failed to parse on the next boot. The
success path was manufacturing broken packages.

> **One fix was attempted and reverted, and the reasoning is recorded at the load site so nobody
> re-attempts it:** re-verifying the payload against `marker.sha256` on load. That digest is of the
> artifact **as fetched from the registry**, which is what the install-time check verifies against the
> index — while the store holds `payload.to_json()`, a re-serialization with different bytes. The
> check failed every healthy install; the MCP boot-reconstruction test caught it. Detecting on-disk
> tampering needs a *second*, separately recorded digest of the persisted file.

### 7. Observability — you could not reconstruct what happened

Redaction was already well done and must not be regressed: a single `mask_values` call at the one
`logging::append` choke point. The gaps were about what never got written at all.

- **No grease install failure reached any log.** A sha256 mismatch, a missing signature, a failed
  RFC-6962 inclusion proof — all printed to the terminal and vanished. On the agent that matters more
  than it sounds: terminal output is gone the moment the invocation returns, so a **rejected package
  left no evidence anywhere**. Now a `grease op=… package=… outcome=ok|failed:N` record goes to
  `ops.log` at the dispatch choke point, making supply-chain rejections as auditable as destructive
  ops already were.
- **No PIDs on `shell.log` start/end lines**, so concurrent lines could not be correlated — while the
  module doc advertised "PID/PPID-addressable audit events".
- **Native `DefaultLogSink` had no rotation.** The agent's 256 KiB rolling tail had no native
  counterpart, so a long-lived native session grew `shell.log` forever. Both targets now bound at the
  same `MAX_LOG_BYTES`, from the same constant.
- **No timestamps — deliberate, and kept.** The clock is a replay-nondeterministic host call. Ordering
  survives; latency does not. This is a real limitation and it is documented rather than papered over.

### 8. Panic reporting — the net under everything else

The prior audit filed "no panic containment" and proposed wrapping the three component exports in
`catch_unwind`. **That would have been dead code**, which is why the `panic-strategy` output at the top
of this document is quoted rather than assumed: there is nothing for `catch_unwind` to catch.

But a panic **hook** does run, immediately before the abort. So clank cannot survive the panic — it can
say where it died and what it was doing. `runtime::panicreport` installs a hook that writes

```
panic at=<file:line> line=<command> message=<payload>
```

to `ops.log` through the installed `LogSink` — on the agent, the replay-safe whole-file rewrite, never
a raw append. The command line comes from a thread-local set for the duration of `eval_line` (the same
install/restore-on-drop shape as `sysprompt`/`proctable`) and passes through the same `log_safe_line`
redaction as the shell events, so a secret-bearing flag never leaks into a crash report.

Two supporting hardenings, both about not destroying the report:

- `logging::append` uses `try_borrow` on the sink slot. The hook can fire at any point, including while
  `install`/`Drop` holds the mutable borrow — a panicking borrow *there* is a panic inside a panic,
  which aborts instantly and loses everything.
- The three `self.session.as_mut().unwrap()` calls in the exports became `let-else`. The invariant
  holds and was documented, but they sit *inside* the export, where being wrong costs the instance
  rather than one command. Degrading to a clean exit-1 is free.

---

## The panic surface is otherwise disciplined

Worth stating plainly, because a list of fixes reads worse than the code is. Counted by
[`dev-tools/panic-surface.py`](../../dev-tools/panic-surface.py) over every `src/**/*.rs` in
`crates/` and `utilities/`, with comment lines and `#[cfg(…test…)]` modules excluded:

| | Count |
|---|---|
| `panic!` | **0** |
| `todo!` / `unimplemented!` | **0** |
| `.unwrap()` | **1** (`tools/awk.rs:809`) |
| `.expect(…)` | **8** |
| `unreachable!` | **5** |
| **Total panic primitives in shipped code** | **14** |

Of the 8 `expect`s, **2 are in `main.rs` files** (`wcurl`, `waget`) where the process is about to exit
anyway, and the rest sit behind invariants established a few lines earlier. The 5 `unreachable!`s are
match arms over enums the surrounding code has already narrowed.

Also verified:

- **52 `Mutex::lock()` sites, 52 poison-safe.** Every one is
  `.unwrap_or_else(PoisonError::into_inner)` — zero poisoning panics, no exceptions.
- **10 `unsafe` blocks, all documented.** 8 are `libc` FFI (`signal`, `dup`/`dup2`, `fcntl`, `read`,
  `__wasilibc_fd_renumber`) with no memory-safety precondition; 2 are a test-only no-op-waker
  executor. Each carries a SAFETY comment and each was reviewed and found sound.
- Every `.remove(idx)` derives its index from a `position()` in the same statement.
- Every narrowing `as u8` is `.clamp`ed first.

This was a targeted fix list, not a rewrite. The traps above are notable precisely because they were
exceptions to an otherwise careful codebase.

> Re-run `python3 dev-tools/panic-surface.py` before trusting these numbers. It prints every site,
> so a changed total can be diffed rather than merely noticed — and the whole point of the section
> below is that snapshots go stale.

---

## The resilience tier — the part that outlives this document

Everything above is a snapshot. This is the mechanism.

`crates/clank-conformance` runs **one `.clank` corpus against two targets**: the native `Session` and a
**really-deployed Golem agent**. That existing machinery is what makes the question answerable, because
a hostile-input scenario run against the golem target *is* a direct empirical answer to "can an LLM
wedge the durable agent?" — a number in CI instead of a belief.

Two scenarios were added:

- **`resilience-malformed-args.clank`** — missing and excess args, non-numeric where numeric, unknown
  flags, empty strings, malformed redirects, deep quoting.
- **`resilience-hostile-input.clank`** — input that *parses fine* and asks for something ruinous: the
  absurd printf width, the deep expression nesting, the `usize::MAX` terminal width, multibyte text
  through every path that truncates or slices.

**The contract every step asserts is the same three things:**

1. **nonzero exit** — the driver is told it failed;
2. **legible stderr** — a human or model can tell *what* failed (`err~ field width exceeds`);
3. **the session survives** — the next line still runs (`run echo still-alive-3`).

That third assertion is the one that matters here, and it is why these are conformance scenarios
rather than unit tests. A unit test proves a function returns `Err`. Only running the next command
against the same live instance proves the instance is still there.

The corpus is written to fail loudly if a bound is ever removed:

> *"Each step here corresponds to a bound that exists in `clank_core::config::limits`; if one of these
> ever passes with exit 0, a bound has been removed."*

**To answer "how fault-tolerant is clank today?", run this — don't read this file:**

```bash
cargo test -p clank-conformance --test native     # verified today: 36 passed / 0 failed
scripts/conformance-golem.sh                       # the durable target; needs a live deploy
```

---

## What is NOT fixed

Stated plainly, because a reliability document that only lists wins is not useful.

**A grease `script` package that loops forever still parks the instance permanently.**
`while true; do :; done` from a registry-supplied script has no timeout and no gate. This is not an
oversight — **wasip2 has no preemption**: no threads, no interrupting timer, nothing to implement a
timeout *with*. Fixing it needs either a host-side execution deadline or an interpreter that counts
its own steps. Until then, the mitigation is entirely at the trust boundary: only install scripts from
registries you trust.

**The durable LLM call has no timeout.** The Golem host binding's `Config` exposes no timeout knob. The
obvious workaround — a guest-side wall-clock budget in the ask loop — was **deliberately not taken**,
because reading the clock on the agent is exactly the replay-nondeterministic host call this codebase
avoids on principle (`logging::Record` omits timestamps for that reason). Trading a durability bug for
a timeout is a bad swap. The path is bounded by `ASK_MAX_ITERATIONS` turns times the host's own
deadline, and the limitation is now documented at the call site rather than being invisible.

**The wasm body cap is a pre-check plus a post-read check, not streaming.** `wstd`'s
`Body::contents()` is all-or-nothing. Closing the gap means taking on `http-body-util` pinned to
`wstd`'s `http_body` version — not something to slip into a reliability fix. The asymmetry is
documented at both sites rather than left for a reader to assume parity.

**There is no retry or backoff anywhere in the tree.** A durable agent is exactly where that belongs.
The precondition now exists — the structured error types carry `is_retryable()`, and `golem::Error`
answers it correctly (only `Unreachable`; a remote fault replays identically, so retrying re-runs the
same trap) — but nothing consumes it yet.

**`MCP_MOUNT` is env-overridable through grease's accessor, but `runtime::mcpfs` matches the literal
`/mnt/mcp` prefix.** Under an override the virtual listing and the real files part company. Harmless
today because the override exists only as a test seam, and documented in `config::vfs` where the two
constants now sit next to each other.

**Timestamps remain absent from all logs**, by the replay-determinism argument above. You can
reconstruct order; you cannot reconstruct latency.

---

## If an agent is wedged right now

1. **Read `/var/log/ops.log`.** If it ended in a panic, the hook wrote `panic at=<file:line>
   line=<command> message=<payload>` before the abort — that gives you the exact source location and
   the command that caused it.
2. **Read `/var/log/shell.log`** for the command sequence that led there.
3. **If `ops.log` is silent, suspect a non-panic wedge:** a stack overflow (aborts with no hook), an
   OOM, or an unbounded wait. An unbounded wait is now the least likely of the three — every transport
   clank owns is bounded — which leaves the two known-unbounded paths above: a `script` package
   looping, or the durable LLM call.
4. **Reproduce it in the conformance corpus** before fixing it. A resilience finding that isn't in the
   corpus will be re-found later.
