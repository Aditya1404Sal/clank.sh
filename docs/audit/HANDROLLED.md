# HANDROLLED.md — what clank implements itself, and how ready it is

Audience: anyone deciding whether to trust a clank-implemented tool with real work, or deciding
whether the next gap should be fixed, documented, or delegated to a crate.

Every claim below was re-verified against the tree at `0c29f7d`. Where an earlier audit's grade no
longer holds, the change is called out — several improved during the reliability and security pass
that immediately preceded this document, and publishing the old grades would have been wrong.

---

## The headline

**There is no hand-rolled cryptography.** SHA-256 is `sha2`, Ed25519 is `ed25519-dalek` (verify-only,
via `verify_strict`, which rejects malleable and small-order keys), base64 is `base64`. The one
hand-rolled algorithm anywhere near the security boundary is the RFC-6962 Merkle inclusion proof, and
it is correct — traced against RFC 9162 §2.1.3.2 and tested against an **independent reference
oracle** (a separately-written recursive MTH implementation, not the code under test) across balanced
and unbalanced tree sizes.

**The delegation record is good, and the hand-rolled surface is residue, not NIH.** jaq is the whole
of `jq`. BurntSushi's `grep` is the whole of `grep`. `similar` is `diff`, `diffy` is `patch`, `infer`
is `file`, `walkdir` is the traversal under `find`, `iri-string` is RFC-3986 reference resolution,
uutils is 19 coreutils. What clank writes itself is overwhelmingly the set of things with **no
wasm-clean crate**: `awk` (every Rust awk hard-requires a cranelift or LLVM JIT), `find`'s predicate
layer (findutils is bin-only with C deps that don't build for wasip2), `stat` (uutils' formatting core
is `MetadataExt`-unix throughout).

**And the property that actually matters is honest failure.** These are subsets. A subset that errors
on what it does not implement is safe to hand a model; a subset that silently does the wrong thing is
not. On this axis clank is consistently good, and that is the single most important finding here —
`awk` errors with *"call to unsupported function"* rather than returning an empty string, `find`
errors with *"unknown predicate"* rather than ignoring it, `sed -i` errors with *"redirect to a new
file instead"* rather than pretending, and unknown flags in `wcurl`/`waget` are hard errors rather
than silent no-ops.

---

## How these grades are assigned

**Not** "is this a drop-in replacement for the GNU tool" — none of them are, and that was never the
goal. The question is:

> **Is this fit for the use clank puts it to — driven by an LLM that cannot see the terminal — and does
> it fail honestly when it hits its edge?**

Under that rubric a `find` with six predicates can grade better than a feature-rich tool that guesses,
because a model that gets *"unknown predicate: -size"* can adapt, and a model that gets a silently
filtered result cannot.

| Grade | Means |
|---|---|
| **A** | Fit for purpose, honest at its edges, tested. No known gap that would surprise a caller. |
| **B** | Fit for purpose; gaps exist but are documented and fail loudly. |
| **C** | Usable subset; the common paths work and the edges error honestly. Real gaps a caller will hit. |
| **D** | Works for the demo path; a caller will hit something that is wrong rather than absent. |
| **F** | Should not be relied on. |

---

## Scorecard

| Component | Grade | Δ since the last audit | One-line reason |
|---|---|---|---|
| Crypto **use** (grease) | **A−** | ↑ from B− | Nothing hand-rolled; `verify_strict`; keys validated at add time; RFC-8032 vectors |
| RFC-6962 verifier | **A** | = | Correct, domain-separated, independent-oracle tested, rejects six tamper classes |
| Delegation posture | **A−** | ↑ from B | jaq/grep/similar/diffy/infer/walkdir/iri-string/uutils — the hard parts are all delegated |
| CI | **B** | ↑↑ from **F** | Was never running. Now 3 jobs; unit + conformance + fmt + clippy(×2 targets) + audit + deny |
| `stat` | **B−** | = | Prints `-` for what wasip2 cannot know rather than inventing it |
| `whttp` | **B−** | ↑ from C | Timeouts both targets, redirect cap, credential stripping, body cap. One residual on wasm |
| `xargs` | **C+** | ↑ from D+ | Small but correct — and the authz bypass it enabled is now closed |
| `wcurl` | **C** | ↑ from D+ | ~20 real flags; unknown flags error; `-v` no longer leaks request credentials |
| `sed` (in `texttools.rs`) | **C** | = | `s/d/p/q` + addresses + ranges. No hold space; `-i` errors honestly |
| `MCP client` | **C** | ↑ from C− | 10 methods, Streamable HTTP, bounded pagination. No server→client stream |
| `find` | **C** | = | Six predicates, AND-only, honest "unknown predicate" |
| `waget` | **C−** | ↑ from D | 11 real flags + 5 explicit no-ops; `--no-check-certificate` safely reinterpreted |
| `awk` | **C−** | = | Large subset, but no arrays, no control flow, no functions — all honest parse errors |
| YAML frontmatter | **C−** | = | Deliberate tiny subset; hard-errors on anything outside it |
| `authz` | **C−** | ↑ from D | `xargs` closed, model path default-deny. **Human path still has real bypasses** |
| Transparency log *as assurance* | **D** | = | Single-leaf tree, unsigned index entry — the proof is tautological |
| Transparency log *as a claim* | **A** | ↑↑ from D | Output now says "registry-asserted root". The overclaim is gone |

That last pair is the most important row in the table, and the reason it is split in two.

---

## The transparency log, split in two

**The assurance is still worth nothing, and that has not changed.** `verify_signature` signs the
**payload body only**; the index entry carrying `sha256`, `sig`, `signer` and the whole `log` object is
covered by no signature. The authoring tool emits a **single-leaf tree per package**. So the proof
reduces to

```
sha256(0x00 ‖ sha256_hex(payload)) == root
```

…where `root` arrived from the same party as `payload`. Anyone who controls the index can forge it
trivially.

**What changed is that clank no longer claims otherwise.** Install output and `grease info` used to
say *"in log"* and *"transparency log @0"* — language that means something specific and unearned.
They now say **"log proof (registry-asserted root)"**, and the reasoning is written at the source:

> *…trivially forgeable by anyone who controls the index.* — `grease/state.rs:668-683`

That is why the two rows grade so differently. The security property is D and always was; the
**honesty** about it is now A, and honesty is the part that was actually broken — a caller reading
"in log" would have made a decision the system could not support.

**The verifier is kept deliberately.** It is correct and proven; when a genuine append-only log with a
signed tree head exists, the hard part is already built. Grading the verifier D for the weakness of
the thing it is pointed at would be the wrong read.

---

## `authz` — the one component whose grade is held down by something unfixed

Two enforcement points with **different strictness**, which is the design decision that makes the
grade defensible at C− rather than D.

**Model path — fails closed.** `session/ask.rs` layers three guards on top of the ordinary decision:
`ask` cannot call itself; command substitution (`` ` ``, `$(`, `<(`, `>(`) is refused outright; and a
per-segment scope gate **default-denies any unrecognized leading word**, with a small
`MODEL_SAFE_BUILTINS` allowlist (`echo pwd test [ true false :`). So `eval`, `for`, `if`, and
variable-indirection are all blocked for the model — not by special-casing each, but as a consequence
of default-deny, which is the right shape.

**Human path — still permissive, by decision.** An unknown leading word maps to `Allow`, so these
remain open for a person at the keyboard:

| Bypass | Status |
|---|---|
| `echo $(rm -rf /x)` and the backtick form | open — `$(…)` is one word token; splitting it needs recursive substitution parsing |
| `eval "rm -rf /x"` | open — `eval` has no manifest, re-enters Brush without returning through `eval_line` |
| `for f in *; do rm $f; done`, `if true; then rm …; fi` | open — segments lead with `do`/`then`, which have no manifest |
| `X=rm; $X -rf /x` | open — `command_words` dequotes but does not expand |
| `command rm x` | open — only `xargs` is seen through |
| malformed (untokenizable) line | fails **open** — tracked as AUDIT P2-2, deferred |
| `find . \| xargs rm -rf` | **CLOSED** — `see_through_wrapper` resolves through the wrapper, recursively |

The `xargs` fix is the one that mattered most, because it was the only bypass reachable from the
*model* path too, and because it was silent: `echo /path \| xargs rm` deleted the file at exit 0 while
bare `rm` (sudo-only) pauses.

The human-path gaps are a deliberate scoping call, not an oversight — closing them means inverting a
default that the conformance corpus pins for human use, and the population that motivated this work
(a model using the shell in ways a person doesn't) is already covered. But the grade should reflect
that a documented bypass is still a bypass, hence C− rather than B.

> `tools/xargs.rs`'s module doc used to say the re-entered lines "are not re-gated", which stopped
> being true when `see_through_wrapper` landed. Corrected in the commit that added this file — the
> lines are indeed not gated *on the way out*, but they are gated *before* they get there, which is
> the part that matters and the part the comment omitted.

---

## `whttp` — the one remaining asymmetry

Four properties, three of them clean on both targets:

| Property | Native | wasm (durable agent) |
|---|---|---|
| Connect + request timeout | ✅ 30 s / 5 min | ✅ — but the request budget is a **first-byte** timeout, the closest WASI-HTTP primitive to a deadline |
| Redirect cap | ✅ 50 default | ✅ |
| Credential stripping on cross-origin redirect | ✅ | ✅ — `same_origin` **fails closed**, stripping when either URL won't parse |
| Body cap (64 MiB default) | ✅ pre-check **plus streaming bound** via `chunk()` | ⚠️ pre-check only; a chunked response with no `Content-Length` is buffered before the check fires |

**The residual lands on the wrong target** — the durable agent has fixed linear memory, so an
oversized buffer there is a trap rather than an error. It is not hidden: the reason is written at the
site (`wstd`'s `Body::contents()` is all-or-nothing; closing it means depending on `http-body-util`
pinned to `wstd`'s `http_body` version). This is the highest-value remaining item in `whttp`.

The native streaming property has the test that a post-hoc check cannot pass: a server that sends **no
`Content-Length`** and streams 8 MiB against a 256 KiB cap.

---

## Where a crate was available and wasn't used

Three cases, and they are not equivalent.

**`wcurl`'s hand-rolled base64** (`parse.rs:271-293`) — clank-core already depends on `base64`, so at
first glance this is unjustified. It isn't: `wcurl` is a deliberately dependency-free wasm-facing
crate, and adding `base64` there to encode one Basic-auth header would be the tail wagging the dog.
The stated reason holds. It is 22 lines of table-driven encoding with no decode path, which is the
low-risk direction.

**YAML frontmatter** (`grease/pkg.rs:158-213`) — the stated reason is that clank-core keeps its
dependency set pure-Rust and wasm-clean. That was truer when written than it is now: pure-Rust YAML
crates exist. The counter-argument is that the parser covers exactly three scalar keys and one list
shape, hard-errors on everything else, and pulling in a full YAML implementation would *widen* the
accepted grammar — accepting anchors, merge keys and flow style in a file that a registry supplies is
a larger attack surface, not a smaller one. **Verdict: keep it, but the justification should be
"deliberately narrow grammar", not "no crate exists".**

**`clap` for `wcurl`/`waget`** — already a direct dependency (used by `xargs`). These two hand-roll
their parsers to mirror curl's and wget's actual flag grammar, which clap does not naturally express
(short clustering with embedded values, `-Ffs`, `-nv` staying whole). Defensible.

---

## Component detail

### `awk` — `tools/awk.rs`, 1379 lines

A real lexer, recursive-descent parser and tree-walking evaluator. No Rust awk crate compiles for
`wasm32-wasip2` — frawk and zawk both hard-require a JIT backend.

**Has:** `-F` (including the inline `-Ffs` form) and `-v`; `$0..$NF`; `NR NF FS OFS`; `pattern {
action }` with `/regex/`; `BEGIN`/`END`; `print`/`printf`; arithmetic; string concatenation by
juxtaposition; comparisons with awk's numeric-vs-string duck typing; `~`/`!~`; `&& || !`; compound
assignment; `++`/`--`; `length`. Parse depth bounded at 200 (a stack overflow **aborts**, which on the
agent cannot be caught at all).

**Lacks:** arrays, `for (i in x)`, all control flow (`if`/`while`/`for`/`do`), user functions,
`getline`, field assignment, `-f progfile`, and every string builtin (`substr`, `split`, `gsub`,
`sub`, `match`, `sprintf`, `toupper`, `tolower`) plus `RS ORS FNR FILENAME SUBSEP ARGV ENVIRON`.

**Why C− and not lower:** every one of those is a **parse error naming the construct**, pinned by a
test called `unsupported_surface_is_honest`. A model gets told, and can fall back to `sed` or `jq`.

### `sed` — inside `tools/texttools.rs` (there is no `sed.rs`)

**Has:** `-n`, `-e` (repeatable), bare script, multiple FILEs concatenated into one stream so `$` and
line numbers span them; addresses `N`, `$`, `/re/` and two-address ranges with stateful activation;
`s///` with `g`/`i`/occurrence-number, `&` and `\1` backrefs; `d`, `p`, `q`.

**Lacks:** the entire hold space (`h H g G x`), `a i c y r w n N D P`, labels and `b`/`t`, `{}`
blocks, `!` negation, `-f`, `-r`/`-E`, `-z`, `0,/re/` and `addr,+N` forms.

`-i` is an explicit error telling you to redirect instead — the right call, since in-place editing on
a durable agent's replayed filesystem is a correctness hazard, not just an unimplemented flag.

### `find` — `tools/find.rs`

Traversal is `walkdir`; the predicate layer is clank's. **Has:** `-name`/`-iname`, `-path`, `-type
f|d`, `-maxdepth`, `-mindepth`, `-print`, multiple start paths, and glob→anchored-regex translation.
Also serves the virtual `/bin`, `/proc/<pid>`, `/proc/clank` namespaces.

**Lacks:** boolean composition entirely — no `-o`, `!`, or `(...)`, so predicates only AND. No
`-size`, `-newer`, `-mtime`, `-perm`, `-user`, `-empty`, `-prune`, `-regex`, `-print0`, `-xdev`. No
`-exec` — deliberate, since clank cannot spawn; `find … | xargs cmd` is the supported composition, and
that composition is now correctly authz-gated.

### `xargs` — `tools/xargs.rs`

**Has:** `-n`, `-I`, `-d` (single char, with `\n`/`\t`/`\0` recognized), default `echo`, whitespace
tokenization, and — importantly — `shell_quote` on every token before re-entry, so filenames with
spaces survive. GNU's `--no-run-if-empty` is the default here rather than a flag.

**Lacks:** `-0`, `-P`, `-a`, `-p`, `-s`, `-t`, `-E`, `-L`.

Upgraded from D+ chiefly because the thing that made it dangerous — laundering a `SudoOnly` command
past the gate — is fixed at the gate rather than inside `xargs`, which is the right altitude.

### `stat` — `tools/stat.rs`

wasip2 has no `stat(2)`: no inode, uid/gid, mode bits, block counts. Rather than invent them, every
unknowable field prints `-`. **Has:** `-L`, `-c`/`--format`, and `%n %s %F %y %Y %x %X %w %W %%` with
`\n`/`\t` escapes; unknown directives render `?` (GNU-like). Serves the virtual namespaces so they
don't leak "No such file".

B− is the highest grade of any tool here, and it is earned by *refusing to guess*.

### MCP client — `mcp/client.rs`

Streamable HTTP, protocol `2025-03-26`. Ten methods: `initialize`, `notifications/initialized`,
`tools/list`, `tools/call`, `prompts/list`, `prompts/get`, `resources/list`,
`resources/templates/list`, `resources/read`, `resources/subscribe`, plus session close via HTTP
DELETE. All four list methods paginate via `nextCursor`, bounded by `MAX_TOOL_PAGES` — and exceeding
that is now an **error** rather than a silently truncated list that grease then persisted forever.

Both response encodings are handled (JSON and SSE-framed). The transport is a trait seam rather than a
cfg branch, because MCP needs response *headers* (`Mcp-Session-Id`) and scriptable multi-step fakes.

**Lacks:** any standalone GET — only POST and DELETE are ever issued — so there is no server→client
stream, no resumability, and **`resources/subscribe` is sent but nothing can ever receive the
resulting notifications**. That specific mismatch is the most misleading gap in the client. Also no
OAuth, no stdio transport, no sampling/roots/elicitation/completion/ping/progress.

### CI — the biggest single improvement

It was **F** because it did not run: the hygiene job passed `-p clank-embed`, a crate that exists only
on `main-rc-1`, so cargo exited before clippy — and `cargo audit` and `cargo deny` never executed at
all. Meanwhile the native job ran roughly 150 of 579 tests, because `cargo test -p clank-core` was
simply absent.

Now three jobs: **native** (library unit tests across six crates, conformance lib, native conformance
tier), **hygiene** (`fmt --check`, clippy on native crates *and* wasm guest crates, `cargo audit`,
`cargo deny check`), and **golem** (the durable-target conformance tier, on schedule and manual
dispatch, against a pinned golem binary).

B rather than A because there is **no `pull_request` trigger** (deliberate today — same-repo PRs would
double-run — but it means a fork PR gets no CI), and because the golem tier only runs nightly and on
demand, so durable-target regressions can land and sit for up to a day.

---

## If you fix three things

1. **The wasm chunked-body gap in `whttp`.** It is the only remaining unbounded allocation on the
   target where unbounded allocation is fatal.
2. **`resources/subscribe` in the MCP client** — either implement the GET stream or stop sending the
   subscribe, because right now it advertises a capability that cannot function.
3. **The `authz` fail-open on untokenizable input.** Every other human-path bypass requires the user
   to type something they meant; this one triggers on a typo.
