# Wall C — why async work runs at the Session layer, one stage per line

**The constraint.** [`brush_core::builtins::SimpleCommand::execute`](../../crates/clank-core/src/session/mod.rs)
is synchronous by signature, on both the native and wasm targets. clank drives the whole Brush
engine — parsing, expansion, every builtin — from inside that synchronous call, one level under a
nested tokio runtime clank owns (`rt.block_on`). A WASI-HTTP future polled by that nested runtime is
never woken, because nothing running under `execute` performs the component-model wait the Golem
host needs to resume it. So HTTP cannot complete inside a Brush builtin on the agent. Since curl,
`ask`'s LLM call, MCP, and Golem agent invocation are all outbound HTTP (or, for agent invocation,
an `.await`ed wRPC call with the same shape), all of them are instead **awaited directly at the
`Session` layer**, one level under the executor that actually drives the agent's exported method —
never through `execute`. The practical consequence is that this async work can only happen as a
**top-level Session-layer stage**: once, at the point `Session::run_command` dispatches a line, not
nested inside `$(...)`, a pipeline stage Brush itself drives, `xargs`, or `eval`.

This page is the canonical explanation; the codebase calls it "the Wall C shape" or just "Wall C" in
at least eight places (module headers on
[`builtins/http.rs`](../../crates/clank-core/src/builtins/http.rs),
[`builtins/interceptstub.rs`](../../crates/clank-core/src/builtins/interceptstub.rs), and
[`ai/ask.rs`](../../crates/clank-core/src/ai/ask.rs); inline comments in
[`session/mod.rs`](../../crates/clank-core/src/session/mod.rs) and
[`lib.rs`](../../crates/clank-core/src/lib.rs); [`docs/WASM_CHANGES.md`](../WASM_CHANGES.md);
[`docs/ONBOARDING.md`](../ONBOARDING.md)) without any one of them spelling out the whole shape.

## Why it's this shape

- **`SimpleCommand::execute` cannot be `async fn`.** It is a plain synchronous function in Brush's
  extension API (the trait clank implements to register its own builtins), on both targets — this
  isn't a wasm-only restriction. Nested inside it, clank still needs Brush's own internal async
  (pipelines, `spawn_blocking` for owned-shell builtins) to run, so `execute` drives it with an owned
  tokio runtime: current-thread on wasm (no threads there), the ambient multi-thread runtime on
  native. See the `session/mod.rs` module header and `docs/WASM_CHANGES.md` §3a.
- **A future needs its *driving* executor to perform a host-specific wake, not just any `poll`.**
  Inside a Golem component, WASI-HTTP integrates with the async runtime that `golem-rust`/wit-bindgen
  drives the agent's exported method with — the one that actually knows how to perform the
  component-model wait for a pending host call. clank's nested `rt.block_on` is a *different*,
  self-contained runtime one level further in. It will poll a WASI-HTTP future once, get "pending,"
  and then park on its own reactor — which has no way to know the host operation completed, because
  nothing running under it ever performs that wait. The future is technically alive, but nothing will
  ever wake it. (`docs/WASM_CHANGES.md` §3c states this as "the load-bearing dispatch rule.")
- **The fix is a placement rule, not a runtime fix.** There is no workaround that lets HTTP complete
  correctly inside `execute`'s nested runtime — the fix is to never put it there. `curl`, `wget`,
  `ask`, `mcp`, and `grease` are therefore **not** registered as ordinary Brush `SimpleCommand`
  builtins at all. They are recognized by [`Session::classify_command`](../../crates/clank-core/src/session/mod.rs)
  (and, for a few of them, earlier still by `classify_line`) and dispatched from
  [`Session::run_command`](../../crates/clank-core/src/session/mod.rs), which awaits their async work
  directly — one level under the Golem SDK's own executor, where the wait actually resolves. Its doc
  comment states the rule explicitly: *"`curl`/`wget` are dispatched here to their async HTTP crates,
  NOT through `execute`. This is load-bearing: `execute` runs Brush on clank's nested `rt.block_on`,
  and a WASI-HTTP future polled by that tokio runtime never gets woken... Awaiting `wcurl::run`/
  `waget::run` directly here keeps the HTTP one level under the SDK's own executor."* `ask` (its LLM
  call), `mcp`/`grease` (their HTTP), and Golem agent invocation follow the identical rule for the
  identical reason.
- **This makes the async work top-level-only by construction, not by choice.** `Session::run_command`
  is reached only once per line, at the point `eval_line` dispatches it — never from inside Brush's
  own pipeline/substitution machinery, which calls back into `execute` for each stage. So anything
  routed through `run_command`'s async dispatch can appear **at most once per line, as that line's own
  top-level command** — never nested inside a `$(...)`, a non-terminal pipeline stage, `xargs`, or
  `eval`, all of which stay inside Brush and therefore inside `execute`'s nested runtime.

## The consequences already in the code

**`curl`/`wget` work as a pipeline HEAD, nowhere else.** A bare `curl URL` and a curl-*headed*
pipeline (`curl -s URL | jq .x`) both work: [`builtins/http.rs`](../../crates/clank-core/src/builtins/http.rs)'s
`classify` recognizes the bare form and `split_http_head` recognizes the headed-pipeline form (byte-exact,
tokenizer-based, quote-aware). `Session::run_http_pipe` runs the head's HTTP at the Session layer, then
feeds the response bytes to the downstream Brush program (`execute_with_stdin`) as ordinary stdin — so
`curl -s URL | jq .x | head -1` composes normally, because only the *first* stage needed the Wall C
placement rule and everything after it is plain synchronous text processing. curl/wget **cannot** be
the pipeline tail, a middle stage, or appear after `&&`/`;`, because those positions run inside Brush's
own `execute`.

**`ask` works only as the pipeline TAIL, never the head or a middle stage.** The LLM call needs the
same Session-layer placement, but the direction is reversed from curl: `ask` must consume upstream
output, not produce it for a downstream Brush stage, so it has to be last. `Session::classify_line`
detects `cat x | ask "…"` via [`ai::ask::split_ask_tail`](../../crates/clank-core/src/ai/ask.rs) at
**Stage 4** of `eval_line_inner` (see [`docs/ONBOARDING.md`](../ONBOARDING.md) §4) and *pre-extracts*
the upstream: it runs the upstream stage through Brush first, captures its stdout, and then dispatches
the `ask` tail directly at the Session layer with those bytes attached as supplementary stdin — never
as a real Brush pipe. Only a literal `|` immediately before `ask` counts; `||`, `;`, and `&&` do not
make `ask` a pipe tail. `context summarize` has the identical top-level-only restriction and the
identical reason — it is also an LLM call — enforced in
[`Session::classify_line`](../../crates/clank-core/src/session/mod.rs) (the `ContextSummarize` route)
and, for the nested case, in [`lib.rs`](../../crates/clank-core/src/lib.rs)'s `apply_context`.

**`$(curl …)`, `xargs curl`, and any other nested placement get an honest-error stub, not a crash or
a silent no-op.** Before the Wall C fix, a curl/wget/ask/mcp/grease/golem/kill word reaching Brush
from inside `$(...)`, a pipeline's non-head stage, `xargs`, or `eval` fell through to Brush's normal
external-command path and died with a misleading "operation not supported on this platform." Instead,
[`builtins/interceptstub.rs`](../../crates/clank-core/src/builtins/interceptstub.rs) registers a real
Brush `SimpleCommand` under each of these names — `CurlStub`, `WgetStub`, `AskStub`, `KillStub`,
`McpStub`, `GreaseStub`, `GolemStub` — that only ever runs when Brush dispatches the name *directly*
(i.e., precisely the nested-context case, since the top-level case is always intercepted earlier and
never reaches Brush at all). Each stub prints a clear, specific message — for `curl`/`wget` it points
at the pipeline-head or `-o file` forms that *do* work; for `ask` it points at the pipeline-tail or
`ask "$(cat x)"` forms — and exits 1. `--help` is special-cased even here (via
[`helpshim::simple_builtin_with_help`](../../crates/clank-core/src/helpshim.rs)): a nested
`$(curl --help)` prints the real manifest help instead of the stub's not-usable-here error, because
help should never depend on where it's asked from. `kill` is registered alongside the HTTP/AI/package
commands for a related but distinct reason: it mutates `Session` state (the background-job map, the
pending-prompt slot) that a Brush builtin cannot reach, not because of an async-wake problem — grouped
here because it needs the same nested-context stub treatment.

**MCP resource reads and grease-agent RPC follow the same rule.** A top-level `cat
/mnt/mcp/<server>/<dynamic>` is served live at the Session layer (`CommandRoute::McpResourceRead`)
because the fetch is outbound HTTP that cannot run inside Brush's synchronous `cat`; the identical
`CommandRoute::McpTemplateLine` restriction applies to grease-installed MCP resource-template
executables. Golem agent invocation (`session/agent.rs`) is dispatched from `run_command` for the
same reason — it is an awaited wRPC call, not a spawned process, so it needs the same placement as
curl/ask/mcp/grease.

## A name reused for an adjacent, distinct problem

The fork rationale in [`docs/WASM_CHANGES.md`](../WASM_CHANGES.md) §1(b) also uses the name "Wall C"
for a **different** constraint that happens to be solved in the same forked Brush: wasip2 has no
`pipe(2)` and no thread pool, so the *upstream* Brush's OS-pipe-plus-`tokio::spawn` model for
pipelines and `$(...)` cannot run at all on the agent, independent of anything in this page. The fork
replaces it with an in-memory `OpenFile::Stream`-backed pipe run inline-sequentially. That fix is what
makes plain pipelines and command substitution work on wasm in the first place; this page's Wall C is
about a narrower thing — which of clank's *own* commands may participate in those pipelines, and only
at which position. See [`wasip2-constraints.md`](wasip2-constraints.md)'s "no `pipe(2)`" row for that
adjacent fix.
