# The `.clank` scenario format

Each file in this directory is one conformance scenario: a transcript of shell steps with
expected results, executed **verbatim against both targets** — the native in-process
`Session` and a deployed clank Golem agent. A behavioral divergence between the targets is
a red test here, not a live surprise. One file = one shell session (one native `Session` /
one fresh agent instance); steps share state top to bottom by design.

Run them:

```
cargo test -p clank-conformance --test native            # native tier — no server needed
scripts/conformance-golem.sh                             # golem tier — throwaway server + deploy
cargo test -p clank-conformance --test native pipelines  # one scenario, by file stem
```

## Grammar

Line-oriented. A line is `keyword`, **one ASCII space**, then a **verbatim payload** —
everything after the first space is content, so expected output can carry significant
leading whitespace (`out       3 f` pins uu `wc` padding exactly). Blank lines and lines
whose first non-space character is `#` are ignored.

### Header (before the first step)

| Directive | Meaning |
|---|---|
| `@only native` / `@only golem` | restrict the scenario to one target (used to pin documented divergences — say why in a comment) |
| `@requires <tier>` | needs a gated tier (`network`/`llm`/`grease`/`mcp`); reported `ignored` until that tier is wired into the harness |

### Steps

| Directive | Meaning |
|---|---|
| `run <line>` | evaluate one command line (`eval`) |
| `+ <line>` | continuation: appends a real newline + line to the `run` payload (here-docs, multi-line). Must DIRECTLY follow its `run` — no blank lines, comments, or expectations between. A blank content line (e.g. an empty here-doc line) is a lone `+` |
| `answer <text>` | resolve the pending prompt with `<text>` (`answer_prompt`) |
| `abort` | abort the pending prompt (`abort_prompt`) |

### Expectations (attach to the step above them)

| Directive | Meaning |
|---|---|
| `out <line>` | exact expected stdout; lines accumulate, the stream must equal them joined with `\n` plus a trailing `\n` |
| `out~ <substr>` | stdout must contain the substring (repeatable) |
| `out*` | stdout unasserted (may not combine with `out`/`out~`) |
| `err` / `err~` / `err*` | same three forms for stderr |
| `exit <n>` / `exit any` | expected exit code |
| `prompt~ <substr>` | a prompt must be pending and its question contain the substring |
| `choices <a,b,c>` | the pending prompt's choice list, exactly. Items are verbatim between commas (no trimming — `choices a, b` means `" b"`); a choice containing a comma is inexpressible |

**Defaults are strict** — a step with no expectation directives asserts: empty stdout,
empty stderr, exit 0, **no pending prompt**. Every deviation must be stated. The
no-pending default is load-bearing: it catches commands that unexpectedly pause.

Output with no trailing newline can only be matched with `out~`.

### Variables

`${TMP}` — the per-scenario sandbox directory (a unique host temp dir natively; `/tmp/w`
on the agent's fresh, isolated VFS). Substituted in payloads AND expectation text. The
harness runs an injected `mkdir -p ${TMP}` as step 0 (a fresh agent has no `/tmp`).
Confine all file activity to `${TMP}` — natively this is the real filesystem.

## Conventions

- Exit-code contract for the pause protocol: answered → 0, aborted → 130, authorization
  denied → 5, answer outside `--choices` → 1 with the prompt still pending.
- The uu coreutils fork emits raw fluent keys as error text (e.g.
  `ls-error-cannot-access-no-such-file`) prefixed by the **process binary name** — assert
  on stable fragments like `cannot-access`, never the prefix or the full message.
- Prefer exact `out` lines; use `out~` where output legitimately varies (uu number
  padding, error prefixes, host-dependent text).
- A scenario that pins a **known divergence** gets `@only` for the working target plus a
  comment naming the mechanism and what would close it (see
  `pipelines-uu-consumers.clank`).

## Example

```
# What this scenario pins, in one line.

run prompt-user "Deploy?" --choices staging,production
out~ Deploy?
prompt~ Deploy?
choices staging,production

answer maybe
err~ not one of
exit 1
prompt~ Deploy?

answer staging
out staging
```
