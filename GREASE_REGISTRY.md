# main-rc-1 — the grease registry: what it is and what you upload

`grease` is clank's package manager. A **registry** is just a static HTTP directory serving a signed
`index.json` plus one payload file per package. There is no registry server to run — `python3 -m
http.server` over a directory is a complete registry. This doc explains the on-disk shape, the five
package kinds and what each installs, and how signing/transparency works, so you can author your own
packages for a demo.

Quick start (the fixture) is in [PREREQUISITES.md](PREREQUISITES.md); this doc is the "author your
own" companion.

---

## The registry is two things on disk

```
/tmp/clank-rc1-registry/
├── index.json                 # the signed catalog — one entry per package
└── packages/
    ├── hello.json             # payload, content-addressed by sha256
    ├── greeting.md            # a prompt authored as YAML-frontmatter Markdown
    ├── hostinfo.json
    ├── reviewing.json
    └── greeter.json
```

`grease registry add <url>` reads `<url>/index.json`; `grease install <name>` fetches
`<url>/packages/<name>.<ext>`, verifies it against the index entry, then installs by kind.

### `index.json` — the catalog

Each entry carries the metadata **and the integrity proof** for one package:

```json
{
  "packages": [
    {
      "name": "hello",
      "kind": "prompt",
      "description": "a signed+logged prompt",
      "sha256": "<hex of the served payload bytes>",
      "sig": "<base64 ed25519 signature over the payload bytes>",
      "signer": "clank-fixture",
      "log": { "leaf-index": 0, "tree-size": 1, "root": "<base64>", "proof": [] }
    }
  ]
}
```

- **`sha256`** — content address of the exact bytes served at `packages/<name>.<ext>`. Install fails
  if the fetched bytes don't hash to this.
- **`sig`** — detached ed25519 signature over those same bytes. Verified against the key you passed
  to `grease registry add --key`. No `--key` = signatures not enforced (unsigned registry).
- **`log`** — an RFC-6962 transparency-log inclusion proof: the package is provably present in the
  registry's append-only log. Single-package demos use a one-leaf tree (`tree-size: 1`, empty proof,
  `root = sha256(0x00 ‖ sha256hex)`). Optional per entry.

---

## The five installable package kinds — what to upload

The **payload** (`packages/<name>.<ext>`) declares the kind and carries the kind-specific body. Here
is exactly what each one looks like and what installing it does.

### 1 · prompt → an `ask`-backed command on `$PATH`

Installs to `/usr/lib/prompts/bin/<name>`. Running it fills the stored body from arguments and sends
it through the model. This is the most demo-friendly kind.

**`packages/hello.json`:**
```json
{"kind":"prompt","name":"hello","description":"a signed+logged prompt","body":"Say hello."}
```

**Or, human-friendly, as `packages/greeting.md`** (YAML frontmatter + Markdown body). grease
verifies integrity over the **raw `.md` bytes**, then converts the frontmatter into a prompt:
```markdown
---
name: greeting
description: a frontmatter-authored prompt
arguments:
  - name: who
    required: true
---
Say hello to {{who}}.
```
Install → run: `grease install greeting` then `sudo greeting who=Aditya`.

**Good things to upload:** a code-review prompt, a "summarize this diff" prompt, a commit-message
writer — anything you'd otherwise paste into `ask` repeatedly.

### 2 · script → a shell script at `/usr/bin/<name>`

Runs local shell source (no model). `{{arg}}` tokens are filled from `name=value` arguments.

**`packages/hostinfo.json`:**
```json
{"kind":"script","name":"hostinfo","description":"print a labelled hostname","arguments":[{"name":"label","required":true,"description":"a label"}],"body":"echo {{label}}: $(cat /etc/hostname)"}
```
Install → run: `grease install hostinfo` then `hostinfo label=demo`.

**Good things to upload:** repo bootstrap scripts, a "show me the environment" helper, canned curl
calls.

### 3 · skill → context under `/usr/share/skills/<name>/` (NOT a command)

A capability-context package: documents the model consults plus bundled `$PATH` scripts. Installing
it does **not** create a callable command — it enriches what `ask` knows.

**`packages/reviewing.json`:**
```json
{"kind":"skill","name":"reviewing","description":"how to review code","intended-use":"when reviewing code","documents":[{"path":"SKILL.md","content":"Review for correctness first."}],"scripts":[{"name":"review-note","body":"echo check error paths"}]}
```
Install → inspect: `grease install reviewing` then `ls /usr/share/skills/reviewing/`.

**Good things to upload:** a house style guide, a runbook, domain knowledge the model should apply.

### 4 · mcp → an MCP server (tools/prompts/resources land as usual)

The payload points at an MCP server URL; installing it wires the tools into `$PATH`
(`/usr/lib/mcp/bin/`), prompts into `/usr/lib/prompts/bin/`, and resources into `/mnt/mcp/<server>/`.
`--tools` / `--prompts` / `--resources` select subsets.

*(Not in the fixture set — you'd point it at a real HTTPS MCP server. For a live MCP demo, `mcp add`
directly is simpler; use an mcp *package* when you want the connection to travel in a registry.)*

### 5 · agent → a Golem agent executable — AGENT/CLUSTER

Installs to `/usr/lib/agents/bin/<name>`; invoking it dispatches a wRPC call to a deployed Golem
agent. Needs the target agent (`GreeterAgent`) deployed alongside clank.

**`packages/greeter.json`:**
```json
{"kind":"agent","name":"greeter","description":"a greeter agent (wRPC round-trip target)","agent-type":"GreeterAgent","constructor-params":["name"],"methods":[{"name":"greet","description":"greet someone by name","params":["who"]}],"ephemeral":false}
```
Install → invoke (agent only): `grease install greeter` then `greeter greet who=world`.

**Good things to upload:** any second agent you've deployed and want to invoke by name.

> **6 · wRPC WASM components** are roadmap-only — reserved in the README, not yet an installable
> kind.

---

## How to produce a registry

### The easy path — the fixture (recommended for demos)

The `grease-fixture` example writes all five packages above, computes real sha256 + ed25519
signatures + transparency proofs with the **same code the verifier uses** (so it can't drift), and
prints the public key:

```bash
cd ~/Desktop/clank-spike
cargo run -q --example grease-fixture -- /tmp/clank-rc1-registry
# → PUBKEY=6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=   (deterministic; fixed [7u8;32] seed)
cd /tmp/clank-rc1-registry && python3 -m http.server 8823
```

### Adding your own package to it

To add a package to a registry you author, you must, for the new payload bytes:

1. Write `packages/<name>.<ext>` with one of the payload shapes above.
2. Compute `sha256` of the exact served bytes.
3. Sign those bytes with your ed25519 key → `sig` (base64).
4. Optionally build the transparency-log entry (`leaf = sha256hex`, `root = sha256(0x00 ‖ leaf)` for
   a single leaf).
5. Append the `{name,kind,description,sha256,sig,signer,log}` entry to `index.json`.

The cleanest way to do steps 2–5 correctly is to **edit `crates/clank-core/examples/grease-fixture.rs`**:
add a `Pkg { … }` to the `packages` vec and re-run the example. It does the hashing, signing, and log
construction for you, in lockstep with the verifier — no chance of a signature mismatch. That is the
intended authoring loop for the demo.

> The `--key` you trust at `grease registry add` time must match the signer. The fixture's key is the
> deterministic `6kpsY+…` above; a real registry would use your own key and publish its public half.

---

## Verifying it end-to-end (either target)

```sh
grease registry add http://localhost:8823 --key 6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=
grease search hello          # catalog is readable
grease info hello            # per-package metadata + generated help
grease install hello         # fetch → verify sha256+sig+log → install
sudo hello                   # run it
grease list                  # confirm it's installed
```

If you tamper with a payload byte after signing, `grease install` fails integrity verification —
that's the whole point of the signed, content-addressed, transparency-logged design.
