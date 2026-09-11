# Replay safety — why `std::fs` append is unsafe and whole-file write is the fix

**The constraint.** On the durable Golem agent, `std::fs` append duplicates data under oplog replay,
because the agent's filesystem is **re-run guest code, not a restored snapshot**. Golem recovers a
crashed or migrated agent by replaying its oplog from the start: re-running `new` and then every past
`eval`/method call, in order, on a fresh instance. Any raw local side effect that isn't itself part of
the durability model — and a plain `std::fs` write is exactly that — re-executes verbatim on every
replay. An `OpenOptions::append(true)` write appends *again* each time the line that produced it is
replayed, so the file grows a duplicate copy of everything written since the last actual crash, every
time recovery runs. A **whole-file** `std::fs::write` has no such failure mode: replaying it just
overwrites the file with the same bytes it would already contain, so it is naturally idempotent under
replay regardless of how many times that line re-executes.

## Why this is the shape it is

Golem's durability model works by recording durable function calls (host calls — network, time,
randomness, and so on) to an oplog and replaying their **recorded results** on recovery rather than
re-executing them live. That is what makes non-deterministic operations replay-safe. Plain local
filesystem I/O is not wrapped by that mechanism the same way: it is guest-side code with no host call
to intercept, so nothing records or skips it — it simply runs again, identically, as part of re-running
the guest code that produced it the first time. This is stated directly in
[`crates/clank-embed/src/log_sink.rs`](../../crates/clank-embed/src/log_sink.rs)'s module doc: *"a raw
`std::fs` append is a local side effect Golem neither records nor skips, so a crash-then-recovery would
re-run the append and duplicate the line."*

The mitigation the durability model recommends for a raw side effect like this is to make it
**idempotent** — safe to re-execute any number of times with the same net effect — rather than to try
to detect or suppress the replay. A whole-file rewrite is idempotent by construction: `std::fs::write`
always produces the *complete, current* content, so re-running it on replay converges to the same file
rather than accumulating a duplicate.

**Why not just check whether the current execution is a live invocation vs. a replay, and skip the
write on replay?** `golem_rust::durability::Durability::is_live()` *is* public, but the only public
path to construct a `Durability` opens a durable-function region — calling it just to peek at
liveness, without actually wrapping a durable operation, would leave that region dangling. The cheap
raw accessor that reports execution state (`current_durable_execution_state`) is `pub(crate)` inside
the SDK, not exposed to guest code at all. So gating the write on liveness isn't available as a clean
option; the idempotent-rewrite mitigation is the one actually reachable from here. (See
[[golem-fs-append-replay-unsafe]] and the `golem-rust` SDK for the API shape.)

## The two places this governs

**The durable log sink** —
[`crates/clank-embed/src/log_sink.rs`](../../crates/clank-embed/src/log_sink.rs), `DurableLogSink`.
clank's `/var/log/{shell,http,mcp,ops}.log` files are append-shaped by nature (one line per event,
accumulated over the agent's entire lifetime), which is exactly the shape a naive implementation would
reach for `OpenOptions::append`. Instead, `DurableLogSink` keeps a per-file **in-memory** buffer
(`RefCell<HashMap<&'static str, String>>`) and, on every `append` call, pushes the new line onto that
buffer and then rewrites the *whole file* from it via `std::fs::write`. This is replay-safe for a
specific reason beyond just "it's a whole-file write": the in-memory buffer is **never seeded from the
on-disk file** — if it were, a replay would read back the file's already-written content, append the
replayed line to *that*, and reproduce the duplication the whole-file rewrite was supposed to prevent.
Because the buffer is pure in-memory state, and recovery reconstructs it by replaying the exact same
sequence of `append` calls that built it the first time, the buffer converges to bit-identical content
on every replay — "exactly like the transcript and process table," in the module's own words — and the
whole-file `std::fs::write` on top of that identical content is a true no-op in effect. The buffer is
bounded to a rolling tail (`MAX_LOG_BYTES`, shared with the native rotation constant) so the per-write
cost stays bounded; because the bound is applied deterministically, replay reproduces the identical
truncated tail too, so bounding doesn't reintroduce non-determinism.

**The grease payload store** —
[`crates/clank-core/src/session/grease.rs`](../../crates/clank-core/src/session/grease.rs),
`persist_package`. Installing a grease package writes its payload to
`<store>/<name>/<kind>.json` with a single `std::fs::write` of the complete serialized payload — never
an append. This is a simpler case than the log sink: there is no incremental in-memory accumulation to
protect, because one `grease install` produces one complete JSON value in one write. It is idempotent
under replay for a more direct reason — the payload bytes come from an outbound HTTP fetch that *is*
wrapped by Golem's durability (the recorded response replays identically), so replaying the same
install line reproduces the same payload and writes the same bytes to the same path. `persist_package`
additionally refuses to write a payload that serialized to an empty string, rather than let a broken,
zero-byte package "install successfully" and fail to parse on the next boot — nothing half-lands.

## The general rule this implies

Any code that persists state on the agent should default to whole-file `std::fs::write` of a value
the guest code can reconstruct deterministically (in memory, or from a durably-recorded host call),
and treat `OpenOptions::append` — or any other operation whose correctness depends on how much has
already run — as a replay hazard unless it is proven otherwise. The `Transcript` in
[`lib.rs`](../../crates/clank-core/src/lib.rs) follows the same discipline for a different reason (it
is never disk-backed at all — it is pure in-memory state, rebuilt by replaying the same `record_command`/
`record_output` calls), which is why the log sink's module doc points at it as the precedent.
