---
title: "Tools as commands: local validation and release latency"
date: 2026-09-17
author: agent
---

# Tools as commands: local validation and release latency

The shared adapter and stateless bash component passed 27 live acceptance checks in debug and
release builds. The release run used a fresh disposable local server, a minimal BashHost caller,
and the configured development SDK at `d211d1a77e1dd4d4744c186d8fdfed1127b19e38`. These are clank integration results;
upstream main's test suite was not rerun. The server was stopped after acceptance.

## Evidence

- 499 native unit/integration tests passed across bash, bash-golem, bash-tool, clank-core,
  clank-embed, and echo-tool; one compile doctest passed and one existing doctest was ignored.
- Wasm clippy passed with warnings denied for bash, bash-golem, bash-tool, clank-embed,
  echo-tool, and greeter-agent. Formatting and shell syntax checks passed.
- Live checks covered real nested dispatch, canonical arguments, stdin/stdout pipelines,
  substitution, declared error codes 2/7, parser stderr redirection, confirmation and denial,
  shared owner files in both directions, hidden command refusal, shell variables/functions/options,
  prompt continuation, and state/output after a simulated crash.
- Provider stderr is unavailable in the current WIT; parser/RPC diagnostic tests are distinct
  from the unfulfilled provider stderr acceptance requirement. Optional Rust argument wire
  compatibility remains blocked on the upstream SDK fix.

Reproduce release acceptance with:

```sh
GOLEM_PROBE_PRESET=release scripts/bash-tool-live.sh release
```

Generated logs live under target/: tools-as-commands-tests.log, tools-as-commands-clippy.log,
tools-as-commands-release-live.log, and bash-split/bash-live-latency-release.csv.
The bash release component with Golem metadata is 19,483,864 bytes.

## Release latency

| Measurement | CLI wall clock |
|---|---:|
| First BashHost invocation: owner construction, shell loading, and discovery/help | 6,575 ms |
| First reflected sibling invocation (greet) | 352 ms |
| Steady shell calls, same owner, three samples | 98, 102, 99 ms |
| Steady median | 99 ms |

These measurements include CLI startup, JSON handling, network/queue work, state restoration,
and output capture. They are a small local sample, not runtime-only latency or a percentile study.
The first BashHost call follows an unrelated ClankAgent readiness probe on the same server; shell
and caller components had not yet been invoked. No instance cache was added. Fix the protocol/SDK
blockers first, then profile cold loading separately if that latency matters to release consumers.

The first timing attempt used separate Python monotonic clocks, which have process-relative epochs
on this machine and produced invalid samples. Those samples were discarded. The recorded run uses
shared wall-clock timestamps around each CLI call.
