#!/usr/bin/env bash
#
# golem-probe.sh — deploy clank:agent to a throwaway Golem server and run ad-hoc command lines.
#
# A debugging companion to golem-e2e.sh: same server/deploy/teardown machinery, but instead of
# fixed assertions it invokes `run_line` for each command line you pass and prints the raw output,
# then dumps the tail of the server log (where agent-side panics / errors surface). Handy for
# reproducing a single failing case with full stderr, without editing the e2e.
#
# Usage:   scripts/golem-probe.sh [--takeover] [--keep] [--stderr] 'cmd 1' 'cmd 2' ...
#   --takeover   kill any golem server already bound to the default port first (else refuse).
#   --keep       leave the server + data dir up after the run (poke at the agent manually).
#   --stderr     after each command, print the full agent stderr for that invocation (not just
#                the run_line String, which merges stdout+stderr). Uses the `eval` method's JSON.
#   Each remaining argument is one shell command line run on the agent, in order, on ONE agent
#   instance (so state — cwd, files, transcript — persists across them, like a session).
#
# Requires: golem (>=1.5), cargo, jq. Run from anywhere inside the repo.

set -uo pipefail

# --- locate the repo root (dir containing golem.yaml) so the script is cwd-independent ---
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT" || { echo "cannot cd to repo root $REPO_ROOT" >&2; exit 1; }

TAKEOVER=0
KEEP=0
WANT_STDERR=0
CMDS=()
for arg in "$@"; do
  case "$arg" in
    --takeover) TAKEOVER=1 ;;
    --keep)     KEEP=1 ;;
    --stderr)   WANT_STDERR=1 ;;
    -h|--help)  sed -n '2,19p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    --*) echo "unknown option: $arg" >&2; echo "usage: $0 [--takeover] [--keep] [--stderr] 'cmd' ..." >&2; exit 2 ;;
    *) CMDS+=("$arg") ;;
  esac
done

if [[ ${#CMDS[@]} -eq 0 ]]; then
  # Default probe set: the pipeline cases that failed in the e2e, from simplest to most involved.
  CMDS=(
    'printf "a\nb\n"'
    'echo hi | grep hi'
    'printf "a\nb\nc\n" | grep b'
    'cat /proc/1/status | grep State'
    'ls /bin | grep curl'
  )
fi

# Same default router port as golem-e2e.sh — the CLI profile targets it, so our server must bind it.
ROUTER_PORT=9881

# ask needs ANTHROPIC_API_KEY set for the strict Jinja substitution in golem.yaml; empty is fine here.
export ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}"

AGENT_TYPE="ClankAgent"
AGENT_NAME="probe-$$"
AGENT_ID="${AGENT_TYPE}(\"${AGENT_NAME}\")"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/clank-golem-probe.XXXXXX")"
PORTS_FILE="$DATA_DIR/ports.json"
SERVER_LOG="$DATA_DIR/server.log"
SERVER_PID=""

c_grn=$'\033[32m'; c_red=$'\033[31m'; c_dim=$'\033[2m'; c_cyn=$'\033[36m'; c_rst=$'\033[0m'
note() { echo "${c_dim}··${c_rst} $*"; }
step() { echo; echo "▸ $*"; }

# Snapshot AGENTS.md so build/deploy churn doesn't leak into the tree (same as the e2e).
AGENTS_BACKUP="$DATA_DIR/AGENTS.md.orig"
[[ -f AGENTS.md ]] && cp AGENTS.md "$AGENTS_BACKUP"

cleanup() {
  local ec=$?
  [[ -f "$AGENTS_BACKUP" ]] && cp "$AGENTS_BACKUP" AGENTS.md
  if [[ $KEEP -eq 1 ]]; then
    echo
    note "--keep: server pid ${SERVER_PID:-?}, data dir $DATA_DIR, log $SERVER_LOG"
    note "stop it later with:  kill ${SERVER_PID:-<pid>}"
    exit $ec
  fi
  step "Tearing down"
  if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null
    for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$SERVER_PID" 2>/dev/null || break; sleep 0.3; done
    kill -9 "$SERVER_PID" 2>/dev/null
    note "server stopped"
  fi
  rm -rf "$DATA_DIR"
  # `exit` (not `return`) in an EXIT trap is what actually sets the final status.
  exit $ec
}
trap cleanup EXIT INT TERM

# --- resolve a server already bound to the default port ---
port_pids() { lsof -tiTCP:"$ROUTER_PORT" -sTCP:LISTEN 2>/dev/null; }
existing="$(port_pids)"
if [[ -n "$existing" ]]; then
  if [[ $TAKEOVER -eq 1 ]]; then
    step "Taking over port $ROUTER_PORT (killing existing server: $existing)"
    # shellcheck disable=SC2086
    kill $existing 2>/dev/null
    for _ in 1 2 3 4 5 6 7 8 9 10; do [[ -z "$(port_pids)" ]] && break; sleep 0.3; done
    # shellcheck disable=SC2086
    [[ -n "$(port_pids)" ]] && kill -9 $(port_pids) 2>/dev/null
    note "port $ROUTER_PORT freed"
  else
    echo "${c_red}A golem server is already on port $ROUTER_PORT (pid $existing).${c_rst}" >&2
    echo "  Re-run with --takeover to kill it automatically." >&2
    exit 1
  fi
fi

step "Building the wasm component (golem build)"
if ! golem -Y build 2>&1 | tail -4; then
  echo "${c_red}golem build failed${c_rst}" >&2
  exit 1
fi

step "Starting throwaway golem server (data dir: $DATA_DIR)"
golem server run --clean --router-port "$ROUTER_PORT" --data-dir "$DATA_DIR" --ports-file "$PORTS_FILE" \
  >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
note "server pid $SERVER_PID"

note "waiting for server to come up..."
for i in $(seq 1 60); do
  kill -0 "$SERVER_PID" 2>/dev/null || { echo "${c_red}server exited early — see below${c_rst}" >&2; tail -25 "$SERVER_LOG" >&2; exit 1; }
  if [[ -f "$PORTS_FILE" ]] && golem component list >/dev/null 2>&1; then
    note "server ready after ${i}s"
    break
  fi
  sleep 1
  [[ $i -eq 60 ]] && { echo "${c_red}server did not become ready in 60s${c_rst}" >&2; tail -25 "$SERVER_LOG" >&2; exit 1; }
done

step "Deploying clank:agent (golem deploy)"
if ! golem -Y deploy 2>&1 | tail -5; then
  echo "${c_red}golem deploy failed${c_rst}" >&2
  exit 1
fi

# --- invoke run_line for a command and print its (stdout+stderr merged) String result ---
run_line() {
  local cmd="$1"
  golem agent invoke -q --format json "$AGENT_ID" run_line "\"${cmd//\"/\\\"}\"" 2>>"$SERVER_LOG" \
    | grep '"result_json"' | tail -1 | jq -r '.result_json.value // empty' 2>/dev/null
}

# --- invoke eval for a command and return the structured EvalResult JSON (stdout/stderr/exit split) ---
eval_json() {
  local cmd="$1"
  golem agent invoke -q --format json "$AGENT_ID" eval "\"${cmd//\"/\\\"}\"" 2>>"$SERVER_LOG" \
    | grep '"result_json"' | tail -1 | jq -c '.result_json.value // empty' 2>/dev/null
}

step "Probing ${#CMDS[@]} command(s) on agent $AGENT_ID"
LOG_MARK=0
for cmd in "${CMDS[@]}"; do
  echo
  echo "${c_cyn}\$ ${cmd}${c_rst}"
  if [[ $WANT_STDERR -eq 1 ]]; then
    j="$(eval_json "$cmd")"
    echo "  stdout:    $(printf '%s' "$j" | jq -r '.stdout // ""' 2>/dev/null | sed 's/^/  /' )"
    echo "  stderr:    $(printf '%s' "$j" | jq -r '.stderr // ""' 2>/dev/null)"
    echo "  exit_code: $(printf '%s' "$j" | jq -r '.exit_code // ""' 2>/dev/null)"
  else
    out="$(run_line "$cmd")"
    printf '%s\n' "$out" | sed 's/^/  /'
  fi
  # Show any NEW server-log lines this invocation produced (panics/tracing land here).
  new="$(tail -n +$((LOG_MARK+1)) "$SERVER_LOG" 2>/dev/null | grep -iE 'panic|error|unsupported|thread .* panicked' || true)"
  [[ -n "$new" ]] && { echo "  ${c_dim}server-log:${c_rst}"; printf '%s\n' "$new" | sed 's/^/    /'; }
  LOG_MARK=$(wc -l <"$SERVER_LOG" 2>/dev/null || echo 0)
done

echo
step "Server log tail"
tail -15 "$SERVER_LOG"
echo
note "done"
