# Code reviewing

A capability-context skill. On `grease install code-reviewing` this document is
written under /usr/share/skills — it is **not** a command. It is reference
material the model can consult when the task at hand matches the skill's
intended use ("when reviewing a code change").

## How to review a change

1. **Correctness first.** For every changed line, ask what input, state, or
   timing makes it wrong. Read the whole enclosing function, not just the hunk —
   a bug can hide in the unchanged lines the change re-exposes.
2. **Removed behavior.** For every deleted line, name the invariant it enforced
   and find where the new code re-establishes it. A dropped guard is a finding.
3. **Error paths.** Check that errors are surfaced, not swallowed. A discarded
   `Result` or a bare `catch` that logs and continues is suspect.
4. **Blast radius.** Trace the callers of any function whose signature or
   contract changed.

## Bundled scripts

This skill ships one helper script (`checklist.sh`) alongside the document —
skills may carry both prose and runnable scripts.
