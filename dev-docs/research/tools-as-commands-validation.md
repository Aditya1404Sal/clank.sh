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

## Parser and finite-invocation follow-up

Release acceptance passed 38 checks after completing environment flag coercion, stdin-backed
positionals/tails, custom separator semantics, and canonicalization of constraint aliases. Scalar
defaults take precedence over implicit stdin; explicit dash selects stdin. Typed scalars trim
whitespace, strings preserve UTF-8 bytes, tails consume lines and enforce min/max after reading.

The shell's stateless profile checks nested command lists and expansions, validates expanded eval/
source/trap/alias code and restored function bodies, and refuses job-control/history execution.
Dynamic prompt expansion is disabled, including PS4 trace strings; sourcing requires bounded
regular UTF-8 files and descriptor paths are refused. Native tests verify safe eval, source
positional parameters and return codes, arithmetic bitwise operators, and forged state rejection.

505 unit/integration tests plus one compile doctest passed in a fresh Clank clone using upstream
SDK commit 541300b4a104e4f70838690b525527e3fc26a664. One existing doctest remains ignored. Fresh SDK
configuration used scripts/use-golem-sdk.py with the exact-revision check. Wasm clippy passed with
warnings denied against the configured development SDK. The live run used that development CLI;
upstream executor integration tests and GitHub's Linux CI jobs were not rerun.
Native core/adapter clippy also passed with all targets and warnings denied, including the new
integration-test helpers. Two state accessor attributes and helper lint scopes were corrected.

Evidence: target/tools-as-commands-follow-up-fresh-tests.log,
target/tools-as-commands-follow-up-clippy.log, target/tools-as-commands-follow-up-live.log, and
target/bash-split/bash-live-latency-follow-up.csv. The disposable live server was stopped.

The CI setup action now checks out the pinned upstream SDK, configures its absolute workspace path,
and builds its matching full Golem binary for live jobs. Per-push library tests include bash-golem;
nightly/manual release acceptance runs bash-tool-live.sh and uploads logs. Workflow/action YAML
parsed locally, but the matching CLI source build and Linux jobs await their actual CI execution.

## CI audit remediation

[GitHub run 35229474036](https://github.com/Aditya1404Sal/clank.sh/actions/runs/35229474036)
passed Linux native tests/conformance, formatting, native/wasm clippy, and fork inventory checks.
Its hygiene job failed on [RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285.html)
in rustls 0.23.41; cargo-deny was skipped after that failure. Main had the same vulnerable lockfile
version. The fix updates only rustls to 0.23.45 and rustls-webpki to a compatible patch, 0.103.15.

- `cargo audit` passed with the existing fxhash unmaintained and chacha20 yanked warnings.
- `cargo deny check` passed advisories, bans, licenses, and sources; no exceptions were added.
- `cargo test --locked -p clank-native -p whttp -p wcurl -p waget -- --test-threads=1`
  passed all 86 tests.
- The rebuilt native wcurl client fetched RustSec's advisory headers over HTTPS and received 200.

Evidence: target/tools-as-commands-rustls-audit.log, target/tools-as-commands-rustls-deny.log,
target/tools-as-commands-rustls-tests.log, and target/tools-as-commands-rustls-https.log.
The TLS dependency is native-only; prior Golem release acceptance is recorded above.
