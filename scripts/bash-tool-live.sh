#!/usr/bin/env bash
# Acceptance: tools as commands through a disposable local Golem server.
# A minimal BashHost agent invokes bash-tool and carries its returned state.
# Tears down the disposable server at the end.
set -uo pipefail

C="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
S="$C/target/bash-split"
mkdir -p "$S"
LIB="$C/scripts/lib/golem-json.sh"
export GOLEM_BIN="${GOLEM_BIN:-$HOME/Desktop/clank.sh/golem-stuff/golem/target/debug/golem}"
export GOLEM_JSON_LOG="$S/bash-live-cli-stderr.log"
: > "$GOLEM_JSON_LOG"
PHASE="${1:-tools-as-commands}"

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

AGENT_ID='BashHost("tools-acceptance")'
. "$LIB"
run_json() {
  local cmd=$1 method=${2:-eval}
  local arg
  arg=$(jq -Rn --arg cmd "$cmd" '$cmd')
  golem_eval_json "$method" "$arg"
}
pass=0; fail=0
printf 'check,cli_wall_clock_ms\n' > "$S/bash-live-latency-$PHASE.csv"
check() {
  local label=$1 line=$2 predicate=$3 method=${4:-eval} got started elapsed
  started=$(python3 -c 'import time; print(time.time_ns())')
  got=$(run_json "$line" "$method")
  elapsed=$(python3 -c 'import sys,time; print(round((time.time_ns()-int(sys.argv[1]))/1e6))' "$started")
  printf '%s,%s\n' "$label" "$elapsed" >> "$S/bash-live-latency-$PHASE.csv"
  if printf '%s' "$got" | jq -e "$predicate" >/dev/null 2>&1; then
    pass=$((pass+1)); printf 'PASS %-48s %sms\n' "$label" "$elapsed"
  else
    fail=$((fail+1)); printf 'FAIL %s: %s\n' "$label" "$got"
  fi
}
check 'discovery/help' 'echo-tool --help' '.exit_code == 0 and (.stdout | contains("greet"))'
check 'canonical scalar/default/global arguments' 'echo-tool greet -vv Ada -n2 --shout' '.exit_code == 0 and .stdout == "HELLO ADA HELLO ADA [color=auto verbose=2]" and .stderr == ""'
check 'stdin and stdout pipe' 'printf "hello\\n" | echo-tool upper | tr A-Z a-z' '.exit_code == 0 and .stdout == "hello\n" and .stderr == ""'
check 'command substitution' 'printf "[%s]" "$(echo-tool greet Ada)"' '.exit_code == 0 and .stdout == "[hello Ada [color=auto verbose=0]]"'
check 'environment count and boolean flags' 'export ECHO_VERBOSE=3 ECHO_SHOUT=yes; echo-tool hi Ada' '.exit_code == 0 and .stdout == "HELLO ADA [color=auto verbose=3]"'
check 'CLI flags override environment' 'echo-tool greet Ada -v --no-shout; unset ECHO_VERBOSE ECHO_SHOUT' '.exit_code == 0 and .stdout == "hello Ada [color=auto verbose=1]"'
check 'stdin-backed positional implicit' 'printf "one\\ntwo\\n" | echo-tool stdin-arg' '.exit_code == 0 and .stdout == "one\ntwo\n" and .stderr == ""'
check 'stdin-backed positional explicit dash' 'printf "one\\ntwo\\n" | echo-tool stdin-arg -' '.exit_code == 0 and .stdout == "one\ntwo\n"'
check 'structured list renders as JSON' 'echo-tool list' '.exit_code == 0 and (.stdout | fromjson) == ["alpha","beta"]'
check 'repeatable map and record subtree' 'echo-tool t add -c a=1 -c b=2 --meta "{\"k\":\"x\",\"n\":2}"' '.exit_code == 0 and (.stdout | fromjson) == {"k":"x+a,b","n":4}'
check 'named usage error exit 2' 'echo-tool fail --usage' '.exit_code == 2 and (.stderr | contains("bad-input"))'
check 'named runtime error exit 7' 'echo-tool fail' '.exit_code == 7 and (.stderr | contains("boom"))'
check 'usage errors stay on stderr' 'echo-tool greet Ada --unknown 2>/usage.txt; cat /usage.txt' '.exit_code == 0 and (.stdout | contains("unknown option"))'
check 'mutating command requests confirmation' 'capable-echo write /nested.txt nested-hi' '.pending_prompt != null'
check 'answer authorizes nested filesystem write' 'yes' '.exit_code == 0 and .pending_prompt == null' answer_prompt
check 'nested filesystem read' 'capable-echo read /nested.txt' '.exit_code == 0 and (.stdout | contains("nested-hi"))'
check 'owner sees the same shared filesystem' 'cat /nested.txt' '.exit_code == 0 and (.stdout | contains("nested-hi"))' owner_eval
check 'owner writes file' 'echo owner-hi > /owner.txt' '.exit_code == 0' owner_eval
check 'shell reads owner file' 'cat /owner.txt' '.exit_code == 0 and (.stdout | contains("owner-hi"))'
check 'hidden mutating command is refused' 'f() { echo-tool destroy; }; f' '.exit_code == 3 and (.stderr | contains("confirmation"))'
check 'background jobs are refused' 'echo unsafe &' '.exit_code == 2 and (.stderr | contains("background"))'
check 'expanded eval background is refused' 'code="echo unsafe &"; eval "$code"' '.exit_code == 2 and .stdout == "" and (.stderr | contains("background"))'
check 'trap background is refused' 'trap "echo unsafe &" EXIT' '.exit_code == 2 and .stdout == "" and (.stderr | contains("background"))'
check 'alias background is refused' 'alias unsafe="echo unsafe &"' '.exit_code == 2 and .stdout == "" and (.stderr | contains("background"))'
check 'write a script for source refusal' 'printf "%s" "echo unsafe &" > /background.sh' '.exit_code == 0'
check 'sourced background is refused' 'source /background.sh' '.exit_code == 2 and .stdout == "" and (.stderr | contains("background"))'
check 'state variables/functions/options' 'x=kept; f() { echo fn-kept; }; set -o pipefail; (exit 4)' '.exit_code == 4'
check 'restored status and variables' 'echo status=$?; echo $x; f; set -o | grep pipefail' '.exit_code == 0 and (.stdout | contains("status=4")) and (.stdout | contains("fn-kept"))'
check 'prompt is carried across fresh stores' 'prompt-user "choose" --choices yes,no' '.pending_prompt.choices == ["yes","no"]'
check 'invalid answer retains prompt' 'invalid' '.pending_prompt != null and .exit_code != 0' answer_prompt
check 'valid answer clears prompt' 'yes' '.exit_code == 0 and .pending_prompt == null and (.stdout | contains("yes"))' answer_prompt
check 'denial does not run a mutating tool' 'echo-tool destroy' '.pending_prompt != null'
check 'deny pending confirmation' 'no' '.exit_code == 5 and .pending_prompt == null' answer_prompt
check 'state before recovery' 'y=survives; echo-tool greet replay > /replayed.txt' '.exit_code == 0'
"$GOLEM_BIN" agent simulate-crash "$AGENT_ID" >>"$GOLEM_JSON_LOG" 2>&1 || exit 1
check 'state and regenerated stdout after recovery' 'echo $y; cat /replayed.txt; capable-echo read /nested.txt' '.exit_code == 0 and (.stdout | contains("survives")) and (.stdout | contains("replay")) and (.stdout | contains("nested-hi"))'
for i in 1 2 3; do check "steady shell call #$i (CLI wall clock)" 'echo steady' '.exit_code == 0'; done
printf '%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
