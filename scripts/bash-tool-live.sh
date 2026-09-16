#!/usr/bin/env bash
# SPIKE — the `bash` tool walking skeleton on a live Golem server.
# Deploys the clank-spike app (ClankAgent + echo-tool + clank:bash) with golem-probe.sh --keep, then
# drives ClankAgent, whose `probe-tool bash …` builtin invokes the bound `bash` tool over tool-rpc and
# carries the returned state between calls. Tears the server down at the end.
set -uo pipefail

C="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
S="$C/target/bash-split"
mkdir -p "$S"
LIB="$C/scripts/lib/golem-json.sh"
export GOLEM_BIN="${GOLEM_BIN:-$HOME/Desktop/clank.sh/golem-stuff/golem/target/debug/golem}"
export GOLEM_JSON_LOG="$S/bash-live-cli-stderr.log"
: > "$GOLEM_JSON_LOG"
PHASE="${1:-unset}"

if [ -n "$(lsof -tiTCP:9881 -sTCP:LISTEN 2>/dev/null)" ]; then
  echo "port 9881 is busy (another golem server); refusing to start" >&2
  exit 1
fi

cd "$C" || exit 1
echo "=== deploy ($PHASE) via golem-probe.sh --keep"
start=$(date +%s)
scripts/golem-probe.sh --keep 'echo ready' > "$S/bash-live-deploy-$PHASE.log" 2>&1
echo "deploy took $(( $(date +%s) - start ))s"
AGENT=$(grep -o 'ClankAgent("probe-[0-9]*")' "$S/bash-live-deploy-$PHASE.log" | head -1)
SERVER_PID=$(grep -o 'server pid [0-9]*' "$S/bash-live-deploy-$PHASE.log" | head -1 | awk '{print $3}')
SERVER_LOG=$(grep -o 'log [^ ]*server[^ ]*' "$S/bash-live-deploy-$PHASE.log" | head -1 | awk '{print $2}')
if [ -z "$AGENT" ] || [ -z "$SERVER_PID" ]; then
  echo "deploy did not leave a kept agent/server" >&2
  tail -40 "$S/bash-live-deploy-$PHASE.log" >&2
  exit 1
fi
echo "agent $AGENT, server pid $SERVER_PID, log ${SERVER_LOG:-?}"
trap 'kill "$SERVER_PID" 2>/dev/null; sleep 1; kill -9 "$SERVER_PID" 2>/dev/null' EXIT

find "$C" -path '*golem-temp*' -name '*bash*.wasm' -newer "$C/crates/bash/src/lib.rs" 2>/dev/null \
  | xargs ls -la 2>/dev/null | awk '{printf "component %s %.1f MiB\n", $9, $5/1048576}'

run_line() {
  AGENT_ID="$AGENT" perl -e 'alarm shift; exec @ARGV' "${2:-240}" \
    bash -c '. "$0"; golem_run_line "$1"' "$LIB" "$1"
}

pass=0; fail=0
# t <label> <contains|absent> <want> <line>
t() {
  local label=$1 mode=$2 want=$3 line=$4 got dt ok=0 start
  start=$(date +%s)
  got=$(run_line "$line")
  dt=$(( $(date +%s) - start ))
  case "$mode" in
    contains) case "$got" in *"$want"*) ok=1 ;; esac ;;
    absent) case "$got" in *"$want"*) ;; *) ok=1 ;; esac ;;
  esac
  if [ $ok = 1 ]; then pass=$((pass+1)); v=PASS; else fail=$((fail+1)); v=FAIL; fi
  printf '%-4s %-46s %3ss %s\n' "$v" "$label" "$dt" "$(printf '%s' "$got" | tr '\n' '|' | cut -c1-160)"
  [ $v = FAIL ] && printf '       want(%s)=[%s]\n' "$mode" "$want"
}

echo "=== discovery and a first call"
t 'bash is bound to ClankAgent'          contains 'bash v0.1.0'   'probe-tool tools'
t 'first call runs a script'             contains 'hello-from-bash' "probe-tool bash 'echo hello-from-bash'"
t 'first call returns exit 0 + state'    contains '[bash exit=0'  "probe-tool bash 'true'"

echo "=== state carried by the caller"
t 'reset'                                contains ''              'probe-tool bash-reset'
t 'call 1: cd, var, fn, alias, exit 4'   contains 'exit=4'        "probe-tool bash 'cd /tmp; x=kept; f() { echo fn-kept; }; alias ll=\"echo alias-kept\"; (exit 4)'"
t 'call 2: $? from call 1'               contains 'status=4'      "probe-tool bash 'echo status=\$?'"
t 'call 3: cwd carried'                  contains 'cwd=/tmp'      "probe-tool bash 'pwd'"
t 'call 4: var, fn, alias carried'       contains 'fn-kept'       "probe-tool bash 'echo var=\$x; f; alias ll'"
t 'reset, then a fresh shell'            contains '[]'            "probe-tool bash-reset; probe-tool bash 'echo [\$x]'"

echo "=== filesystem ($PHASE)"
t 'bash writes a file'                   contains 'exit='         "probe-tool bash 'echo from-bash > /bash-wrote.txt; cat /bash-wrote.txt; ls /'"
t 'owner sees the file bash wrote'       contains 'from-bash'     'cat /bash-wrote.txt'
t 'owner writes, bash reads'             contains 'from-owner'    "echo from-owner > /owner-wrote.txt; probe-tool bash 'cat /owner-wrote.txt'"

echo "=== tools from inside bash (tool-to-tool from a sidecar)"
t 'echo-tool greet from inside bash'     contains 'hello Ada'     "probe-tool bash 'probe-tool greet Ada'"
t 'fs-capable tool from inside bash'     contains 'nested-hi'     "probe-tool bash 'probe-tool cwrite /nested.txt nested-hi; probe-tool cread /nested.txt'"
t 'owner sees the nested tool write'     contains 'nested-hi'     'cat /nested.txt'

echo "=== per-call cost (CLI wall clock; plain eval vs a bash tool call)"
for i in 1 2 3; do t "plain eval #$i" contains 'p' 'echo p'; done
for i in 1 2 3; do t "bash tool call #$i" contains 'q' "probe-tool bash 'echo q'"; done

echo "=== crash and replay"
t 'state before crash'                   contains 'exit=0'        "probe-tool bash-reset; probe-tool bash 'y=survives; cd /tmp'"
"$GOLEM_BIN" agent simulate-crash "$AGENT" >> "$GOLEM_JSON_LOG" 2>&1
echo "simulate-crash exit $?"
t 'state after replay'                   contains 'y=survives'    "probe-tool bash 'echo y=\$y; pwd'"

echo "=== server log: replay / divergence / panic / trap lines"
if [ -n "${SERVER_LOG:-}" ] && [ -f "$SERVER_LOG" ]; then
  grep -i -E 'diverge|mismatch|panic|trap|unexpected oplog' "$SERVER_LOG" | tail -15
fi
echo "=== $pass passed, $fail failed"
