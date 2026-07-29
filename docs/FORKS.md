# FORKS.md — every third-party fork clank depends on

Audience: anyone who needs to answer *"what did we change in someone else's code, why, and what
happens if that fork disappears?"* — before a dependency bump, a security review, or an attempt to
build clank on a fresh machine.

**Five forks. Three forms.**

| # | Fork | Form | Cures |
|---|------|------|-------|
| 1 | [coreutils](#1-coreutils--the-uu_-command-crates) (`uucore` + 18 `uu_*`) | git dep, `rev = 35ecf24` | wasip2 `OsStr` encoding, empty-argv panic, `uu_cp` permissions |
| 2 | [brush](#2-brush--the-shell-interpreter) (3 crates) | git dep, `rev = 02de798` | wasip2 file redirects, pipelines, `$(…)`, heredocs |
| 3 | [reedline](#3-reedline--the-native-line-editor) | vendored path (`reedline-fork/`) | native REPL prompt hang after `ask` |
| 4 | [crossterm](#4-crossterm--the-terminal-backend) | vendored path (`crossterm-fork/`) | the same hang, from the other side |
| 5 | [golem](#5-golem--the-agent-shell-cli-patch) | nested git clone (`golem-stuff/golem`) | supplies `golem agent shell` |

Three sentences of orientation before the detail:

- **Forks 1 and 2 are load-bearing for the durable agent.** Without them clank does not *work* on
  `wasm32-wasip2` — redirects silently discard output, pipelines don't exist, `ls` panics on argv.
- **Forks 3 and 4 are quality-of-life for the native REPL**, paired to fix one symptom. They never
  enter the wasm build.
- **Fork 5 is not a dependency at all** — it is a separate clone that produces a CLI *binary*, not a
  crate clank links.

The wasm-specific reasoning for forks 1 and 2 — which upstream commit, which syscall, which
`Unsupported` — is written up at length in [`docs/WASM_CHANGES.md`](WASM_CHANGES.md). This file is the
ledger: what exists, where it is pinned, and what the exposure is. It does not repeat that analysis.

---

## The risk that applies to all of them

**Forks 1 and 2 live in a single maintainer's personal GitHub account** (`Aditya1404Sal/coreutils`,
`Aditya1404Sal/brush`). A delete, a rename, a force-push that drops the pinned object, or an account
change breaks **every build of clank on every machine that hasn't already got the git cache warm** —
including CI, including a fresh clone, including a `golem deploy`. Cargo will fail to resolve, not
fall back.

Two things reduce that today, and one thing does not:

- **Reduces it:** both are pinned to an exact `rev`, not a branch. A branch pin silently advances to
  branch-tip on a fresh resolve (audit P2-7); a rev pin is reproducible from source control alone and
  a force-push that *keeps* the object still resolves.
- **Reduces it:** `Cargo.lock` records the full 40-char hash for each, so the exact object is named
  even though `Cargo.toml` abbreviates.
- **Does not reduce it:** nothing mirrors these repositories. The `Cargo.toml` comment says "mirror
  the fork so a delete/force-push can't break the build" — that is an instruction that has not been
  carried out. **This is the single highest-leverage supply-chain fix available to this project**, and
  it costs one `git push --mirror` per fork to an org-owned remote plus a one-line URL change.

Forks 3 and 4 have none of this exposure — the source is vendored in-tree and committed. Their cost
is the opposite one: nothing tells you when upstream moves, so they go stale silently.

---

## 1. coreutils — the `uu_*` command crates

|  |  |
|---|---|
| **Upstream** | [`uutils/coreutils`](https://github.com/uutils/coreutils) |
| **Fork** | `https://github.com/Aditya1404Sal/coreutils`, branch `wasip2-oscompat` |
| **Pin** | `rev = "35ecf24"` (lock: `35ecf24d7caa2202940a18ef61be5037776ecd36`) |
| **Scope** | 19 `[patch.crates-io]` entries — `uucore` plus 18 `uu_*` command crates |
| **Targets** | both (the patch is unconditional; native behaviour is unchanged) |
| **Upstreamed?** | no — no PR found from this author on `uutils/coreutils` |

**Why 19 entries and not one.** The published `uu_*` crates share the patched `uucore`, so a fix
*inside* `uucore` reaches all of them through one patch line. But a fix inside a *command* crate —
the `set_permissions` skip in `uu_cp` — is only picked up when that command crate itself comes from
the fork. Hence every command clank registers is patched individually. Adding a new coreutil to
clank's registry means adding its crate here too.

**What changed:**

- **`OsStr` encoded-bytes shim.** `uucore` reached for a platform API that does not exist on
  `wasm32-wasip2`; the fork adds a stable shim.
- **Empty-argv guard.** A `uu_*` `uumain` invoked with an empty argv panicked. On the agent a panic
  is a trap, so this was an instance-killer reachable from an ordinary command. (Fixed and pinned —
  see the memory note `golem-agent-filesystem-findings`.)
- **`uu_cp` permission skip.** `set_permissions` is `Unsupported` on wasi; `cp` failed on a target
  that otherwise worked.

**If it vanishes:** clank does not build at all, on either target. Every registered coreutil is gone.

---

## 2. brush — the shell interpreter

|  |  |
|---|---|
| **Upstream** | [`reubeno/brush`](https://github.com/reubeno/brush), base commit `0300a84` |
| **Fork** | `https://github.com/Aditya1404Sal/brush`, branch `std-utils` (stacked on `wall-c-wasm-pipes`) |
| **Pin** | `rev = "02de798"` (lock: `02de798167b633fca57dd81efe253afa18f124d4`) |
| **Scope** | `brush-core`, `brush-builtins`, `brush-parser` — one monorepo, pinned in lockstep |
| **Targets** | both; the wasm changes are `cfg`-gated, native behaviour is unchanged |
| **Upstreamed?** | no — no PR found from this author on `reubeno/brush`. One change (the `Arc<File>` redirect refactor) is *already on upstream `main`* and simply not released to crates.io — that part is a backport, not a divergence. |

**What changed** (detail in [`WASM_CHANGES.md`](WASM_CHANGES.md) §1):

- **File redirects.** Published `brush-core 0.5.0` holds a redirect target as
  `OpenFile::File(std::fs::File)` and duplicates it with `File::try_clone()` — `Unsupported` on
  wasip2, so `echo > file` **silently discarded the write** on the agent. Upstream's `Arc<File>`
  refactor fixes it; the fork carries it ahead of a release.
- **Pipelines, `$(…)`, and heredocs on wasm.** `std::io::pipe()` is unsupported on wasip2 and there
  is no blocking thread pool, so all three now run through an in-memory `OpenFile::Stream` pipe with
  inline-sequential stages (producer completes, drops its writer, reader sees clean EOF) instead of
  OS pipes plus `tokio::spawn`. This is the "Wall C" work.
- **POSIX `unquote_str` fix** (rev `02de798`), which the `curl | jq` pipeline work depends on.

**If it vanishes:** clank does not build. There is no shell.

---

## 3. reedline — the native line editor

|  |  |
|---|---|
| **Upstream** | published `reedline 0.38.0`, verbatim |
| **Fork** | vendored at `reedline-fork/` (in-tree, committed) |
| **Scope** | **one** patch site: `src/painting/painter.rs` |
| **Targets** | native only — never in the wasm build |
| **Upstreamed?** | no |

**The symptom.** `initialize_prompt_position` propagates a `cursor::position()` timeout with `?`. A
terminal that answers the Device-Status-Report query late — which is exactly what happens right after
`ask` dumps a burst of output — aborts `read_line` and drops the shell into a degraded fallback.

**The fix.** Make that one call tolerant: fall back to (col 0, bottom row). Correct for clank
specifically, because clank always ends command output with a newline.

**Why vendored rather than a git dep:** it is the exact published source plus a one-line change, and
it is native-only. Productionizing means pushing it as a real fork and swapping to a `git`+`rev` pin
like the two above — at which point it inherits their single-maintainer risk, which is the tradeoff.

---

## 4. crossterm — the terminal backend

|  |  |
|---|---|
| **Upstream** | published `crossterm 0.28.1`, verbatim |
| **Fork** | vendored at `crossterm-fork/` (in-tree, committed) |
| **Scope** | **two** patch sites: `src/cursor/sys/unix.rs`, `src/terminal/sys/unix.rs` |
| **Targets** | native only |
| **Upstreamed?** | no |

Paired with fork 3 — same symptom, other end. crossterm's `cursor::position()` (the call reedline
makes) and its keyboard-enhancement probe both poll for a terminal DSR reply with a **2000 ms**
ceiling, so a late-answering terminal blocks roughly a second *at every prompt*. The fork cuts both
ceilings to **250 ms** so the query fails fast and reedline-fork's fallback takes over immediately. A
responsive terminal replies in under 10 ms — far inside the window — so it is unaffected.

**Only crossterm 0.28 is patched** (reedline's). Brush pulls crossterm **0.25**, a distinct version,
left untouched. `scratchpad/pty_latency.py` is the probe that measured the timeout.

---

## 5. golem — the `agent shell` CLI patch

|  |  |
|---|---|
| **Upstream** | [`golemcloud/golem`](https://github.com/golemcloud/golem) |
| **Fork** | `https://github.com/Aditya1404Sal/golem`, branch `clank-connect-patch` |
| **Form** | a **nested git clone** at `golem-stuff/golem` — its own repo, with its own remotes, gitignored by clank |
| **Scope** | 10 commits, 12 files, +3200/−2 |
| **Upstreamed?** | **attempted and auto-rejected** — see below |

### The upstreaming attempt, and the actual blocker

[**PR #3700**](https://github.com/golemcloud/golem/pull/3700) was opened 2026-07-16 17:19:08Z and
**closed 19 seconds later, at 17:19:27Z**, by a bot:

> Hi @Aditya1404Sal, thanks for your interest in contributing! This project requires that pull
> request authors are vouched, and you are not in the list of vouched users. This PR will be closed
> automatically.

The CLA *was* signed. **The PR was never reviewed on technical merit** — no maintainer looked at it.
So the blocker is not code quality, an API objection, or a design disagreement; the patch has simply
never been read.

The unblock is documented in [`.github/VOUCHED.td`](https://github.com/golemcloud/golem/blob/main/.github/VOUCHED.td):

> To vouch for a new contributor, a maintainer can comment `vouch @username` on any issue in this
> repository.

**So: ask a Golem maintainer to comment `vouch @Aditya1404Sal` on any issue, then reopen or re-file
#3700.** That is the whole gate. Worth doing — every rebase of this fork costs real work (this one
cost three compile errors from a single upstream API change), and that cost recurs forever while the
patch lives out-of-tree. Note the PR would need rebasing onto current `main` first; that is already
done as of 2026-07-29.

**This one is not a dependency.** Nothing in clank's `Cargo.toml` points at it. It builds the
`golem` CLI *binary*, which supplies `golem agent shell` — the command that drives a deployed clank
agent as an interactive shell. Clank builds and deploys fine without it; you just cannot connect that
way. (On `main-rc-1` the same clone additionally serves the dev Golem SDK via path deps, which *is* a
build dependency — see that branch's `DEV_SDK_CHANGES.md`.)

**Shape of the diff — and why that shape matters:** only **five files are modified**; everything else
is **new**. The new files (`interactive_shell.rs` at 1112 lines, `tests/agent_shell.rs`, the
`test-components/agent-shell/` component) cannot textually conflict with anything upstream does. So a
rebase is rarely a merge-conflict problem — it is a **compile-level API drift** problem, and drift
shows up as build errors after a clean rebase rather than as `<<<<<<<` markers.

| Modified file | Change |
|---|---|
| `cli/golem-cli/src/command_handler/worker/mod.rs` | +42/−2 — dispatch into the shell handler |
| `cli/golem-cli/src/command.rs` | +8 — the `agent shell` subcommand |
| `cli/golem-cli/src/command_examples.rs` | +7 — help examples |
| `golem-worker-executor/tests/lib.rs` | +7 — register the integration test module |
| `test-components/build-components.sh` | +1/−1 — build the `agent-shell` test component |

**Rebase procedure:**

```bash
cd golem-stuff/golem
git branch -f clank-connect-patch-prerebase-<date> HEAD   # backup ref, always
git fetch upstream
git rebase upstream/main
cargo build -p golem-cli                                   # the real gate
cargo test -p golem-cli 'command_handler::worker::'
git push --force-with-lease origin clank-connect-patch     # origin ONLY, never upstream
```

**Last rebase: 2026-07-29**, from base `96cffbc17` (2026-07-16) onto `79122bd92` (2026-07-27) — 16
upstream commits, 11 days of drift. It went exactly the way the shape above predicts:

- **One textual conflict**, trivial: upstream added `agent-self-rpc` to the TypeScript test-app list
  in `build-components.sh` while our commit added `agent-shell` to the Rust one. Both kept.
- **Three compile errors, all in `interactive_shell.rs`, all from one upstream commit** —
  `bc50cf28c` *"Use SchemaValue instead of raw Json in the API (#3710)"*. `AgentInvocationRequest`
  changed `parameters` and `method_parameters` from `serde_json::Value` to `SchemaValue`, and gained
  a required `config` field. The fix was to drop two `serde_json::to_value(…)?` conversions (the
  values were already `SchemaValue`) and add `config: None` — **copied from upstream's own adapted
  call site** in `command_handler/worker/mod.rs`, which is the right source for this kind of fix: it
  is how upstream decided the new API should be used.
- Verified after: `cargo build -p golem-cli` clean, `cargo test -p golem-cli
  'command_handler::worker::'` **44 passed / 0 failed** (including all 10 `interactive_shell` tests).

The lesson worth keeping: **the unit tests build `SchemaValue` by hand, so they cannot catch a wire
regression.** They confirm the decode logic, not that the CLI and a deployed agent still agree. The
only thing that proves that is `golem agent shell` against a live clank agent.

**Watch-outs, each learned the hard way:**

- **Never push to `upstream`.** Keep the clone's own `main` clean.
- Commits in this clone carry **no** `Co-Authored-By: Claude` trailer (clank's do).
- Its tests are **test-r**: a module that does not `use test_r::test;` silently registers *nothing*
  and passes vacuously.
- `golem build` / `golem deploy` run from this clone **rewrite tracked files inside clank** — it has
  rewritten `AGENTS.md`. Run `git status` on clank afterwards.
- The working tree is ~92 GB with build artifacts. `cargo clean` there before worrying about disk.

---

## Not forks

For completeness, because they get mistaken for forks:

- **`utilities/whttp`, `wcurl`, `waget`, `grease-tool`** — first-party clank crates, not forks of
  anything. Their coverage is assessed in [`docs/audit/HANDROLLED.md`](audit/HANDROLLED.md).
- **`golem-rust`** — the real published SDK (2.1.0). Both wasm crates compile against it unmodified.
  A maintainer report about a trait-default-method dispatch footgun is outstanding, but no fork.

---

## Maintenance checklist

When bumping any fork:

1. **Bump the `rev`, never point at a branch** (audit P2-7).
2. Update the corresponding row in this file *and* the analysis in `WASM_CHANGES.md`.
3. `cargo clean` first if the target dir has wasm artifacts — a stale `target/` produces
   "failed to parse WebAssembly module", which reads like a toolchain regression and is not one.
4. Verify on **both** targets: `cargo test -p clank-core`, then `scripts/golem-e2e.sh`. A fork fix
   for wasm that breaks native is the failure mode these pins exist to prevent.
