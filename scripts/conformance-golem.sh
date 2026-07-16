#!/usr/bin/env bash
#
# conformance-golem.sh — run the golem tier of the conformance suite against a throwaway server.
#
# Stands up a local Golem server, deploys clank, then runs every scenarios/*.clank through
# `cargo test -p clank-conformance --test golem` (each scenario drives a FRESH agent instance
# via `golem agent invoke`). The server lifecycle is a straight copy of the proven
# scripts/golem-e2e.sh blocks — deliberately not shared/refactored so the e2e stays untouched.
#
# Usage:   scripts/conformance-golem.sh [--takeover] [--keep] [-- <libtest args>]
#   --takeover   kill any golem server already bound to the router port instead of refusing.
#   --keep       leave the server + data dir up after the run (per-scenario agents make
#                post-mortems cheap: `golem agent list`).
#   -- <args>    passed through to the test binary, e.g. `-- pipelines` for one scenario,
#                `-- --test-threads=4` once concurrency is proven on your server.
#
# Env: GOLEM_BIN (default `golem`), JOBS (default 1 → --test-threads),
#      CLANK_CONFORMANCE_STEP_TIMEOUT_SECS (per-invoke timeout, default 60).
#
# Requires: golem (>=1.5), cargo. Run from anywhere inside the repo.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT" || { echo "cannot cd to repo root $REPO_ROOT" >&2; exit 1; }

ROUTER_PORT=9881
KEEP=0
TAKEOVER=0
PASSTHRU=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --keep)     KEEP=1; shift ;;
    --takeover) TAKEOVER=1; shift ;;
    --)         shift; PASSTHRU=("$@"); break ;;
    -h|--help)  sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1 (use -- to pass args to the test binary)" >&2; exit 2 ;;
  esac
done

GOLEM="${GOLEM_BIN:-golem}"
command -v "$GOLEM" >/dev/null 2>&1 || { echo "golem CLI not found (need >=1.5; set GOLEM_BIN to override)" >&2; exit 1; }
command -v cargo >/dev/null 2>&1 || { echo "cargo not found" >&2; exit 1; }

# golem.yaml passes ANTHROPIC_API_KEY through via STRICT Jinja substitution — a missing host
# var fails the deploy, so export an empty default. No conformance scenario calls the model.
export ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}"

c_dim=$'\033[2m'; c_red=$'\033[31m'; c_rst=$'\033[0m'
note() { echo "${c_dim}··${c_rst} $*"; }
step() { echo; echo "▸ $*"; }
warn() { echo "${c_red}$*${c_rst}" >&2; }

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/clank-conformance.XXXXXX")"
PORTS_FILE="$DATA_DIR/ports.json"
SERVER_LOG="$DATA_DIR/server.log"
SERVER_PID=""

# `golem build`/`deploy` re-append a `<!-- golem-managed -->` section to AGENTS.md every run.
# Snapshot it up front so teardown restores the tree exactly (clean stays clean).
AGENTS_BACKUP="$DATA_DIR/AGENTS.md.orig"
[[ -f AGENTS.md ]] && cp AGENTS.md "$AGENTS_BACKUP"

cleanup() {
  local ec=$?
  # Disarm the traps so the `exit` below cannot re-enter cleanup (INT/TERM arrive as
  # their own handlers that `exit` with the conventional code, which fires this EXIT
  # trap exactly once with $? already set).
  trap - EXIT INT TERM
  [[ -f "$AGENTS_BACKUP" ]] && cp "$AGENTS_BACKUP" AGENTS.md
  if [[ $KEEP -eq 1 ]]; then
    echo
    note "--keep: leaving server (pid ${SERVER_PID:-?}) and data dir $DATA_DIR"
    note "poke at the per-scenario agents with: $GOLEM agent list"
    note "stop the server later with:  kill ${SERVER_PID:-<pid>}"
    exit $ec
  fi
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
# INT/TERM exit with the conventional codes (130/143) so a Ctrl-C can never read as
# success; the EXIT trap then runs cleanup once with that status.
trap 'exit 130' INT
trap 'exit 143' TERM
trap cleanup EXIT

step "Building the wasm component (golem build)"
"$GOLEM" -Y build 2>&1 | tail -4 || { warn "golem build failed"; exit 1; }

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
    # Settle-and-verify after the kill -9: racing a dying server produces a misleading
    # "server exited early" later instead of an honest failure here.
    for _ in 1 2 3 4 5 6 7 8 9 10; do [[ -z "$(port_pids)" ]] && break; sleep 0.3; done
    if [[ -n "$(port_pids)" ]]; then
      warn "could not free port $ROUTER_PORT (still held by: $(port_pids))"
      exit 1
    fi
    note "port $ROUTER_PORT freed"
  else
    warn "A golem server is already on port $ROUTER_PORT (pid $existing). Re-run with --takeover."
    exit 1
  fi
fi

step "Starting throwaway golem server (data dir: $DATA_DIR)"
"$GOLEM" server run --clean --router-port "$ROUTER_PORT" --data-dir "$DATA_DIR" --ports-file "$PORTS_FILE" \
  >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
note "server pid $SERVER_PID — waiting for it to come up..."
for i in $(seq 1 60); do
  kill -0 "$SERVER_PID" 2>/dev/null || { warn "server exited early — see $SERVER_LOG"; tail -20 "$SERVER_LOG" >&2; exit 1; }
  if [[ -f "$PORTS_FILE" ]] && "$GOLEM" component list >/dev/null 2>&1; then note "server ready after ${i}s"; break; fi
  sleep 1
  [[ $i -eq 60 ]] && { warn "server did not become ready in 60s"; tail -20 "$SERVER_LOG" >&2; exit 1; }
done

step "Deploying clank:agent (golem deploy)"
"$GOLEM" -Y deploy 2>&1 | tail -5 || { warn "golem deploy failed"; exit 1; }

step "Running the golem conformance tier"
# ${arr[@]+...} (not a bare "${arr[@]}") — macOS ships bash 3.2, where expanding an empty
# array under `set -u` is an unbound-variable error. And only inject the default
# --test-threads when the passthrough doesn't set one: the test binary rejects the flag
# stated twice.
threads_default=(--test-threads="${JOBS:-1}")
for arg in ${PASSTHRU[@]+"${PASSTHRU[@]}"}; do
  case "$arg" in
    --test-threads|--test-threads=*) threads_default=() ;;
  esac
done
CLANK_CONFORMANCE_GOLEM=1 GOLEM_BIN="$GOLEM" \
  cargo test -p clank-conformance --test golem -- ${threads_default[@]+"${threads_default[@]}"} ${PASSTHRU[@]+"${PASSTHRU[@]}"}
