# DEV_SDK_CHANGES.md — what `spike/dev-golem-sdk` changed to run clank on the dev Golem SDK

Audience: a maintainer who needs to know exactly what moving clank.sh off the pinned crates.io
`golem-rust 2.1.0` and onto the **unreleased dev SDK** cost, and why. Every entry below was verified
against the tree on branch `spike/dev-golem-sdk` (HEAD `db9a820`) versus `main` (`4586d81`). `main` is
fully contained in the spike — it is a linear +4 commits.

This is a **throwaway branch**. It exists to answer "what breaks?", not to merge. Sibling doc:
[WASM_CHANGES.md](WASM_CHANGES.md) (clank's third-party forks for `wasm32-wasip2`) — **§6 below
falsifies one of its opening claims while this branch is checked out.**

The changes fall into **four buckets**, and only one of them is a real loss.

---

## 1. Version anchors — what "1.6.0" and "2.1.0" actually refer to

There are five independent version axes here and they do not move together. Conflating them is the
main way to get confused, so:

| Axis | `main` | `spike/dev-golem-sdk` |
|---|---|---|
| `golem-rust` **dep declaration** | `version = "2.1.0"` (crates.io) — `crates/clank-agent/Cargo.toml:16` | `path = "../../golem-stuff/golem/sdks/rust/golem-rust"` — `crates/clank-agent/Cargo.toml:19` |
| `golem-rust` **resolved version** | `2.1.0` (registry + checksum) | **`0.0.0`** (path dep — no source, no checksum) |
| `golem:agent` **WIT world** | `@1.5.0` | **`@2.0.0`** (`golem-stuff/golem/sdks/rust/golem-rust/wit/deps/golem-agent/host.wit:1`) |
| `golem.yaml` `manifestVersion` | `1.5.0` | **`1.6.0`** (`golem.yaml:5`) |
| `golem.yaml` `$schema` URL | `…/app/golem/1.5.0/…` | **`…/app/golem/1.6.0-dev.6/…`** (`golem.yaml:1`) |

**The dev SDK has no meaningful version.** It is literally `version = "0.0.0"`
(`golem-stuff/golem/sdks/rust/golem-rust/Cargo.toml:8`) — an unpublished workspace crate. So "the
1.6.0 SDK" is a convenient fiction. The only honest identifiers are:

- **the clone's git SHA** — `golem-stuff/golem` is currently `43605c8df` on branch
  `clank-connect-patch`, stacked on upstream `e2c0fe298`; and
- **the WIT world version** — `golem:agent@2.0.0`.

Cite those, not a semver.

**The manifest/schema asymmetry is correct, not a typo.** `manifestVersion: 1.6.0` (a release string)
alongside `$schema=…/1.6.0-dev.6/…` (a prerelease) looks wrong and will be flagged in review. It isn't
— the CLI genuinely holds two different constants:
`golem-stuff/golem/cli/golem-cli/src/versions.rs:20` (`pub const MANIFEST: &str = "1.6.0";`) and
`:25` (`"1.6.0-dev.6"`). The dev binary wrote both itself.

### The four commits

| SHA | What |
|---|---|
| `b5b8186` | shell builds + runs on the dev SDK (the dep wall, the stubs, `golem_cluster`, greeter drop) |
| `bf68d48` | `ask` un-stubbed via a direct `wstd` Anthropic call (§4) |
| `4bd6fa2` | pending-prompt re-surface — **not SDK-related** (§5) |
| `db9a820` | path-dep repoint after the clone moved (see the note at the end of §2) |

---

## 2. Bucket 1 — forced by the SDK's module reorganization (~15 lines, mechanical)

**WHAT:** three call sites and one field access in `crates/clank-agent/src/golem_cluster.rs`.

**WHERE / the exact moves:**

| `main` | spike | spike line |
|---|---|---|
| `golem_rust::bindings::golem::api::host::get_self_metadata()` | `golem_rust::get_self_metadata()` | `29`, `42` |
| `golem_rust::bindings::golem::api::host::fork()` | `golem_rust::fork()` | `89` |
| `md.agent_id` | `md.agent_id.agent_id` | `32`, `48` |

**WHY:** the dev SDK made the host bindings private. `golem-stuff/golem/sdks/rust/golem-rust/src/lib.rs:84`
still has `pub mod bindings`, but `:89` re-exports `host` as **`pub(crate)`** —
so `golem_rust::bindings::golem::api::host::*` is unreachable from clank. The dev SDK adds hand-written
crate-root wrappers instead: `pub fn get_self_metadata()` at `lib.rs:979` and `pub fn fork()` at
`lib.rs:1007`.

`AgentMetadata` is now a hand-written schema-side struct whose `agent_id` field is an `AgentId`
**struct** (no `Display`) carrying the instance-name string — hence `.agent_id.agent_id` rather than a
formatter change.

> **⚠ One unresolved discrepancy, stated rather than papered over.** clank's own module doc on `main`
> (`golem_cluster.rs:7-10`) asserts *"golem-rust **2.1.0** … has no crate-root `get_self_metadata`/`fork`
> wrapper … Only `ForkResult` is re-exported at the crate root."* A later review claimed the opposite for
> `fork` (that 2.1.0 already re-exported it at the root, making only the *qualified path* the breakage).
> **This could not be re-verified for this doc**: the spike dropped the dep, so `golem-rust 2.1.0` is no
> longer in the local registry cache, and crates.io is unreachable from the build sandbox. The in-repo
> claim above was written when 2.1.0 was the live dependency. Treat the `fork` line as "the qualified
> path definitely broke; whether a root re-export already existed is unconfirmed."

**ACTIVE:** Yes. This is the whole cost of the SDK reorg — about 15 lines. Everything else in
`golem_cluster.rs` (the "honest partial" errors for operations with no clean host primitive) is untouched.

*Note on `db9a820`:* the clone briefly lived outside clank at `~/Desktop/golemcloud/golem` and has since
moved back to `golem-stuff/golem`. That move was motivated by a fear that clank's `/golem-stuff/`
gitignore would lose the connect patch — **it wouldn't**: the clone is a nested git repo with its own
remotes, and gitignored ≠ at-risk. Committed work there lives in its own git and is pushed to the fork.

---

## 3. Bucket 2 — forced by the value-model break (**rewritten; no capability lost**)

**WHAT:** `crates/clank-agent/src/agent_invoker.rs` was **rewritten** onto the new value model. It was
briefly stubbed (143 → 32 lines) while the spike answered "does the shell run?"; it is now real again,
and **live-verified against a second agent in the cluster**.

**WHY:** `golem:agent@2.0.0` **deleted the `data-value` / `element-value` model** the invoker
marshalled against, replacing it with `schema-value-tree` / `SchemaValue`. This is visible in the WIT:
`golem-stuff/golem/sdks/rust/golem-rust/wit/deps/golem-agent/host.wit` still has the `wasm-rpc`
resource, but every signature now takes `schema-value-tree` where 1.5.0 took `data-value` — and
`invoke-and-await` additionally gained an `option<>` in its success type. `common.wit`'s import set has
no `data-value`/`element-value` anywhere.

So it was a rewrite, not a port. What each piece became:

| Piece | 1.5.0 | dev SDK (2.0.0) |
|---|---|---|
| `encode_args` | `Schema::to_element_value` → `DataValue::Tuple` | positional `SchemaValue::Record` → `encode_schema_value` |
| `build_client` | `WasmRpc::new(&ty, &ctor, ph, &[])` | tree + config **by value** (a value tree is affine — it can carry owned handles) |
| `render_result` | `DataValue::Tuple` len 1 | `Option<SchemaValueTree>` — **`None` is a `unit` return, not an error** |
| `parse_phantom` | `golem_wasm::golem_core_1_5_x::types::Uuid` | `schema::wit::wire::Uuid` — same `high_bits`/`low_bits`, body verbatim |
| `parse_epoch_secs` | Hinnant `days_from_civil` (~30 lines, no `chrono`) | **unchanged** — touches no golem API |
| `cancel` | honest `Err(..)` | **still an honest `Err`** — see below |

**Three findings worth keeping:**

1. **The type paths are decided by a `with:` mapping, not by guessing.** `golem-rust` generates *two*
   bindgen worlds — `raw_bindings`/`bindings::` (`lib.rs:66-115`) and the feature-gated `golem_agentic`
   (`lib.rs:201-242`) — so there are **two distinct `WasmRpc` Rust types**, not interchangeable. But
   *both* map `"golem:core/types@2.0.0": golem_schema::schema::wit::wire` (`lib.rs:76`, `lib.rs:213`),
   so `encode_schema_value`'s output feeds either. We use `bindings::golem::agent::host::WasmRpc`: it is
   unconditional, and golem's own reference component (`test-components/agent-rpc/`) uses exactly that
   in exactly clank-agent's configuration.
2. **`decode_value`'s signature flips on a *feature*, not a target.** `golem-schema` gates it on
   `all(feature="guest", not(feature="host"))`: the guest path takes the tree **by value** (to move
   owned handles out), the pure path takes it **by reference**. `golem-rust` pins
   `golem-schema = { default-features = false, features = ["guest"] }` and never `host`, so the
   by-value form holds on native *and* wasm — which is why one source compiles for both.
3. **The old silent-drop bug is now structurally impossible.** `main`'s `encode_args` used
   `filter_map(…ok())`, so an argument that failed to encode was **dropped, shifting every later
   positional arg** — a wrong-agent-call rather than an error. On the new model per-argument lowering is
   *infallible* (`SchemaValue::String` is a direct variant); only the single whole-tree encode can fail,
   and that error is propagated. The bug couldn't be ported even on purpose.

**`cancel` stays honestly stubbed — and that is not a value-model problem.** A scheduled invocation's
`cancellation-token` is a host resource that cannot survive across the agent's serialized invocations
(Golem parks between them), so it can't be re-acquired later for a `kill`. That constraint is unchanged
by the SDK bump; it reads identically on `main`.

**ACTIVE:** Rewritten and green. **No capability is lost on the branch.** Verified by 10 native unit
tests over the pure halves (encode/decode/render/phantom/epoch — a layer `main` had **zero** coverage
for) plus the live `scripts/golem-e2e.sh --with-grease` round-trip through a real `GreeterAgent`.

The same model swap is visible in `Cargo.lock` — `golem-rust`'s own dependency list:

```
main:   golem-rust 2.1.0  → "golem-wasm"
spike:  golem-rust 0.0.0  → "golem-schema", "wit-bindgen 0.58.0"
```

### The value-model break reaches the TEST HARNESS too — the sleeper

`scripts/golem-e2e.sh` was byte-identical to `main`'s and **every one of its ~290 assertions failed on
this branch** (first run: **4 passed, 288 failed**) — each one reading back `got: ''`, starting from a
trivial `mkdir -p /tmp/work; echo ready`. It looks exactly like catastrophic agent breakage. It was
**entirely the result reader**. Two independent changes in the dev CLI's `agent invoke --format json`,
both of which fail *silently* (empty string, never an error):

1. **The JSON key is now `resultJson`, not `result_json`.** `grep '"result_json"'` matched **no line**,
   so the helper emitted nothing at all.
2. **The record arrives as a POSITIONAL `schema-value-tree`** — field names live in the type graph, not
   the value, so **`.value.stdout` does not exist**. Fields come back in declaration order as tagged
   `{kind, value}` nodes: `[0]`=stdout `[1]`=stderr `[2]`=exit_code `[3]`=pending_prompt (option →
   `.value.inner`, record → `.value.fields`, list → `.value.elements`).

Both breaks existed in **two** helpers — `eval_json` (the structured path) and `run_line` (the text
path most assertions use). Fixing them means remapping positional→named **inside those two helpers**,
which leaves all ~290 downstream `jq` filters untouched.

**This makes field declaration order in `clank_agent.rs` (`EvalResult`, `PendingPromptView`) a wire
contract** for anything decoding an invoke result — the same reason golem-cli's `#[derive(FromSchema)]`
matches by position. Reordering fields silently mis-assigns values.

`golem-native-testing/probe.sh` already documented #2 and sidesteps both by reading the top-level
`"result"` Debug string (parsed with python — *not* `grep -o '"result":"[^"]*"'`, which truncates at the
first escaped quote and makes a good result look empty).

**The cost worth reporting to the maintainers:** a value-model change is not confined to guest code. It
silently invalidates every *out-of-band* consumer of the CLI's JSON — test harnesses, CI scripts,
dashboards — with an empty read rather than a parse error.

---

## 4. Bucket 3 — forced by the ecosystem lagging the SDK (and it came out ahead)

**WHAT:** `ask`'s durable provider was re-implemented. `crates/clank-agent/src/ask_provider.rs`
(178 → 99 lines), plus a new shared module and a refactor:

| File | Change |
|---|---|
| `crates/clank-shell/src/ai/anthropic_wire.rs` | **NEW**, 311 lines — the target-agnostic Anthropic `POST /v1/messages` wire format |
| `crates/clank-shell/src/ai/anthropic_native.rs` | 455 → 206 lines — now a thin `reqwest` transport over the shared wire |
| `crates/clank-shell/src/ai/mod.rs` | `pub mod anthropic_wire;` — **deliberately un-`cfg`-gated** |
| `crates/clank-agent/Cargo.toml:27-32` | `golem-ai-llm` / `golem-ai-llm-anthropic` commented out |

**WHY:** `^0.5.1` resolves to `golem-ai-llm 0.5.2`, which hard-pins `golem-rust = "=2.1.0"`. That is an
**exact** pin — it cannot coexist with a `0.0.0` path dep, and `[patch]` cannot override an exact
requirement either. The crate is irreconcilable with the dev SDK, full stop.

**The replacement, and why it is not a downgrade:** `ask` now POSTs to Anthropic **directly over
`wstd::http`** — the same durable client `WstdMcpHttp` uses for MCP and `wcurl`/`waget` use for
`curl`/`wget`. On Golem the runtime **records that HTTP call in the oplog and replays it on recovery**,
so the LLM response is not re-billed after a restart. That is precisely the guarantee
`golem-ai-llm`'s `DurableAnthropic` provided — **obtained from the transport instead of a wrapper
crate**. Durability was relocated, not lost.

It cost nothing to reach for: **`wstd` was already a direct dependency** at `=0.6.5`
(`crates/clank-agent/Cargo.toml:19`, unchanged from `main`). **Net: the spike removes two dependencies
and adds none.**

**The wire module is the interesting part.** The request/response mapping (neutral
`AskTurn`/`AskTool`/`AskToolCall`/`AskToolResult` ↔ Anthropic JSON) was already written and tested — but
privately, inside the native-only provider. It was extracted verbatim into `anthropic_wire`, which is
**ungated** and depends only on `serde_json` + the neutral types. Two functions were added for one
specific reason: `serialize_request` (`Value` → `Vec<u8>`) and `parse_response_body` (`&str` →
`AskResponse`) give a **byte-level boundary**, so the wasm agent can build/parse requests through the
module **without taking a `serde_json` dependency** (`clank-agent` has `serde` but not `serde_json`).

Three things improved as a side effect:
- the wire format is now **defined once** — native and agent cannot drift;
- `MAX_TOKENS` was **two separate `4096` constants** kept in sync by a comment; now one;
- the pure mapping tests were **native-only** (`anthropic_native` is `#[cfg(not(target_arch = "wasm32"))]`)
  and are now in an ungated module — **they cover the agent's wire path too**. That is real new coverage,
  not a move.

**ACTIVE:** Yes — `ask` works on the dev SDK and has been verified live end-to-end against the real API.

⚠ **Honest caveat:** `MAX_BODY` (`ask_provider.rs:28`, 8 MiB) is checked **after** the body is fully
buffered (`:94`). It bounds the *reported* size, not the allocation — a sanity check, not a memory guard.

---

## 5. Bucket 4 — incidental: a real bugfix that rode this branch

**WHAT:** `4bd6fa2` — `crates/clank-shell/src/session/mod.rs:479-494` (+ a regression test at
`crates/clank-shell/src/session/tests.rs:3336-3363`).

**This has nothing to do with the SDK** and is a **merge-back candidate for `main`.**

When a `prompt-user`/authorization pause is outstanding and a new command arrives, the shell rejects it
(correct) — but returned that rejection via `LineResult::stderr(...)`, which hard-codes
`pending_prompt: None`. A client seeing an empty `pending_prompt` concludes nothing is pending, hands
control back, and routes the user's next line to `eval` — which is rejected again. Forever.

The fix re-surfaces the outstanding prompt (`pending_prompt: Some(...)`, cloned off `self.pending`) with
the **same message and the same `exit_code: 1`** — only that one field flips `None` → `Some`. It mirrors
an existing precedent, the `InvalidChoice` re-ask in `session/prompt.rs`.

**Why it surfaced now:** building the `agent stream` interactive-shell patch in `golem-stuff/golem`
exposed it. A connect session that Ctrl-C'd mid-`ask` left the agent holding a prompt; the next session
couldn't recover. See `golem-native-testing/GOLEM-CONNECT.md`.

---

## 6. ⚠ THE FINDING — the spike adds a **third git fork** to `Cargo.lock`

[WASM_CHANGES.md](WASM_CHANGES.md) opens with this invariant:

> *"No other crate is pinned to a git rev (verified: the only `git+` sources in `Cargo.lock` are the
> Brush and coreutils forks)."*

**That sentence is false on `spike/dev-golem-sdk`.** Verified by diffing the lock:

```
main  (2 git sources):
  git+https://github.com/Aditya1404Sal/brush?rev=309a054#309a054576090e428d1874a1210306ed4f7f29aa
  git+https://github.com/Aditya1404Sal/coreutils?branch=wasip2-oscompat#35ecf24d7caa2202940a18ef61be5037776ecd36

spike (3 — both of the above, unchanged, PLUS):
  git+https://github.com/golemcloud/wit-bindgen?branch=golem-outline-lift-v0.58.0#78a74643fa25708ff6d134b9b2ffc80de70c89b1
```

**WHAT:** golemcloud's own **fork of `wit-bindgen`** (`0.58.0`), replacing the crates.io `wit-bindgen
0.53.1` that `golem-rust 2.1.0` resolved.

**WHY it matters:** it is a **third-party toolchain fork silently in clank's build**, and **nothing in
clank's own manifests mentions it.** It arrives transitively through the dev SDK's dependency graph, so
it is invisible unless you read `Cargo.lock`. Anyone auditing clank's supply chain from `Cargo.toml`
alone will miss it.

Note this is *not* the `wit-bindgen 0.57` in clank's `[workspace.dependencies]` — that one is only used
by `clank-shell`'s `repl-driver` feature, which the agent build drops. They coexist without conflict
(a wall the spike expected to hit and didn't).

**ACTIVE:** Yes, while this branch is checked out. **This section supersedes WASM_CHANGES.md's
two-forks claim for `spike/dev-golem-sdk`.**

*Footnote:* WASM_CHANGES.md is *already* drifted from `main` independently of this spike — it says it was
verified against branch `clank-golem-agent` and cites brush `rev = "0f4a89c"`, but **both** `main` and
the spike now carry `rev = "309a054"`. Don't inherit that number.

---

## 7. What is NOT changed (so nobody hunts phantoms)

- **`[workspace.dependencies]` is byte-identical** to `main`. Verified by diff. `wit-bindgen = "0.57"`,
  `wasip3 = "0.7"`, `futures = "0.3"` — untouched.
- **The entire `[patch.crates-io]` block is byte-identical** (uucore + 19 `uu_*` + the 3 brush crates).
  Verified by diff. **The whole WASM_CHANGES.md fork story survives the spike intact** — the coreutils
  and Brush forks, the `cfg`-gated wasm infrastructure, the hand-rolled text tools: all unaffected.
- **`wstd = "=0.6.5"` and `serde` are unchanged** in `crates/clank-agent/Cargo.toml` — which is exactly
  why §4's rewrite cost zero new dependencies.
- **`crates/greeter-agent/` is back in the build.** It was briefly dropped from `members`/added to
  `exclude` and removed from `golem.yaml` (never deleted) while §3 was stubbed, because it pinned
  `golem-rust 2.1.0`. Restoring it took **three edits and zero source changes**: repoint its one dep to
  the dev-SDK path (`Cargo.toml:17`, in lockstep with clank-agent — greeter is the wRPC *target*, so it
  must speak the same `golem:agent@2.0.0` world as the invoker calling it), put it back in `members`,
  restore its `golem.yaml` block. **`src/lib.rs` needed no change at all** — a plain
  `#[agent_definition]` String→String agent absorbed the WIT bump exactly as `clank_agent.rs` did, which
  is a small but real data point about the migration's blast radius on ordinary agent code.
- **`golem.yaml` is otherwise byte-identical**: `app: clank`, the `environments:` block, and the
  `clank:agent` component with its `ANTHROPIC_API_KEY: "{{ ANTHROPIC_API_KEY }}"` passthrough.

### The dependency wall that wasn't

The spike was scoped expecting to vendor ~4 SDK crates and rewrite ~45 workspace-inherited deps. It
took **one line**: `exclude = ["golem-stuff"]` (`Cargo.toml:9`). That makes cargo resolve
`golem-schema`'s `{ workspace = true }` deps against **its own** workspace (`golem-stuff/golem/`)
instead of clank's root. No vendoring, no dep rewrite.

---

## 8. Known-stale / follow-ups

- **`agent_invoker.rs` is DONE** (§3) — the branch has **no stubs left**. What remains is not clank's to
  fix: the spike still path-deps an unreleased `golem-rust 0.0.0`, so it cannot merge until the dev SDK
  ships and `golem-ai-llm` stops pinning `=2.1.0` (§4). That is a release-timing wall, not a code wall.
- **`crates/clank-shell/src/session/mod.rs`'s fix (`4bd6fa2`)** should be **cherry-picked to `main`** —
  it is SDK-independent (§5).
- **`scripts/golem-e2e.sh` still calls a bare `golem`** (it is byte-identical to `main`'s). On this
  branch that resolves to whatever is on `PATH` — a release **1.5.1** CLI cannot parse this branch's
  `manifestVersion: 1.6.0`. Run it with the dev binary ahead on `PATH`
  (`golem-stuff/golem/target/debug/golem`), the way `golem-native-testing/setup.sh:16` already does.
- **The `fork` re-export discrepancy** in §2 is unresolved; re-check against `golem-rust 2.1.0`'s source
  when a network/registry copy is available.
- The dev binary **rewrites tracked files in this repo** when it builds/deploys (it has rewritten
  `AGENTS.md`, and once `golem.yaml` + Cargo manifests). Always `git status` after a `golem build`.
  `golem-native-testing/teardown.sh` checks for this.
