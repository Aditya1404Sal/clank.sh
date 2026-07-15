#!/usr/bin/env bash
# Run ONE clank command on the deployed agent non-interactively, via `agent invoke eval`.
# For debugging when you don't want (or can't have) the interactive `agent stream` TTY.
#
#   ./probe.sh 'echo hi'
#   ./probe.sh 'env | grep -c ANTHROPIC_API_KEY'
#   ./probe.sh --answer yes            # resolve a pending prompt (answer_prompt)
#   ./probe.sh --abort                 # cancel a pending prompt (abort_prompt)
#
# Two things this encodes that are easy to get wrong (see GOLEM-CONNECT.md):
#
#  1. RESULT EXTRACTION. The invoke response carries the record as a POSITIONAL schema-value-tree
#     (field names live in the type graph, not the value), so `.resultJson.value.stdout` is EMPTY.
#     We read the top-level `"result"` string (the Rust Debug of EvalResult) instead. And we parse it
#     with python, NOT `grep -o '"result":"[^"]*"'` — that truncates at the first escaped quote and
#     makes a perfectly good result look empty.
#
#  2. SERIALIZATION. Golem runs ONE invocation at a time per agent instance. If the agent is busy
#     (e.g. a ~30s `ask`), this probe QUEUES behind it and appears to hang — that is expected, and
#     firing more probes makes it worse. Check `grep elapsed_ms /tmp/clank-server.log | tail`.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
GOLEM="$REPO/golem-stuff/golem/target/debug/golem"
AGENT="${AGENT:-demo}"
AGENT_ID="ClankAgent(\"$AGENT\")"
cd "$REPO"

[[ -x "$GOLEM" ]] || { echo "no golem binary at $GOLEM (run ./setup.sh first)"; exit 1; }
lsof -i :9881 >/dev/null 2>&1 || { echo "no server on :9881 — run ./setup.sh first"; exit 1; }

# Pull the human-readable `result` string out of the multi-object JSON stream.
extract() {
  grep '"agent.invoke"' \
    | python3 -c "import sys,json; print(json.loads(sys.stdin.read()).get('result','(no result)'))" \
    2>/dev/null || echo "(could not parse invoke response — agent busy, or invoke failed)"
}

case "${1:-}" in
  --answer)
    [[ -n "${2:-}" ]] || { echo "usage: ./probe.sh --answer <response>"; exit 1; }
    "$GOLEM" agent invoke -q --format json "$AGENT_ID" answer_prompt "\"${2//\"/\\\"}\"" 2>/dev/null | extract
    ;;
  --abort)
    "$GOLEM" agent invoke -q --format json "$AGENT_ID" abort_prompt 2>/dev/null | extract
    ;;
  "")
    echo "usage: ./probe.sh '<clank command>' | --answer <text> | --abort"
    exit 1
    ;;
  *)
    "$GOLEM" agent invoke -q --format json "$AGENT_ID" eval "\"${1//\"/\\\"}\"" 2>/dev/null | extract
    ;;
esac
