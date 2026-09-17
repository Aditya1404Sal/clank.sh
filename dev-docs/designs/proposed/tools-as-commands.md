---
title: "Metadata-driven tool commands in bash"
date: 2026-09-17
author: agent
---

# Metadata-driven tool commands in bash

The developer approved faithful command behavior for 1.6: discovery, help, argument parsing,
pipes, separate stderr, exit codes, authorization, and replay. Secret capability arguments and
background work are explicitly refused. Upstream tickets and PR follow-ups are prepared as drafts.

## Boundaries

`bash::agent_tools` owns transport-independent command metadata, argv projection, help, and builtin
dispatch. `Session` installs these alongside its existing builtins, independently of its plug-in.
A shared `bash-golem` library discovers owner bindings, derives canonical fields through the SDK,
packs input against that exact schema, and completes RPC while pumping attachments concurrently.
Neither the core nor the bash component depends on `clank-embed`. The adapter uses the SDK's pinned
wit-bindgen runtime until a supported upstream blocking adapter becomes available.

Compiled-in commands win collisions. Tool policy is selected per command body: read-only allows,
all other annotations confirm. A builtin also checks approval at dispatch, so substitutions,
functions, eval, source, and wrapper reentry cannot acquire approval through an unrelated command.

## Stateless calls

Keep the existing `bash run [--state] <script>` interface and add `answer-prompt` and `abort-prompt`.
State v2 adds shell options and the serializable core prompt continuation; v1 remains readable.
Capture shell state through an internal session operation even while a prompt is pending. Marked
secret variables are omitted. State input and output are bounded before decoding or allocation.
State restoration accepts literal declarations, shell options, and deferred function definitions,
and rejects command execution or value expansion within declarations. The bash descriptor is 0.2.0;
its pending prompt carries question and choices, so callers must rebuild with the component.
The component refuses background work that cannot survive the invocation, including substitutions.
Its finite-invocation profile validates command lists and nested expansions, wraps eval/source/trap/
alias entry points to validate their expanded code, and checks restored function bodies. Job-control
and history execution return exit 2. Source accepts bounded regular UTF-8 files; descriptor paths
such as /dev/stdin are refused. Prompt expansion is disabled and cannot be enabled through shopt;
this prevents PS4 trace strings from introducing unvalidated command substitutions.

## CLI projection

Environment flag values are coerced by their declared type, with explicit CLI flags taking
precedence. Alias names in presence/value constraints resolve to canonical field names. Custom
tail separators retain flag parsing unless the tail is verbatim; `--` always ends option parsing.
For accepts_stdio positionals, an absent argument without a default or a literal dash consumes
bounded stdin. Scalar strings retain UTF-8 bytes, typed scalars trim surrounding whitespace, and
tails use one argument per line. Cardinality and constraints are checked after input is read.
The parser exposes an explicit stdin-needed projection instead of advertising partial canonical
input; dispatch checks authorization before reading and creates attachments only when declared.

## Upstream compatibility

Develop against the configured SDK; open reflection and external-invocation PR APIs are not assumed
to exist. Named custom errors select exit codes from the command descriptor. The adapter never
merges structured results with a declared stdout stream. Independent tool stderr is unavailable in
the current WIT and remains explicitly unfinished until GOL-594 lands. Bash's own diagnostics and
its result stderr field already work.

## Validation boundary

The real adapter uses Golem host imports on wasm; native installation is a no-op. Pure native
projection/dispatch tests use a fake invoker. Native Brush pipeline worker threads do not inherit
the dispatch context, so arbitrary native transports require context propagation to support those
paths. Live pipeline/substitution, owner filesystem, prompts, and recovery coverage runs through
the actual bash component on Golem. Each attachment is bounded to 16 MiB; shell state to 256 KiB.
CI runs the adapter's schema tests per push and release bash-tool acceptance nightly/on demand.
A shared setup action checks out the pinned upstream SDK and builds its matching CLI for live jobs;
the workspace path remains absolute but can be configured by scripts/use-golem-sdk.py in a fresh clone.
