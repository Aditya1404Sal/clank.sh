---
title: "Implement tools as commands for bash-tool"
date: 2026-09-17
author: agent
issue: dev-docs/issues/open/tools-as-commands.md
design: dev-docs/designs/proposed/tools-as-commands.md
research:
  - dev-docs/research/golem-1.6-tools-as-commands-tickets.md
---

# Implement tools as commands for bash-tool

## Tasks

- [x] Prepare current fileable upstream tickets and PR comment drafts.
- [x] Add pure command metadata, argv projection, constraints, and help.
- [x] Add a shared Golem discovery/canonical-input/RPC adapter.
- [x] Register tool builtins, manifests, and PATH entries without replacing the Clank plug-in.
- [x] Enforce command-body policy and deny unseen nested confirmations.
- [x] Install the adapter in bash-tool and the embedding path; retire the throwaway probe.
- [x] Carry core prompts and shell options in bounded state; omit marked secrets.
- [x] Add a minimal caller and verify nested streams, shared files, and recovery live.
- [x] Run native regression tests, wasm checks, formatting, and appropriate lint checks.
- [x] Measure cold and steady-state release calls.
- [x] Complete environment flags, stdin positionals/tails, separator semantics, and alias constraints.
- [x] Refuse background work introduced through eval, source, traps, aliases, and restored definitions.
- [x] Extend schema round-trip and live acceptance coverage for these paths.
- [x] Add adapter tests to CI and provide a reproducible compatible SDK/CLI setup.
- [x] Patch the rustls advisory blocking CI and verify the native HTTP consumers.
- [ ] Enable independent tool stderr after upstream support lands (external dependency).

## Acceptance

The echo fixture exercises inherited globals, aliases, repeatable arguments, JSON record options,
constraints, stdin/stdout, typed errors, and filesystem-capable sibling tools. Native tests cover
projection and policy independently of Golem. Live checks exercise actual calls from bash-tool,
including pipelines, substitutions, prompt answering/aborting, denied hidden commands, and replay.

## Implementation notes

The existing tree contains unrelated user edits; those are preserved. Research drafts remain local;
filing tickets or posting comments is a separate external action. Full stderr acceptance is gated
on the upstream protocol addition; it must not be reported as complete with the current SDK.

## Verification

- Native regression run: 499 unit/integration tests plus one compile doctest passed across bash,
  bash-golem, bash-tool, clank-core, clank-embed, and the echo fixture. One existing doctest is ignored.
  Tests run serially because the existing logger tests share process-global state.
- Wasm clippy passed with warnings denied for bash, bash-golem, bash-tool, clank-embed, echo-tool,
  and greeter-agent. The fixture suppresses one SDK-generated large-error lint on subtree clients.
- Formatting, shell syntax, and git whitespace checks passed.
- Debug and release acceptance each passed 27 live checks through a fresh disposable Golem server.
  The shell has an explicit filesystemAccess grant and no provisioned marker. The shared adapter
  is installed in both bash-tool and the embedding path.

## Deviations and remaining dependencies

- An additional upstream umbrella draft tracks 1.6 delivery across local shell work and upstream
  dependencies. No tickets or comments were posted.
- The bash tool descriptor is now 0.2.0: pending_prompt carries question/choices and two prompt
  commands were added. Rebuild callers/components together. Opaque state v1 is still readable.
- State restoration validates declarations and restores function definitions without executing their
  bodies. This avoids false authorization prompts from saved function bodies and rejects execution
  or expansions embedded in caller-carried declarations.
- Attachments are finite and bounded at 16 MiB per channel. RPC pumping and draining run concurrently;
  raw partial stdout is retained on attachment failure. Secret capability input is refused and
  structured results/error payloads are rendered with SDK redaction.
- Real provider invocation is wasm/Golem-only. Native tests use the transport-independent boundary;
  the native SDK adapter does not import a host. Native pipeline threads do not inherit its dispatch
  context, so arbitrary native transports need context propagation before they can project pipelines.
- Provider stderr cannot be implemented with the current tool WIT. Local parser and RPC diagnostics
  use shell stderr; that does not satisfy the upstream stderr acceptance item.
- Optional Rust argument encoding remains an upstream SDK bug. The implementation does not invent
  a replacement wire contract. Existing reflection/constructor and external-client PR APIs are not
  assumed to have landed.

Release latency and reproducibility details: [validation](../../research/tools-as-commands-validation.md).

## Follow-up verification

- 505 native unit/integration tests and one compile doctest passed in a fresh clone configured
  against upstream SDK commit 541300b4a104e4f70838690b525527e3fc26a664. One existing doctest is ignored.
- Release acceptance passed 38 live checks using the configured development CLI/SDK. New checks
  assert exact outputs for environment flags, positional stdin, pipelines, JSON lists/records, and
  refusal of background work in eval, traps, aliases, and sourced files.
- Wasm clippy and native adapter/core clippy (all targets) passed with warnings denied;
  formatting, shell syntax, and whitespace checks passed.
- CI includes adapter tests per push and release live acceptance nightly/on demand. The shared
  setup configures a pinned compatible SDK/CLI; YAML parsed locally and fresh-clone SDK setup was
  exercised. GitHub's Linux jobs and the source-built matching CLI have not been run locally.

## CI audit remediation

GitHub run 35229474036 passed native tests/conformance and both clippy targets, but cargo-audit
blocked the hygiene job on RUSTSEC-2026-0285. The lockfile now selects rustls 0.23.45 and its
compatible rustls-webpki 0.103.15 patch. No advisory exceptions or dependency constraints changed.
Local cargo-audit and cargo-deny passed; all 86 tests for clank-native, whttp, wcurl, and waget
passed, and the rebuilt wcurl client completed an HTTPS request to RustSec with status 200.
