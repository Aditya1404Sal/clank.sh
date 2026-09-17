---
title: "How bash-tool is exported, bound, and discovers sibling commands"
date: 2026-09-17
author: agent
---

# How bash-tool is exported, bound, and discovers sibling commands

There are three names in the standalone path: the Rust crate is `bash-tool`, the deployed component
is `clank:bash`, and the published/bound tool is `bash`. It is a Golem tool, with no agent identity
or constructor of its own. The consuming agent supplies the owner context.

## Export and binding

`crates/bash-tool/src/lib.rs` declares the `Bash` trait with `#[tool_definition(version = "0.2.0")]`
and implements it using `#[tool_implementation]`. The SDK generates command metadata, schema
conversion, registration, and invocation dispatch for `run`, `answer-prompt`, and `abort-prompt`.
The leaf crate enables `export_golem_agentic` and builds a `cdylib` named `clank_bash`; the SDK
provides the Golem guest exports.

`golem.yaml` registers `components.clank:bash`, declares `tools.bash`, and binds `bash` under both
`agents.ClankAgent.tools` and `agents.BashHost.tools`, with `filesystemAccess: allowed`. Echo tools
are sibling bindings on those same owners. Declaring a tool publishes it; binding it makes it
available to that agent. Filesystem grants belong to each consuming binding.

The fixture in `fixtures/greeter-agent/src/bash_host.rs` is the minimal standalone caller. Its
`eval` invokes the bound `bash` tool at path `["run"]`, with `{state, script}` as canonical input.
It reads the returned BashResult and saves its opaque state for the next call. Answer/abort methods
use the corresponding tool paths. Each bash invocation creates a fresh Session, restores its state,
executes the action, and returns updated state, output, status, cwd, and any pending prompt.

ClankAgent's `eval` uses `EmbeddedShell`, which owns the shared core plus Clank's plugin/providers.
`EmbeddedShell::ensure` installs the same bash-golem adapter before running Clank setup. Thus the
standalone tool and the embedding path share projection/invocation code; Clank's regular eval path
currently executes its embedded shell. BashHost exercises the standalone tool RPC path.

## Discovery

`bash-tool::new_session` calls `bash_golem::install`. On wasm that calls `discover`, whose host
operation is `golem:tool/host.get-all-tools`. Inside a tool invocation the host supplies the consuming
owner's bound tools, including their lookup names and full definitions. This is owner-scoped
binding discovery; publishing a tool elsewhere does not automatically add it to the shell.

`bash-golem::project` validates each SDK definition, traverses its command graph, and derives its
canonical input fields. It retains subcommand names/aliases, inherited globals, argument types,
defaults, environment keys, stdin/stdout declarations, constraints, annotations, and named errors.
The binding lookup name is retained separately from the tool descriptor's root name.

`Session::set_tools` registers one Brush builtin per bound lookup name, attaches help/manifests,
and adds `/usr/lib/tools/bin` to PATH. Entries in that directory carry discovery/help text; actual
invocation is the builtin. Existing shell/plugin commands win collisions. For example:

```sh
echo-tool --help
echo-tool greet --help
echo-tool hi Ada -vv --shout
printf 'hello\n' | echo-tool upper
printf 'one\ntwo\n' | echo-tool stdin-arg -
```

Discovery runs on each fresh bash-tool call. The invoker keeps the original SDK definitions for
that call so parsing and invocation use the same schema. An embedded Session discovers when it
is first initialized; it does not automatically refresh bindings on every eval.

## Invocation

For `echo-tool greet Ada -vv`, Brush expands shell words and resolves the registered `echo-tool`
builtin. The pure parser selects `["greet"]`, applies metadata-driven coercion/defaults, and builds
the input record. Alias names become canonical command/field names. Dispatch checks command policy
before reading stdin: read-only commands run; other visible commands request confirmation. Hidden
mutating calls in functions/eval/source/substitutions are refused when they cannot ask safely.

`bash-golem::encode_input` packs and validates that record using the SDK's published canonical
model. The invoker opens `ToolRpc` with the binding lookup name and calls `invoke_and_await` with
the selected command path and typed input. It concurrently pumps finite stdin, drains stdout, and
awaits completion through the SDK's wit-bindgen runtime. Golem preserves the original owner's
context and applies binding grants, so filesystem-capable siblings observe the same owner files.

A declared stdout stream supplies output bytes. Without a stdout stream, string results become
plain text and other structured results become redacted JSON. Named tool errors select the
descriptor's declared exit code. Output and diagnostics return to Brush's fd table, which applies
pipes, substitution, and redirections.

## Verified limits

Release acceptance passed 38 live checks through BashHost and the actual bash component. A fresh
clone using upstream SDK commit 541300b4a passed 505 native tests plus one compile doctest; wasm
clippy passed. Details are in tools-as-commands-validation.md. The CI jobs are configured but their
Linux execution and source-built matching CLI have not been exercised locally.

Independent provider stderr still needs an upstream attachment channel. Optional Rust argument
wire encoding still needs the SDK fix. The native SDK install is a no-op; fake transports exercise
the pure boundary, with native pipeline context propagation remaining separate work. Secret
capability inputs and background work are explicitly refused for the 1.6 stateless tool profile.
