# AGENTS.md

## Project

clank.sh is an AI-native shell targeting `wasm32-wasip2` and native Rust. See `README.md` for full design documentation.

## Build & Test

The shell (`crates/clank-shell`) runs on two targets from one codebase, split by
`#[cfg(target_arch = "wasm32")]`. Command evaluation (`eval`) is a pure, shared core; only
the I/O differs. See `dev-docs/designs/proposed/target-abstraction-native-and-wasm.md` and
`dev-docs/designs/proposed/shell-entrypoint-and-io-realized.md`.

- **native** — an ordinary binary (`clank`) over blocking `std::io`.
- **wasm32** — a `wasi:cli/run` component (WASI 0.3, async p3 streams). Compiled with the
  stable `wasm32-wasip2` rustc target but binding the p3 (`wasi:cli@0.3.0`) interfaces via
  component-model async (`wasip3` crate).

### Host tests (fast; no wasm)

```sh
cargo test        # exercises the pure `eval`/`trim_eol` core on the host
```

### Native

```sh
cargo build --bin clank                 # → target/debug/clank
cargo run --bin clank                    # interactive REPL
printf 'echo hello\nhelp\nexit\n' | cargo run -q --bin clank   # smoke test
```

### Wasm component

Prerequisites: `rustup target add wasm32-wasip2`; **Wasmtime ≥ 46** (final `wasi-0.3.0`).

```sh
cargo build --release --target wasm32-wasip2 --lib
# → target/wasm32-wasip2/release/clank_shell.wasm
```

Use `--lib`: the crate also has a `clank` bin whose `main` is empty on wasm. Run on
wasmtime (`-Sp3` = WASI 0.3 imports, `-W component-model-async=y` = component-model async):

```sh
wasmtime run -Sp3 -W component-model-async=y \
  target/wasm32-wasip2/release/clank_shell.wasm
printf 'echo hello\nhelp\nexit\n' | wasmtime run -Sp3 -W component-model-async=y \
  target/wasm32-wasip2/release/clank_shell.wasm
```

## Code Conventions

_To be filled in._

## Development Workflow

All development artifacts live in `dev-docs/`. Everything is a Markdown file with YAML frontmatter.

**One feature branch (one PR) carries exactly one issue, one design, and one plan**, sharing a
single kebab-case slug. A `dual-target-shell-loop` branch has:

```
dev-docs/issues/open/dual-target-shell-loop.md
dev-docs/designs/proposed/dual-target-shell-loop.md
dev-docs/plans/proposed/dual-target-shell-loop.md
```

Research is the only exception: zero or more docs under `dev-docs/research/`, freely named.

### Document Types

| Type | Location | Purpose |
|---|---|---|
| Issue | `dev-docs/issues/open/` → `closed/` | The problem / capability gap and why it matters. No solution detail. |
| Design | `dev-docs/designs/proposed/` → `approved/` | What is built and how: approach, key decisions, rejected alternatives. Written as intent, finalized to its as-built state by merge, then frozen. |
| Plan | `dev-docs/plans/proposed/` → `done/` | The steps: a `## Tasks` checkbox list, acceptance tests, and deviations noted during implementation. |
| Research | `dev-docs/research/` | Optional. Raw investigation and prior art. Informs; does not decide. |

### Frontmatter Schema

Every document begins with YAML frontmatter:

| Field | Issue | Design | Plan |
|---|---|---|---|
| `title` | required | required | required |
| `date` | required | required | required |
| `author` | required | required | required |
| `issue` | — | — | required — path to the branch's issue |
| `design` | — | — | required — path to the branch's design |
| `research` | — | — | optional — paths to research consulted |

Lifecycle state is encoded in directory position, not frontmatter — there is no `status` field.
For agent-authored documents, use `author: agent`. Use ISO 8601 dates (`YYYY-MM-DD`). All three
docs share the feature slug in `kebab-case`.

### Lifecycle (per feature branch / PR)

1. **Open the branch.** Create the issue (`issues/open/`), design (`designs/proposed/`), and plan
   (`plans/proposed/`) under one slug. The design records the approach and the decisions behind
   it; the plan lists `## Tasks` and acceptance tests. Consult the developer on significant design
   decisions and record their feedback in the design or plan.
2. **Implement on the branch.** Check off tasks as they complete; note deviations inline in the
   plan. Keep the design in sync so it reflects what was actually built. If implementation reveals
   the plan is fundamentally wrong, stop and reconsider with the developer rather than proceeding
   unilaterally.
3. **Merge (the human gate).** Review happens on the PR. On merge: issue → `issues/closed/`,
   design → `designs/approved/` (now as-built), plan → `plans/done/`. All three become immutable.

### Rules

- Agents write; the human gates the merge, which moves the docs to `closed/`/`approved/`/`done/`.
- Documents in `closed/`/`approved/`/`done/` are never modified — permanent record.
- The code is ground truth for current state; the design records intent + as-built decisions at
  merge time, not a live mirror of the code.
- Agents must not modify files in `dev-docs/issues/closed/`, `dev-docs/designs/approved/`, or
  `dev-docs/plans/done/`.
- Exactly one issue, one design, one plan per feature branch, sharing its slug. (Research is the
  only exception: zero or more, freely named.)

