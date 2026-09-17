---
title: "Golem 1.6 tools-as-commands: fileable upstream drafts"
date: 2026-09-17
author: agent
---

# Golem 1.6 tools-as-commands: fileable upstream drafts

Source-inspected baseline: [main at 541300b4a](https://github.com/golemcloud/golem/commit/541300b4a104e4f70838690b525527e3fc26a664).
Upstream integration tests were not rerun. Linear status and duplicates are unverified.
These are local drafts; no issues or PR comments have been submitted.

Tool-to-tool execution and shared owner files already have upstream regression coverage in
[tool_streaming.rs](https://github.com/golemcloud/golem/blob/541300b4a104e4f70838690b525527e3fc26a664/golem-worker-executor/tests/tool_streaming.rs#L1535).
Binding grants landed in [#3842](https://github.com/golemcloud/golem/pull/3842).
Named errors are available. These capabilities need local integration, rather than feature tickets.

## What we are building

We are building a reusable `bash-tool` component that any Golem agent can bind. Within that shell,
the owner's other bound tools become commands. The shell owns discovery/help, metadata-driven
argv parsing, canonical input construction, pipes/substitution, command policy, audit, and prompt
continuation. `bash-golem` owns the SDK adapter; Clank embeds that same adapter and adds its wrapper.
Golem already supplies nested tool execution and owner filesystem sharing.

The local implementation has passed 27 live checks, including streamed pipelines, substitution,
named error exit codes, confirmation/denial, shared files, carried shell state, and crash recovery.
Those checks used the configured development SDK, not upstream main. Provider stderr and optional
Rust argument wire compatibility remain upstream gaps; local parser/RPC diagnostics already have
shell stderr. Release packaging and integration with the pending external client remain work.
Release acceptance also passed 27 checks; see [local validation](tools-as-commands-validation.md)
for test evidence and the cold/steady measurements.

| Tracking item | Priority | Where the work belongs | Filing action |
|---|---|---|---|
| Ship composable bash-tool for Golem 1.6 | P1 | Shell/adapter in clank; upstream release coordination | New umbrella |
| Fix optional canonical argument encoding | P1 | Rust SDK macros | New bug after duplicate check |
| Add independent tool stderr | P1 | WIT, executor, SDKs, native tools | Expand GOL-594 |
| Hide internal command subtrees | P2 | Rust SDK registration/discovery | New SDK issue |
| Document/test stdout regeneration | P2 | Runtime tests and authoring docs | New docs/test issue |
| Supported blocking completion | P2 | Rust SDK reflection | Comment on #3874 |
| External shell owner-context coverage | P1 for external client | External invocation API | Comment on #3897 |
| Publish filesystem requirements | Later | Capability/release metadata | Optional packaging issue |

## P1 — Ship a reusable bash-tool with tools as commands for Golem 1.6

**Area:** 1.6 release integration / tools ecosystem. **Action:** new umbrella tracking issue.

**Objective:** any agent can bind `bash-tool`, execute bash, and invoke that owner's other bound
tools as commands. Clank supplies the wrapper around this reusable shell. Track shell implementation
in the clank repository and link the upstream dependencies below; the SDK/runtime tickets remain
owned upstream.

**Delivery checklist:**

- [ ] Publish a bindable shell component independently of the Clank wrapper.
- [ ] Project only the owner's bound tools into discovery, help, and metadata-driven argv parsing.
- [ ] Construct canonical input using authoritative SDK schemas and dispatch through nested tool RPC.
- [ ] Support stdin/stdout pipes and command substitution, preserving declared error exit codes.
- [ ] Carry provider stderr independently through shell redirection (GOL-594 dependency).
- [ ] Support present/absent optional arguments once the canonical encoding fix lands.
- [ ] Enforce command-body authorization and audit; deny hidden calls requiring confirmation.
- [ ] Carry shell state and answer/abort prompts across fresh tool invocations.
- [ ] Verify shared owner files and regenerated output after crash recovery.
- [ ] Refuse unsupported secret arguments and background jobs explicitly.
- [ ] Validate the existing-owner external client path when #3897 lands.
- [ ] Record cold and steady-state release latency before deciding on instance caching.

**Release acceptance:** an agent binds bash plus a filesystem-capable sibling; a bash script discovers
that sibling, invokes it through a pipe/substitution, redirects stdout/stderr separately, handles a
named failure, confirms or denies a mutating command, and recovers after a simulated crash with
consistent output and shared files. Bound names that collide with compiled commands have documented
precedence. A minimal caller proves this without requiring Clank-specific features.

**Dependencies:** the optional argument bug and GOL-594 are 1.6 requirements. Follow #3873/#3874
for constructor/reflection support and #3897 for the external caller. Internal subtree publication
and filesystem requirement metadata are improvements rather than blockers of the initial shell.

## P1 — Fix canonical input schemas for optional Rust tool arguments

**Area:** Rust tool SDK / canonical input model. **Action:** new bug, after duplicate check.

An optional Rust argument can publish a field of type T while generated dispatch decodes Option<T>.
Sending T passes host validation and fails guest decoding; sending option<T> fails host validation.
A metadata-driven caller cannot produce a valid invocation or represent absence.

[Descriptor generation](https://github.com/golemcloud/golem/blob/541300b4a104e4f70838690b525527e3fc26a664/golem-rust-macro/src/tool/descriptor.rs#L1125)
unwraps Option; [generated decoding](https://github.com/golemcloud/golem/blob/541300b4a104e4f70838690b525527e3fc26a664/golem-rust-macro/src/tool/definition.rs#L689)
uses the original Rust type.

**Request:** make canonical fields and generated decoding agree. Publish option<T> for an optional
scalar; retain CLI requiredness separately. Specify absence explicitly.

**Acceptance:**

- Optional string and record arguments invoke successfully when present and absent.
- Supported optional positionals use the same contract.
- Discovery → canonical model → host validation → guest decoding round-trips Some and None.
- Existing bool/count flags and repeatable list/map behavior remains covered.

## P1 — Carry independent, caller-readable tool stderr

**Area:** tool WIT / attachments / supported SDKs / native tools.
**Action:** expand **GOL-594**, identified in the handoff; do not create a duplicate.

The [current contract](https://github.com/golemcloud/golem/blob/541300b4a104e4f70838690b525527e3fc26a664/golem-common/wit/deps/golem-tool/common.wit)
has stdin and stdout but no independent stderr. A shell cannot project diagnostics uniformly into
2>, 2>&1, or 2>/dev/null. stdout diagnostics contaminate pipes; structured result fields are not a
uniform stream.

**Request:** add an optional stderr attachment across metadata, guest/host invocation, middleware,
native-tool APIs, and supported SDKs. Preserve both channels through completion, cancellation,
and recovery.

**Acceptance:**

- Distinct stdout/stderr bytes reach the caller independently.
- A filesystem-capable outer tool receives both from an inner tool.
- Both readers drain concurrently with the terminal wait without deadlock.
- Limits and stream failures apply independently to each channel.
- Tools with no declared stderr remain compatible.
- Document protocol versions and required rebuilds.

## P2 — Keep implementation-only subtrees out of public tool discovery

**Area:** Rust tool registration / discovery / deployment validation. **Action:** new SDK issue.

Subtree dispatch forwards through the guest registry, so registering a child implementation also
publishes it independently. The echo fixture consequently declares tree as a separate tool.
[Discovery currently returns every registry member](https://github.com/golemcloud/golem/blob/541300b4a104e4f70838690b525527e3fc26a664/sdks/rust/golem-rust/src/agentic/tool_registry.rs#L70).

**Request:** separate public exports from internal subtree implementations. Parent dispatch retains
internal access. Public discovery, binding lookup, and direct invocation expose only public exports.
Explicit independent publication of a child remains possible.

**Acceptance:**

- A parent with an internal subtree deploys with only the parent declared.
- Subcommands, aliases, and inherited globals still dispatch.
- Internal children are absent from public discovery and cannot be invoked directly.
- Explicitly public children remain discoverable and bindable.
- Tests cover both reuse and independent publication.

## P2 — Document and pin regenerated stdout during tool recovery

**Area:** tool authoring docs / WIT docs / oplog rendering / replay tests. **Action:** new docs/test issue.

Completed tool bodies reexecute during recovery. Structured terminals are checked for divergence;
stdout is regenerated outside that terminal, without a comparison against recorded bytes.
[The current CLI wording](https://github.com/golemcloud/golem/blob/541300b4a104e4f70838690b525527e3fc26a664/cli/golem-cli/src/model/agent/oplog.rs#L93)
says “none (live attachment only)” and leaves this consequence unclear.

**Request:** document regenerated output and its determinism requirements for pipelines and
substitutions, update oplog wording, and pin the behavior with recovery tests.

**Acceptance:**

- Tool authoring and host interface docs explain regeneration and structured-result comparison.
- A recovering caller receives regenerated output.
- Nested tool recovery verifies both output delivery and shared-file effects.
- Tests distinguish stdout behavior from structured-result divergence detection.

Digest-based stdout divergence detection belongs in a separate follow-up.

## Later — Publish filesystem requirements separately from access grants

**Area:** capability metadata / releases / deployment validation. **Action:** new packaging issue.

Binding-level grants work, but tool publishers cannot say that operation requires filesystem access.
A consumer can omit the grant and discover it through runtime errors.
[Tool declarations](https://github.com/golemcloud/golem/blob/541300b4a104e4f70838690b525527e3fc26a664/cli/golem-cli/src/model/app_raw/mod.rs#L204)
do not carry that requirement.

**Request:** publish a requirement that survives release selection, validate against effective
binding access, and name the tool/binding in deployment diagnostics. Consumers still control grants.

**Acceptance:**

- Requirements survive publication and release consumption.
- Required access with an effective deny fails deployment with an actionable diagnostic.
- An explicitly allowed binding succeeds without a dummy initial file.
- Requirement metadata never grants access itself.

Explicit binding grants already support the shell implementation; this is not a 1.6 blocker.

## Draft comment on #3874 — supported blocking completion

[Rust reflection PR](https://github.com/golemcloud/golem/pull/3874)

> A synchronous shell builtin needs to complete a reflected tool invocation using the SDK's async
> runtime, concurrently pumping input and draining output. Could golem-rust expose a supported
> blocking adapter or its own block_on to reusable libraries without requiring guest exports?
> The shared bash-golem adapter currently pins the SDK's wit-bindgen fork directly. Please cover
> nested use inside an already-running agent or tool invocation.

This is an SDK convenience/support boundary, rather than a synchronous host RPC requirement.
Reuse the PR's schema and invocation APIs once its stack lands. Its fallible ToolRpc::create
depends on [#3873](https://github.com/golemcloud/golem/pull/3873); do not file another constructor issue.

## Draft comment on #3897 — preserve the shell's owner-context case

[External tool invocation PR](https://github.com/golemcloud/golem/pull/3897)

> Please retain coverage for external invocation of an agent's bound shell tool: the shell discovers
> sibling bindings, invokes a filesystem-capable sibling, and observes the same owner files.
> Include retry/idempotency and recovery coverage, preserving named custom errors.

This requests coverage of the existing-agent queue/context proposal, rather than a new route.
Coordinate its golem:core/types.tool-rpc-error change with reflection's constructor change.

## Recommended order

Optional argument encoding and stderr are 1.6 requirements. Implement the shell projection against
existing bindings now. Follow existing constructor/reflection/external-client work. Address internal
subtree publication and the replay contract next; filesystem packaging and warm-instance caching
can follow measured release latency.
