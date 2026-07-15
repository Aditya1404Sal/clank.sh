#!/usr/bin/env bash
# Stand up a local Golem server + deploy the clank agent, ready for an interactive `agent stream`.
# Leaves the SERVER RUNNING and prints the connect command. See GOLEM-CONNECT.md for the full story.
#
# Why each step is the way it is (all of these were learned the hard way):
#   * --reset, not plain deploy: plain `deploy` says "no changes required" and will NOT re-provision
#     env into agents that already materialized -> `ask` reports "not configured" forever.
#   * the key must be NON-EMPTY *in this process*: golem.yaml's `{{ ANTHROPIC_API_KEY }}` substitution
#     is strict about a MISSING var but silently accepts an EMPTY one (lands as len=0). Hence the gate.
#   * the binary must come from golem-stuff/golem: a dev CLI vs a release server fails with
#     `missing field accountEmail`.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
GOLEM="$REPO/golem-stuff/golem/target/debug/golem"
PORT=9881
DATA_DIR=/tmp/clank-connect-data
SERVER_LOG=/tmp/clank-server.log
AGENT="${1:-demo}"
AGENT_ID="ClankAgent(\"$AGENT\")"

cd "$REPO"

# --- gate: the key must be non-empty HERE, or `ask` silently deploys broken -------------------------
if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
  cat <<EOF
ANTHROPIC_API_KEY is empty/unset in THIS shell.

  Everything except \`ask\` works without it. To test \`ask\`, export it and re-run:
      export ANTHROPIC_API_KEY=sk-ant-...
      ./setup.sh

  To proceed WITHOUT ask, re-run with:
      ALLOW_NO_KEY=1 ./setup.sh
EOF
  [[ -n "${ALLOW_NO_KEY:-}" ]] || exit 1
  export ANTHROPIC_API_KEY=""   # golem.yaml substitution requires the var to be DEFINED
  echo "proceeding without a key (ask will report 'not configured')"
else
  echo "key present in this shell: len=${#ANTHROPIC_API_KEY}, starts '${ANTHROPIC_API_KEY:0:6}'"
fi

[[ -x "$GOLEM" ]] || {
  echo "no golem binary at $GOLEM"
  echo "build it:  (cd golem-stuff/golem && cargo build -p golem)"
  exit 1
}

if lsof -i ":$PORT" >/dev/null 2>&1; then
  echo "port $PORT is busy — an old server is probably still up. Run ./teardown.sh first."
  exit 1
fi

echo "=== 1/4 build clank (wasm agent) ==="
"$GOLEM" -Y build 2>&1 | tail -1 || { echo "build failed"; exit 1; }

echo "=== 2/4 start server on :$PORT (log: $SERVER_LOG) ==="
rm -rf "$DATA_DIR"; mkdir -p "$DATA_DIR"
"$GOLEM" server run --clean --router-port "$PORT" --data-dir "$DATA_DIR" >"$SERVER_LOG" 2>&1 &
for _ in $(seq 1 60); do lsof -i ":$PORT" >/dev/null 2>&1 && break; sleep 1; done
lsof -i ":$PORT" >/dev/null 2>&1 || { echo "server didn't come up:"; tail -15 "$SERVER_LOG"; exit 1; }
echo "server up"

echo "=== 3/4 deploy --reset (wipes agents+env so the key is actually re-provisioned) ==="
"$GOLEM" -Y deploy --reset 2>&1 | tail -2 || { echo "deploy failed"; exit 1; }

echo "=== 4/4 verify the agent's env (length only — never print the key) ==="
"$GOLEM" agent invoke -q --format json "$AGENT_ID" \
  eval '"env | grep -c ANTHROPIC_API_KEY"' 2>/dev/null \
  | grep '"agent.invoke"' \
  | python3 -c "import sys,json; print('  agent env check:', json.loads(sys.stdin.read()).get('result',''))" \
  2>/dev/null || echo "  (verify invoke failed — agent may still be starting)"

cat <<EOF

=== READY ===  server up on :$PORT, agent $AGENT_ID deployed.

Connect (interactive shell):
    $GOLEM agent stream --interactive '$AGENT_ID'

Try:  echo hi  |  pwd  |  prompt-user "your name?"  |  ask "in one word, what is a shell"  |  exit

Debug one command without a TTY:
    ./probe.sh 'echo hi'

Tear down when finished:
    ./teardown.sh
EOF
