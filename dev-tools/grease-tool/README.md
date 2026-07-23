# grease-populate

An interactive authoring tool **and** live HTTP server for a
[grease](../../README.md) package registry. One process does two jobs:

- **Serves** a registry directory over HTTP so a clank shell can
  `grease registry add` it and `grease install` from it.
- **Authors** every package kind — prompt, script, skill, agent, mcp —
  walking you through the fields, then **signing + content-hashing +
  transparency-logging** each entry.

The crucial property: authoring uses the *same* `clank_core::grease::pkg` code
the durable agent verifies with (sha256 content hash, ed25519 signature,
RFC-6962 single-leaf inclusion proof). So what this tool emits is, by
construction, exactly what `grease install` accepts — no separate,
drift-prone re-implementation of the wire format.

This is a **native-only dev tool** (`publish = false`): it uses a terminal UI
(`inquire`), `std::net` for the server, and `/dev/urandom` for key generation.

## Quick start

From the workspace root, point the tool at a directory to use as the registry
(it is created if missing):

```console
$ cargo run -p grease-tool -- /tmp/my-registry
grease-populate — registry: /tmp/my-registry
serving:  http://localhost:8823
pubkey:   qA3f…（base64 ed25519 public key）

In a clank shell, trust + install from this registry with:
  grease registry add http://localhost:8823 --key qA3f…

? add to the registry:  ›
  prompt
  script
  skill
  agent
  mcp
  list
  quit
```

Options: `--port <n>` (default `8823`; `0` picks an ephemeral port) and
`--signer <name>` (default `grease-populate`, recorded in each index entry).

Then, in a clank shell, trust the registry with the printed public key and
install a package:

```console
clank$ grease registry add http://localhost:8823 --key qA3f…
clank$ grease install summarize
```

## What gets written

Inside the registry directory:

| Path | Contents |
|---|---|
| `index.json` | The package index: one signed, hashed, log-proofed entry per package (upserted by name). |
| `packages/<name>.json` &nbsp;·&nbsp; `<name>.md` | The exact payload bytes that were hashed and signed (`.md` for a Markdown prompt authored from a file; `.json` otherwise). |
| `.signing-seed` | The base64 ed25519 seed, generated on first run and reused after — so the **public key stays stable** across runs and previously-added registries keep verifying. Treat it like a private key. |

The HTTP server exposes exactly two paths — `GET /index.json` and
`GET /packages/<name>` — the only things `grease` fetches, with a
path-traversal guard.

## The five package kinds

The menu authors each of grease's five package kinds. The **prompt**,
**script**, and **skill** kinds can pull their bodies from a file; **agent**
and **mcp** are answered entirely at the prompts. Ready-to-use samples live
under [`samples/`](samples/).

### prompt — a reusable, parameterized model prompt

Installs as an `ask`-backed command. Author it two ways:

- **inline** — type the body in `$EDITOR`, declare arguments interactively; or
- **from a `.md` file** — a Markdown file with a leading `---` YAML
  frontmatter block declaring `name` / `description` / `model` / `arguments:`,
  and the prompt body (with `{{arg}}` placeholders) after the closing fence.

Sample: [`samples/prompt/summarize.md`](samples/prompt/summarize.md). Choose
`prompt` → `from a .md file` → give it that path, then a registry name and
description.

### script — a shell script installed as a command

The body is shell source; on install it lands at `/usr/bin/<name>` and becomes
a first-class command. `{{arg}}` placeholders become `--arg` flags. Choose
`script`, then point the "body source" at a `.sh` file (or type it inline).

Sample: [`samples/script/greet.sh`](samples/script/greet.sh).

### skill — capability context (not a command)

A `{name, description, intended-use?, documents[], scripts[]}` envelope.
Installs under `/usr/share/skills` as reference material the model consults
when the task matches `intended-use` — it is **not** run as a command, though
it may carry bundled scripts. Choose `skill`, then add one or more documents
(path within the skill + body) and, optionally, bundled scripts.

Sample: [`samples/skill/code-reviewing/`](samples/skill/code-reviewing/) —
a `SKILL.md` document plus a `checklist.sh` bundled script.

### agent — a deployed Golem agent, reachable over wRPC

Records an agent *type*, its constructor params, and its methods so
`grease install` can wire up wRPC calls. Answered entirely at the prompts (no
file).

Sample walkthrough: [`samples/agent/greeter.md`](samples/agent/greeter.md),
mirroring the repo's `greeter-agent` fixture.

### mcp — a remote MCP server

Records a server URL (and an optional auth env-var name). On install the agent
connects and exposes the server's tools as `<server> <tool>` commands, its
prompts on `$PATH`, and its resources under `/mnt/mcp`. Answered at the prompts
(no file).

Sample walkthrough: [`samples/mcp/deepwiki.md`](samples/mcp/deepwiki.md).

## Samples at a glance

| Kind | Sample | Consumed as |
|---|---|---|
| prompt | [`samples/prompt/summarize.md`](samples/prompt/summarize.md) | a `.md` file (frontmatter + body) |
| script | [`samples/script/greet.sh`](samples/script/greet.sh) | a `.sh` file (script body) |
| skill | [`samples/skill/code-reviewing/`](samples/skill/code-reviewing/) | a document + a bundled script |
| agent | [`samples/agent/greeter.md`](samples/agent/greeter.md) | typed answers (walkthrough) |
| mcp | [`samples/mcp/deepwiki.md`](samples/mcp/deepwiki.md) | typed answers (walkthrough) |

## Development

The pure authoring core (`Registry`, `write_package`, `serve`) is
`inquire`-free and unit-tested against the real agent-side verifier — every
authored kind is round-tripped through `verify_signature` and
`verify_inclusion_proof`:

```console
$ cargo test -p grease-tool
```
