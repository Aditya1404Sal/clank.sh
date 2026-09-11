# dev-docs

Working documents for clank's development: what is broken, what we decided to do about it, and the
ticket breakdown that implements the decision. This is the *engineering* record. User-facing
documentation lives in [`docs/`](../docs/), and [`README.md`](../README.md) is the project's front
page — neither is a stage of this workflow.

## The chain

Each piece of substantial work produces up to four documents, in this order:

| Stage | Answers | Directory |
|---|---|---|
| **issue** | What is wrong? | `issues/{open,closed}/` |
| **research** | What did we find out? | `research/` |
| **design** | What are we going to do, and what did we reject? | `designs/{proposed,approved}/` |
| **plan** | What are the tickets? | `plans/{proposed,approved,done}/` |

Not every piece of work needs all four. A defect with an obvious fix needs an issue and nothing
else; a question that resolves to "no change" produces only research. What is *not* optional is the
separation: an issue states the problem with no solution in it, and a design states the decision
with its rejected alternatives. Collapsing them produces a document that argues for the first idea
anyone had.

Each document carries frontmatter linking back up the chain (`issue:`, `research:`, `designs:`), so
a plan is traceable to the problem it exists to solve. Every directory holds an `example.md` — a
worked template, deliberately fictional so it can never be mistaken for project history. Copy it.

## Moving between stages

Directories are the state. A document moves by `git mv`, which keeps its history:

- `designs/proposed/` → `designs/approved/` when the maintainer approves the decision. **A design is
  approved by a person, not by an agent** — that gate is the point of the directory split.
- `plans/proposed/` → `plans/approved/` when the ticket breakdown is agreed, and → `plans/done/`
  when every ticket has landed.
- `issues/open/` → `issues/closed/` when the work that resolves it is merged.

`plans/approved/` is frequently empty. That means no plan is currently mid-approval — not that the
stage is unused. Approval and completion are often the same session, in which case a plan goes
straight from `proposed/` to `done/`.

## Deviation records

A plan that has been executed carries a **Deviations noted during implementation** section at its
foot, written as the work lands. This is the highest-value part of the document and the reason plans
are kept after completion rather than deleted.

Record, specifically:

- **Where the plan was wrong.** A ticket that budgeted days for work that turned out to be a `git rm`
  is worth writing down, because the mistake was not measuring the delta first.
- **Where a ticket's premise did not survive contact.** A step that would have made the code worse if
  followed literally, and what was done instead.
- **What was deliberately left undone**, and why — so the next reader does not re-raise it as an
  oversight.
- **Disproved hypotheses.** If a suspected defect was measured and found not to exist, the
  measurement belongs here permanently. Otherwise it gets re-proposed every six months.

A deviation record is not an apology. It is the part of the document that could not have been
written in advance, and it is what makes the plan worth more after execution than before it.
