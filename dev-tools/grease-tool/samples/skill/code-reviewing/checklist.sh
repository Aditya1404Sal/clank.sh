#!/bin/sh
# A script bundled *inside* the code-reviewing skill. It travels with the skill
# document rather than being installed as a standalone /usr/bin command — a
# quick reminder of the review checklist, printed on demand.
echo "Review checklist:"
echo "  1. Correctness — what input/state/timing makes each changed line wrong?"
echo "  2. Removed behavior — is every deleted invariant re-established?"
echo "  3. Error paths — are errors surfaced, not swallowed?"
echo "  4. Blast radius — did any caller's contract change?"
