# The resolution surface — virtual namespaces, and why `type` and `which` split

**The constraint.** clank presents a single, coherent-looking filesystem and command surface, but it
is stitched together from namespaces with genuinely different backing: some paths are **real files**
Brush's own filesystem code can see; some are **virtual** — no bytes on disk anywhere, content computed
fresh on every read from in-memory state; and some are a mix of both depending on the specific path.
Command *name* resolution has the same split, for a related reason: Brush's own `type`/`which` can only
see what Brush itself dispatches, and several of clank's own commands (`curl`, `ask`, `context`, …) are
never registered as Brush builtins at all — they're intercepted earlier, in
[`Session::eval_line`](../../crates/clank-core/src/session/mod.rs), precisely so their work can run at
the right layer (see [`wall-c.md`](wall-c.md)). So clank layers its own resolvers beside Brush's for
both the filesystem and the command table, and documents which one is authoritative for what.

## Why it's shaped this way

**Brush's extension API offers no per-command dispatch hook.** clank cannot make Brush's own `type` or
`which` builtins aware of clank-intercepted commands by overriding a hook — no such hook exists (this
is the same limitation the authorization gate works around by running *before* Brush at all; see
[`authz.rs`](../../crates/clank-core/src/authz.rs)). The only way to make `type curl` report something
sensible is to intercept the `type` line itself, ahead of Brush, for the specific names Brush cannot
resolve — and defer to Brush for everything else, so its real strengths (aliases, functions, `$PATH`,
every `type`/`which` flag) stay exactly as correct as upstream Brush already makes them.

**Several directories have no bytes on disk to walk.** `/bin` and `/proc` are computed views over
in-memory state (the command registry; the live process table), not directories a real filesystem
walk would ever find content in. A generic file-backed resolver like `which` — which has to work by
checking real paths for existence — structurally cannot serve them; only a purpose-built resolver that
knows the namespace's shape can.

## The virtual and managed namespaces

| Path | Backing | Resolver |
|---|---|---|
| `/bin`, `/bin/<name>` | Virtual — computed from the command registry, no bytes on disk, not on `$PATH` | [`runtime/binfs.rs`](../../crates/clank-core/src/runtime/binfs.rs) |
| `/proc`, `/proc/<pid>/*`, `/proc/clank/system-prompt` | Virtual — computed per-read from the live `ProcessTable` + environment | [`runtime/procfs.rs`](../../crates/clank-core/src/runtime/procfs.rs) |
| `/mnt/mcp/<server>/` static resources | Real files, materialized to disk at install/refresh time | filesystem directly (uutils) |
| `/mnt/mcp/<server>/` dynamic resources | Virtual — fetched live via `resources/read` on a top-level read (Wall C: needs the Session-layer reactor) | [`runtime/mcpfs.rs`](../../crates/clank-core/src/runtime/mcpfs.rs) |
| `/mnt/mcp/<server>/` resource templates | Executable stubs, listed but not readable as files | [`runtime/mcpfs.rs`](../../crates/clank-core/src/runtime/mcpfs.rs) |
| `/usr/bin`, `/usr/lib/{mcp,agents,prompts}/bin`, `/usr/share/skills/*/bin` | Real files/directories, populated by `grease install`; on `$PATH` | [`grease/config.rs`](../../crates/clank-core/src/grease/config.rs), [`session/env.rs`](../../crates/clank-core/src/session/env.rs) |

**`/bin`** — [`runtime/binfs.rs`](../../crates/clank-core/src/runtime/binfs.rs) — is a pure resolver
from a `/bin/<name>` path to that command's manifest `help_text`, built once from a lazily-initialized
static snapshot of [`registry::build()`](../../crates/clank-core/src/registry.rs) (the builtin set
never changes at runtime, so unlike `/proc` this namespace needs no thread-local or `Session` access
at all). `ls /bin` lists every registered command name; `cat /bin/<name>` prints its help — a complete,
uniform capability inventory an AI can enumerate with tools it already knows. `/bin` is deliberately
*not* on `$PATH` and `which` never reports a `/bin/<name>` path, because `which` only walks real
`$PATH` entries via `Path::exists` and there is nothing there to find.

**`/proc`** — [`runtime/procfs.rs`](../../crates/clank-core/src/runtime/procfs.rs) — is the same shape
(a pure path-to-content resolver) over a source that *does* change per line: the current
`ProcessTable` and environment, reached through the same install-a-thread-local-slot-for-one-line
pattern the transcript uses (see [`wasip2-constraints.md`](wasip2-constraints.md)'s "no threads" row
for why that pattern is sound). `/proc/clank/system-prompt` is the one shell-wide virtual file rather
than a per-process one — the current system prompt, computed on read from installed tools/skills/
config, so it is always exactly what the next `ask` would send.

**`/mnt/mcp`** — [`runtime/mcpfs.rs`](../../crates/clank-core/src/runtime/mcpfs.rs) — is the one
namespace that is genuinely mixed, path by path, and says so in its own module doc. A server installed
with `--resources` can surface three different things under `/mnt/mcp/<server>/`: **static** resources
are materialized as real files at install time (this module doesn't even serve them — `cat`/`grep`
read them straight through uutils, no MCP awareness needed); **dynamic** resources have no file behind
them at all and must be fetched live via `resources/read` on every access, which — because clank's
`cat` is a synchronous Brush builtin with no reactor — only works as a top-level line, the same Wall C
restriction `ask`/`curl` have; and **resource templates** are executable stubs (shown in a directory
listing, but not something `cat` can read — you invoke the generated executable with arguments
instead). The per-line resource index that answers "which of the three is this path" is installed as a
thread-local snapshot, mirroring `procfs`.

**`/usr/bin`, `/usr/lib/{mcp,agents,prompts}/bin`, `/usr/share/skills/*/bin`** are the one category
here that is *not* virtual at all — real files and directories on the agent's real filesystem, whose
locations are config functions in [`grease/config.rs`](../../crates/clank-core/src/grease/config.rs)
(`bin_dir` for prompts, `script_bin_dir` for scripts, `agent_bin_dir`, `skills_dir`, `mcp_mount_dir`,
plus [`mcp/config.rs`](../../crates/clank-core/src/mcp/config.rs)'s own `bin_dir` for MCP tool stubs).
`grease install` and `mcp add` write real executables/stubs into them; `$PATH` is assembled from these
same functions in [`session/env.rs`](../../crates/clank-core/src/session/env.rs), which is exactly why
a freshly-installed package is immediately resolvable with no shell restart. Because these are real
files, `which` (and Brush's own `type`) *can* see them — this is the one part of the resolution
surface Brush's own machinery was always going to get right unassisted.

## The `type` / `which` split

The README states the rule; these two modules are where it is enforced.

**`type` is the authoritative resolver — for exactly the commands Brush cannot see.**
[`builtins/typecmd.rs`](../../crates/clank-core/src/builtins/typecmd.rs) defines `INTERCEPTED`: the
closed set of names (`prompt-user`, `curl`, `wget`, `context`, `ask`, `kill`, `mcp`, `grease`,
`golem`) that clank intercepts in `Session::eval_line`/`run_command` *before* Brush dispatch and that
are therefore never registered as Brush builtins — Brush's own `type` has no way to know they exist.
`typecmd::dispatch` answers `type <name>` for these, matching Brush's own wording exactly
(`"<name> is a shell builtin"`) so the two resolvers are indistinguishable to a caller; it fires
**only** when every queried name in the line is in `INTERCEPTED` — a `type curl cat` (mixing an
intercepted name with a Brush-known one) defers the whole line to Brush rather than half-answering it,
a documented scope cut. `typecmd::help_for` closes the matching gap for `--help`: since these commands
never reach Brush, they would otherwise ignore `<cmd> --help` entirely; this function serves the same
manifest `help_text` `cat /bin/<name>` would, after stripping a leading `sudo` first (`sudo` only
pre-authorizes — it must not change what `--help` prints; this is the fix for a real regression where
`sudo curl --help` surfaced an outbound-HTTP confirmation instead of help). For MCP-installed commands
specifically, [`runtime/dynreg.rs`](../../crates/clank-core/src/runtime/dynreg.rs) installs the current
line's dynamic manifests into a thread-local slot so surfaces like `man` can resolve a runtime-registered
name beside the static registry `binfs` uses — the same "install for one line" pattern used everywhere
else in the resolution surface.

**`which` finds file-backed commands only, deliberately, and cannot be extended to more.**
[`tools/which.rs`](../../crates/clank-core/src/tools/which.rs) is a small hand-written `SimpleCommand`
(clank has no `which` from Brush) that walks `$PATH` and reports only names that resolve to a real,
existing file — never a builtin, alias, or function, and never one of clank's virtual `/bin` entries.
It deliberately does **not** reuse Brush's own `Shell::find_executables_in_path`: on wasm, Brush's
`PathExt::executable()` returns `true` unconditionally with no real existence check (see
[[wasip2-fs-existence-checks]]), so that path yields *phantom* results — reporting
`/usr/local/bin/foo` for a command that was never installed. `which` must not lie, so it checks each
`$PATH` candidate with `Path::exists()` directly, which does behave correctly on the agent's real
per-agent filesystem. The practical effect: `which` is honest but narrow (only real files), `type` is
broad but only extended for the specific names Brush can't already resolve — between the two, and
Brush's own unmodified `type` for everything else, every command clank exposes has exactly one correct
resolver to ask.

Both resolvers ultimately read the same underlying classification: whether a command is
`parent-shell`, `shell-internal`, or `subprocess` scoped is decided once, in
[`registry.rs`](../../crates/clank-core/src/registry.rs)'s manifests, and everything downstream —
`type`, `which`, `man`, `ps`, the authorization gate, and the `ask` tool surface — reads that same
table rather than re-deriving the classification per surface.
