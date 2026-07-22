# clank — Verification commands

This audit is **read-only**: it never built, linted, ran tests, or hit the network. Therefore it
**cannot know** what a compiler, linter, advisory database, or test run would report — only what the
source says. Every claim that depends on a tool is graded `Unverified` in `AUDIT.md` and mapped to a
command here. Run these to promote `Unverified` / `Likely` findings to `Confirmed` (or refute them).

**Build note — do NOT use `cargo … --workspace`.** `clank-agent` and `greeter-agent` are wasm
`cdylib` crates that do not link on the native host; `clank-shell` is `cdylib`+`rlib`. Native tooling
must target crates explicitly with `-p`, and the wasm crates build only for `wasm32-wasip2`. Examples
below reflect that.

**Secrets.** Any probe that exercises `ask` needs a real `ANTHROPIC_API_KEY`; run those yourself. The
audit never handled the key.

---

## Standard suite (run all of these on a release candidate)

| # | Command | What it confirms |
|---|---|---|
| S1 | `cargo clippy -p clank-shell -p clank-embed -p whttp -p wcurl -p waget -p clank-conformance -p grease-tool --all-targets -- -W clippy::pedantic -W clippy::nursery` | Real lint density; the `unwrap_used`/`expect_used` picture behind the ~470 panic sites |
| S2 | `cargo clippy --target wasm32-wasip2 -p clank-agent -p greeter-agent -- -W clippy::pedantic` | Lints on the wasm guest crates specifically (dual-target issues native clippy never sees) |
| S3 | `cargo fmt --check` | Whether the tree is even format-consistent (no `rustfmt.toml` exists today — see RESTRUCTURE C1) |
| S4 | `cargo audit` | Any RUSTSEC advisory on the dependency tree (incl. the `[patch]`ed forks) |
| S5 | `cargo deny check` | Licenses, banned/duplicate crates, and the **git-source** `[patch]` deps (coreutils/brush forks) — see RESTRUCTURE C5 |
| S6 | `cargo +nightly udeps` (or `cargo machete`) | Unused dependencies pulled into the build |
| S7 | `cargo tree -d` | Multiple versions of the same crate in the tree |
| S8 | `cargo miri test -p clank-shell` | Soundness of the `unsafe` blocks under UB detection (native-side: the `mcp/client.rs` waker + any native fd path) |
| S9 | `cargo test -p clank-shell -p clank-embed -p whttp -p wcurl -p waget -p clank-conformance` then `scripts/golem-e2e.sh` | The native + golem-e2e suites actually pass at HEAD |

> `cargo miri` cannot run the wasm `libc` fd manipulation in `tools/coreutils.rs` (that path is
> native-only `libc`; the wasm path uses `__wasilibc_fd_renumber`). Miri covers the native side and
> the `mcp/client.rs` unsafe; the wasm fd path must be argued by review + golem-e2e, not Miri.

---

## Finding-specific verification

These rows are keyed to `AUDIT.md` finding IDs. (IDs are filled in when `AUDIT.md` is finalised; the
structural ones below are already known from the read-only pass.)

| Finding | Command / probe | What confirms it |
|---|---|---|
| **P0-1 shell.log key leak** | `model add anthropic --key sk-TESTVALUE123` in a shell, then `grep -c 'sk-TESTVALUE123' /var/log/shell.log` (native: the log dir; agent: the instance FS via `golem agent` file view) | A non-zero count = the key is persisted in cleartext (expect 2 hits — start + end events). |
| **P0-2 sed panic → durable wedge** | Native: a test driving `sed 's/\BX/Y/1'` over `aX` and asserting it does not panic. Agent: deploy, `eval` that exact `echo aX \| sed 's/\BX/Y/1'`, then `golem agent get <name>` | That the native run panics today (repro), and whether the agent instance goes to a trapped/wedged state vs returns an error `EvalResult` — the blast-radius question for P1-2 as a class. |
| **Panic-trap blast radius (general)** | As P0-2, plus other reached panic paths (malformed coreutils args, parser `unwrap`s) | Whether a trap wedges the durable instance vs fails one invocation — settles P1-2's P0-vs-P2 for the un-enumerated remainder. |
| **P1-4 MAX_BODY OOM** | Point the agent's MCP/LLM client at a test server that streams a body far larger than `MAX_BODY` (4/8 MiB) and watch the worker's memory | Peak RSS exceeding `MAX_BODY` before the cap returns an error = the cap is post-hoc (OOM risk). |
| **`$(…)` command-substitution bypass on the model path** (AUDIT P1) | Native test in `session/tests.rs` shape: drive `run_ask` with a scripted provider whose tool call is `echo $(curl http://127.0.0.1:PORT/x)`; assert whether the inner `curl` executes without hitting the authz/scope gate. (No key needed — the *tool dispatch* is what's under test, not the LLM.) | That the inner command runs unguarded — i.e. the gate resolved only `echo`. Confirms the bypass end-to-end and measures blast radius (does the inner command re-enter clank interception?). |
| **`unsafe` SAFETY comments** (AUDIT unsafe finding) | `rg -B3 'unsafe \{' crates/clank-shell/src/tools/coreutils.rs crates/clank-shell/src/mcp/client.rs` and read each; then `cargo miri test -p clank-shell` (S8) | Presence/adequacy of a `// SAFETY:` per block; Miri exercises the native fd-swap + waker for UB. |
| **Dead code** (the 6 `allow(dead_code/unused)` sites) | Remove each `#[allow(...)]` locally and `cargo clippy -p <crate>` | Whether the suppressed lint was hiding genuinely-unused code vs a false positive. |
| **`clone()` density** (315 sites) | `cargo clippy … -W clippy::redundant_clone` (part of S1's nursery set) | Which clones are redundant vs load-bearing borrow-checker appeasement. |
| **Unbounded collections** (proctable/pending_invocations were bounded; are there others?) | Long-running agent soak: drive many invocations, snapshot memory / oplog size growth over time | Whether any long-lived `Vec`/`HashMap`/cache still grows without eviction on the durable instance. |
| **Reproducible build** (RESTRUCTURE C5) | Fresh clone → `cargo build` on a machine with an empty cargo git cache; compare resolved coreutils commit to `Cargo.lock` | Whether the branch-pinned coreutils fork resolves deterministically or drifts to branch-tip. |

---

## Two-branch note

All security findings in `AUDIT.md` were checked against both `main` and `main-rc-1` with
`git grep`/`git diff origin/main...origin/main-rc-1 -- <file>`; the security hardening
(`split_segments`, `MODEL_SAFE_BUILTINS` default-deny, grease `found_in_index`) is present on **both**
branches, so those findings tag **[both]**. Re-run `git diff --stat origin/main...origin/main-rc-1`
before release to confirm no security-relevant file diverged after this audit.

---

## What this file is not

It is not a promise that these commands pass — the audit could not run them. It is the list a
maintainer runs to replace the audit's honest "I could not execute this" with a real result. If a
command here fails, that failure is itself a finding.
