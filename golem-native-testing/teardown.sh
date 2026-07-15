#!/usr/bin/env bash
# Stop the local Golem server and wipe the throwaway state left by ./setup.sh.
#
# Note: tearing down is almost never the fix for a "stuck" agent. Golem serializes invocations per
# agent instance, so a long `ask` (~30s) makes everything else queue and look wedged — it frees itself
# when the eval returns. Check the log before nuking:
#     grep 'elapsed_ms' /tmp/clank-server.log | tail
# See GOLEM-CONNECT.md, "A long ask is NOT a hang".
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
DATA_DIR=/tmp/clank-connect-data
SERVER_LOG=/tmp/clank-server.log

echo "=== stop server ==="
pkill -f 'golem server run' 2>/dev/null && echo "server killed" || echo "no server process"
sleep 1
lsof -i :9881 >/dev/null 2>&1 && echo "WARNING: port 9881 still busy" || echo "port 9881 clear"

echo "=== wipe throwaway state ==="
# The data dir holds the deployed agents — including their env, which carries the API key.
rm -rf "$DATA_DIR" && echo "removed $DATA_DIR"
rm -rf "$REPO/golem-temp" && echo "removed golem-temp (CLI staging)"
[[ "${KEEP_LOG:-}" == "1" ]] || { rm -f "$SERVER_LOG" && echo "removed $SERVER_LOG"; }

echo "=== check clank for files the dev golem binary may have rewritten ==="
# `golem build`/`deploy` auto-syncs coding-agent skills and has rewritten AGENTS.md before.
cd "$REPO"
if ! git diff --quiet -- AGENTS.md 2>/dev/null; then
  echo "  AGENTS.md was rewritten by the golem binary — revert with:"
  echo "      git checkout -- AGENTS.md"
else
  echo "  AGENTS.md clean"
fi
git status --short 2>/dev/null | head
