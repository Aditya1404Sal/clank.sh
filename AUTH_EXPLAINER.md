# main-rc-1 — authorization & permissions, explained

How clank decides what a command (typed by a human, or emitted by the model inside `ask`) is allowed
to do, and where that's implemented. Grounded in the code, not the README prose.

Primary source: [`crates/clank-shell/src/authz.rs`](crates/clank-shell/src/authz.rs).

---

## The model: three tiers, one enforcement point

Every command carries an **`AuthorizationPolicy`** on its manifest
([`manifest.rs:33`](crates/clank-shell/src/manifest.rs#L33)):

| Tier | Behavior | Assigned to |
|---|---|---|
| **`Allow`** | Runs immediately, no prompt | Everything with no manifest (Brush builtins: `cd`, `echo`, `export`, …) plus read-only commands |
| **`Confirm`** | Pauses for **(y)es / (n)o / (a)ll** unless pre-authorized | Side-effecting commands: `curl`/`wget`, `ask`, `mcp add`, `grease install`, agent invocation |
| **`SudoOnly`** | Pauses for **(y)es / (n)o** — strongest tier, no blanket "all" | `rm` (currently the only one) |

The entire decision is a pure function, `decide()`
([`authz.rs:70`](crates/clank-shell/src/authz.rs#L70)):

```rust
Allow    → always Allow
Confirm  → Allow if (elevated || allow_all), else Confirm { sudo_grant: false }
SudoOnly → Allow if elevated, else Confirm { sudo_grant: true }   // "all" does NOT satisfy it
```

**Where it's enforced:** in `Session::eval_line` / `run_command`, **before the command reaches
Brush** ([`authz.rs:5`](crates/clank-shell/src/authz.rs#L5)). Brush's `ShellExtensions` offers no
dispatch hook, so the clank layer is the single choke point. A gated command surfaces the *same*
pending-prompt pause that `prompt-user` uses — one mechanism, not two.

**Every top-level command in a compound line is gated — not just the leading one.** The line is
split on `;` / `&&` / `||` / `|` / `&` (quote- and subshell-aware, via `authz::split_segments`) and
gated on its **strictest** segment (`Session::resolve_authz_strictest`), so `echo hi && rm -rf /x`
gates on `rm`, not the harmless `echo`. Because clank gates before Brush and Brush runs a compound
line atomically, the whole line is one decision: approving runs all of it (the prompt names every
gated command), denying refuses all of it. This applies at both gates — the human line gate and the
model's per-tool-call gate inside `ask`.

---

## What `sudo` and "all" actually do

- **`sudo <cmd>`** means **conscious human authorization, not Unix credentials** — there is no uid 0,
  no `/etc/sudoers` ([`authz.rs:15`](crates/clank-shell/src/authz.rs#L15)). A `sudo` token marks the
  command it prefixes *elevated*, is stripped before the command runs, and **pre-authorizes** that
  command's `Confirm` or `SudoOnly` gate. Elevation is **per-segment**: `sudo curl X | jq` elevates
  the curl, but `sudo echo && rm x` elevates only `echo` — `rm` still prompts (sudo authorizes what
  it prefixes, not a downstream command).
- **Answering "all"** sets a session-wide `allow_all` grant
  ([`authz.rs:30`](crates/clank-shell/src/authz.rs#L30)) — every later `Confirm` command proceeds
  silently. But **"all" never satisfies `SudoOnly`**: `rm` always asks again. That's the deliberately
  strongest guarantee, and the reason `SudoOnly`'s prompt offers only **(y)es / (n)o**, no "all"
  ([`authz.rs:146`](crates/clank-shell/src/authz.rs#L146)).

---

## Subcommand-aware gating

A coarse top-level policy would over-gate read-only subcommands, so `authz::resolve`
([`authz.rs:98`](crates/clank-shell/src/authz.rs#L98)) prefers a matching **subcommand's** policy
when the line's second word names one. The result:

- **`mcp`** top-level = `Allow`; `mcp add`/`remove`/`reload`/`session open`/`close` = `Confirm`;
  `mcp list`/`tools` = `Allow` ([`mcp/cmd.rs:277`](crates/clank-shell/src/mcp/cmd.rs#L277)).
- **`grease`** top-level = `Allow`; `install`/`remove`/`update`/`registry` = `Confirm`;
  `list`/`search`/`info` = `Allow` ([`grease/cmd.rs:142`](crates/clank-shell/src/grease/cmd.rs#L142)).
- **`golem`** is gated per-subcommand the same way.

So `mcp list` is free but `mcp add` prompts — from the same top-level command.

---

## What the *agent* (the model, inside `ask`) is allowed to call

When the **model** drives the agentic loop, every shell command it emits goes through the **exact
same `decide()` gate** — the model is not privileged. Two things shape its authority
([`ai/ask.rs:92`](crates/clank-shell/src/ai/ask.rs#L92)):

1. **The model is told the rules up front.** The system prompt lists every available command with its
   tier in brackets — `[confirm]`, `[sudo-only]` — and explains they "pause for the user's approval
   unless the user ran `sudo ask`."
2. **`sudo ask` grants blanket confirm-tier authorization** for the whole loop, so the model's
   `curl`/`mcp`/`grease` calls don't pause one-by-one. This elevation is a property of **how the human
   launched `ask`** — carried on the invocation (`AskTailPipe.elevated`,
   [`ai/ask.rs:582`](crates/clank-shell/src/ai/ask.rs#L582)) — and the model can **never assert it
   itself**. Per the README, *"Agents cannot use sudo."*

**The bottom line — what the agent can call:**

- **`Allow`** commands: freely.
- **`Confirm`** commands: only if the human pre-authorized via `sudo ask`, or previously answered
  "all". Otherwise each one pauses for a human yes/no.
- **`SudoOnly`** (`rm`): **never** without a fresh, explicit, per-call human approval — `sudo ask`
  does **not** unlock it, and neither does "all".

The durable **pause model** makes this work without ever blocking the agent: the question is recorded
as durable state and returned immediately in `pending_prompt`; the human answers on a *separate*
invocation (`answer_prompt`), which resumes the loop
([`session/mod.rs:501`](crates/clank-shell/src/session/mod.rs#L501)). This exists because Golem
serializes invocations per agent — a parked agent blocking on a human would be unreachable.

---

## Execution scope — a second manifest field, and how it's actually enforced

Alongside `authorization_policy`, every manifest carries an **`execution_scope`**
([`manifest.rs:18`](crates/clank-shell/src/manifest.rs#L18)) — `ParentShell` / `ShellInternal` /
`Subprocess`. It's easy to read this as "*where* a command runs," but that's not what it is in the
implementation.

**There are no real processes.** wasip2 can't spawn, and natively clank runs everything in-process
too. So `Subprocess` doesn't mean fork/exec — it means **"isolated from parent-shell state."** And
scope is **not** a dispatch router: a line is routed by *interception pattern* (is it `context`?
`curl`? an MCP tool line? else → Brush), never by reading this field. Scope is a **classification of
what session state a command may touch**.

**It's enforced in exactly one place — the `ask` model-tool boundary**
([`session/ask.rs:908`](crates/clank-shell/src/session/ask.rs#L908)). When the model emits a shell
command as a tool call, clank refuses it if its scope is `ShellInternal` or `ParentShell`:

```rust
if matches!(m.execution_scope, ShellInternal | ParentShell) {
    return done_err("… is a shell-internal command, not available as a tool; it mutates \
                     shell state ask cannot access");
}
```

The reasoning is sound: the `ask` tool executes isolated (subprocess-like), so it genuinely can't
reach the parent Session's job table, alias table, cwd, or transcript. `cd`/`export`/`alias`/`jobs`/
`context` from the model would mutate nothing the human sees, so they're rejected; only `Subprocess`
commands (`ls`, `grep`, `curl`, installed scripts/prompts) are callable by the model. Everywhere
else the field is descriptive metadata (surfaced by `type`, asserted in tests).

**So the two manifest gates compose:** `authorization_policy` decides *whether a command may run at
all* (per top-level segment, human or model); `execution_scope` decides *whether the model may be the
one to call it*. A command the model wants must be both `Subprocess`-scoped **and** clear its
authorization tier.

---

## The one remaining gap

Compound-line gating covers **top-level** operators (`;` / `&&` / `||` / `|` / `&`) and simple
subshells (`(rm x)` resolves to `rm`). It does **not** yet reach into a **command substitution** or
backticks: in `echo $(rm x)`, the `$(rm x)` is part of a single word token and isn't split out, so
`rm` there is gated only by `echo`'s policy. Recursing into `$(...)` needs full substitution parsing
(nested `$()`, `$(( ))` arithmetic-not-a-command, quoting) — the honest "full quote/subshell
awareness" boundary, left as future work and documented in the `authz.rs` module doc.

**If you demo authorization:** `echo ok && rm /tmp/x` now correctly prompts for `rm`; so does
`curl X | grep y ; rm z` (naming both `curl` and `rm`). The one shape that still slips through is a
command hidden inside `$(...)`.

---

## Everything logged, even when denied

`sudo-only` attempts are recorded to `/var/log/ops.log` **even when blocked** — the audit trail
captures the attempt, not just the success. The `confirm_question` / `confirm_choices` copy
([`authz.rs:136`](crates/clank-shell/src/authz.rs#L136)) matches the README's phrasing so the prompt
reads identically whether it came from a human command or a model tool call.
