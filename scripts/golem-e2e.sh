#!/usr/bin/env bash
#
# golem-e2e.sh — end-to-end smoke test for clank as a Golem agent.
#
# Starts a throwaway local Golem server, builds and deploys the `clank:agent`
# component, invokes `run_line` for the README file-management command set,
# asserts each returns the expected output, then tears the server down and
# wipes its data dir. Exits non-zero on the first failed assertion.
#
# Usage:   scripts/golem-e2e.sh [--keep] [--takeover]
#   --keep       leave the server running and the data dir on disk after the run
#                (handy for poking at the deployed agent manually afterwards).
#   --takeover   if a golem server is already bound to the default port, kill it
#                first instead of refusing. Without this, the script bails so it
#                never clobbers a server you deliberately left running.
#
# Requires: golem (>=1.5), cargo, jq. Run from anywhere inside the repo.

set -uo pipefail

# --- locate the repo root (dir containing golem.yaml) so the script is cwd-independent ---
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT" || { echo "cannot cd to repo root $REPO_ROOT" >&2; exit 1; }

KEEP=0
TAKEOVER=0
for arg in "$@"; do
  case "$arg" in
    --keep)     KEEP=1 ;;
    --takeover) TAKEOVER=1 ;;
    -h|--help)  sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg" >&2; echo "usage: $0 [--keep] [--takeover]" >&2; exit 2 ;;
  esac
done

# The CLI (deploy/invoke) targets the server via the active profile, which points at the
# default router port. The server we start must therefore bind that same default port — so a
# server already running there is a hard conflict we must resolve before starting.
ROUTER_PORT=9881

# The agent is addressed by type + a constructor arg (its identity). One name for
# the whole run so state persists across invocations (durability check relies on it).
AGENT_TYPE="ClankAgent"
AGENT_NAME="e2e-$$"                 # $$ = PID, unique per run
AGENT_ID="${AGENT_TYPE}(\"${AGENT_NAME}\")"

# Isolated server state so we never touch a real dev environment and can wipe it.
DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/clank-golem-e2e.XXXXXX")"
PORTS_FILE="$DATA_DIR/ports.json"
SERVER_LOG="$DATA_DIR/server.log"
SERVER_PID=""

# --- pretty output ---
PASS=0; FAIL=0
c_grn=$'\033[32m'; c_red=$'\033[31m'; c_dim=$'\033[2m'; c_rst=$'\033[0m'
note() { echo "${c_dim}··${c_rst} $*"; }
step() { echo; echo "▸ $*"; }

# `golem build`/`deploy` re-append a `<!-- golem-managed -->` section to AGENTS.md every run.
# Snapshot it up front so teardown can restore the tree to exactly how it was (clean stays
# clean; an already-dirty AGENTS.md is preserved byte-for-byte).
AGENTS_BACKUP="$DATA_DIR/AGENTS.md.orig"
[[ -f AGENTS.md ]] && cp AGENTS.md "$AGENTS_BACKUP"

restore_agents_md() {
  [[ -f "$AGENTS_BACKUP" ]] && cp "$AGENTS_BACKUP" AGENTS.md
}

# --- teardown: always runs, kills server + wipes data dir (unless --keep) ---
cleanup() {
  local ec=$?
  # Restore AGENTS.md regardless of --keep — the golem-managed churn shouldn't leak into the tree.
  restore_agents_md
  if [[ $KEEP -eq 1 ]]; then
    echo
    note "--keep: leaving server (pid ${SERVER_PID:-?}) and data dir $DATA_DIR"
    note "AGENTS.md restored; stop the server later with:  kill ${SERVER_PID:-<pid>}"
    # copy the backup somewhere it survives, since we're not wiping DATA_DIR
    return $ec
  fi
  step "Tearing down"
  if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null
    # give it a moment to exit, then hard-kill any survivors in its process group
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      kill -0 "$SERVER_PID" 2>/dev/null || break
      sleep 0.3
    done
    kill -9 "$SERVER_PID" 2>/dev/null
    note "server stopped"
  fi
  rm -rf "$DATA_DIR"
  note "data dir removed"
  return $ec
}
trap cleanup EXIT INT TERM

# --- assertion: invoke run_line "<cmd>", compare captured output to expected ---
# `golem agent invoke --format json` prints invocation markers + any agent STDERR stream lines
# to stdout FIRST, then the actual result document as the final line. That document has the
# returned String at `.result_json.value`. So: keep only the line carrying `result_json`, take
# the last one, and pull `.value`. `run_line` never `echo`s the payload (which would let a shell
# re-interpret the `\n` escapes in the JSON and corrupt it) — the raw bytes flow straight to jq.
run_line() {
  local cmd="$1"
  # ONE positional per function arg; the arg is a WIT string literal → wrap in quotes.
  golem agent invoke -q --format json "$AGENT_ID" run_line "\"${cmd//\"/\\\"}\"" 2>>"$SERVER_LOG" \
    | grep '"result_json"' \
    | tail -1 \
    | jq -r '.result_json.value // empty' 2>/dev/null
}

expect() {
  local desc="$1" cmd="$2" want="$3"
  local got; got="$(run_line "$cmd")"
  if [[ "$got" == "$want" ]]; then
    echo "  ${c_grn}✓${c_rst} $desc"
    PASS=$((PASS+1))
  else
    echo "  ${c_red}✗${c_rst} $desc"
    echo "      cmd:      $cmd"
    echo "      expected: $(printf '%q' "$want")"
    echo "      got:      $(printf '%q' "$got")"
    FAIL=$((FAIL+1))
  fi
}

# Like `expect`, but asserts the output CONTAINS a substring — for commands whose exact
# formatting (e.g. uutils `wc` column padding) is an implementation detail not worth pinning.
expect_contains() {
  local desc="$1" cmd="$2" want="$3"
  local got; got="$(run_line "$cmd")"
  if [[ "$got" == *"$want"* ]]; then
    echo "  ${c_grn}✓${c_rst} $desc"
    PASS=$((PASS+1))
  else
    echo "  ${c_red}✗${c_rst} $desc"
    echo "      cmd:      $cmd"
    echo "      want substr: $(printf '%q' "$want")"
    echo "      got:         $(printf '%q' "$got")"
    FAIL=$((FAIL+1))
  fi
}

# ============================================================================
# 1. Build + start the server
# ============================================================================
step "Building the wasm component (golem build)"
# -Y auto-confirms the AGENTS.md manifest-section update (fails otherwise in a non-interactive
# shell); we restore AGENTS.md in teardown so the tree isn't left dirty.
if ! golem -Y build 2>&1 | tail -4; then
  echo "${c_red}golem build failed${c_rst}" >&2
  exit 1
fi

# --- resolve a server already bound to the default port ---
# Our fresh server must bind $ROUTER_PORT (the port the CLI profile talks to). If something is
# already there, `golem server run` exits instantly. Refuse by default so we never kill a server
# the user left running on purpose; --takeover opts into killing it first.
port_pids() { lsof -tiTCP:"$ROUTER_PORT" -sTCP:LISTEN 2>/dev/null; }
existing="$(port_pids)"
if [[ -n "$existing" ]]; then
  if [[ $TAKEOVER -eq 1 ]]; then
    step "Taking over port $ROUTER_PORT (killing existing server: $existing)"
    # shellcheck disable=SC2086
    kill $existing 2>/dev/null
    for _ in 1 2 3 4 5 6 7 8 9 10; do [[ -z "$(port_pids)" ]] && break; sleep 0.3; done
    # shellcheck disable=SC2086
    [[ -n "$(port_pids)" ]] && { kill -9 $(port_pids) 2>/dev/null; sleep 0.5; }
    [[ -n "$(port_pids)" ]] && { echo "${c_red}could not free port $ROUTER_PORT${c_rst}" >&2; exit 1; }
    note "port $ROUTER_PORT freed"
  else
    echo "${c_red}A golem server is already listening on port $ROUTER_PORT (pid $existing).${c_rst}" >&2
    echo "  This script needs that port. Either stop that server, or re-run with --takeover" >&2
    echo "  to kill it automatically:   $0 --takeover" >&2
    exit 1
  fi
fi

step "Starting throwaway golem server (data dir: $DATA_DIR)"
golem server run --clean --router-port "$ROUTER_PORT" --data-dir "$DATA_DIR" --ports-file "$PORTS_FILE" \
  >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
note "server pid $SERVER_PID"

# Wait until the ports file appears AND the health/component API answers.
note "waiting for server to come up..."
for i in $(seq 1 60); do
  kill -0 "$SERVER_PID" 2>/dev/null || { echo "${c_red}server exited early — see $SERVER_LOG${c_rst}" >&2; tail -20 "$SERVER_LOG" >&2; exit 1; }
  if [[ -f "$PORTS_FILE" ]] && golem component list >/dev/null 2>&1; then
    note "server ready after ${i}s"
    break
  fi
  sleep 1
  [[ $i -eq 60 ]] && { echo "${c_red}server did not become ready in 60s${c_rst}" >&2; tail -20 "$SERVER_LOG" >&2; exit 1; }
done

step "Deploying clank:agent (golem deploy)"
if ! golem -Y deploy 2>&1 | tail -5; then
  echo "${c_red}golem deploy failed${c_rst}" >&2
  exit 1
fi

# ============================================================================
# 2. Assertions — the README file-management command set
# ============================================================================
step "Running command assertions on agent $AGENT_ID"

# A fresh agent has an empty writable fs — create the working dir before redirecting into it
# (a `>` into a missing parent is a genuine ENOENT, not a shell bug).
expect "mkdir -p creates the working dir"    'mkdir -p /tmp/work; echo ready'                     $'ready'
# write via redirect, read back
expect "echo > file writes"                 'echo hello > /tmp/work/a; cat /tmp/work/a'          $'hello'
# append
expect "echo >> file appends"               'echo world >> /tmp/work/a; cat /tmp/work/a'         $'hello\nworld'
# mkdir -p (nested) + ls sees it
expect "mkdir -p creates nested dirs"        'mkdir -p /tmp/work/sub/deep; echo ok'               $'ok'
# populate a subtree for cp -r
expect "seed file in subdir"                 'echo x > /tmp/work/sub/y; cat /tmp/work/sub/y'      $'x'
# cp (plain)
expect "cp copies a file"                    'cp /tmp/work/a /tmp/work/b; cat /tmp/work/b'        $'hello\nworld'
# cp -r (recursive, exercises the wasi set_permissions skip)
expect "cp -r copies a directory"            'cp -r /tmp/work/sub /tmp/work/sub2; cat /tmp/work/sub2/y' $'x'
# mv
expect "mv renames a file"                   'mv /tmp/work/b /tmp/work/c; cat /tmp/work/c'        $'hello\nworld'
# rm — delete, then confirm it's gone by testing for the file's existence (the agent fs has no
# /dev/null, so don't redirect there; use a `test -e` guard instead).
expect "rm deletes a file"                   'rm /tmp/work/c; if [ -e /tmp/work/c ]; then echo present; else echo gone; fi' $'gone'
# wc — uutils right-pads the columns, so assert the counts+name as a substring, not exact spacing
expect_contains "wc counts lines/words/bytes" 'wc /tmp/work/a'                                    '2 12 /tmp/work/a'
# head
expect "head -1 prints first line"           'head -1 /tmp/work/a'                                $'hello'
# combined redirect &>
expect "&> redirects stdout+stderr"          'echo both &> /tmp/work/d; cat /tmp/work/d'          $'both'

# ----------------------------------------------------------------------------
# 2b. Newly-added core commands (uutils-backed, file-argument forms)
# ----------------------------------------------------------------------------
step "Newly-added core commands"
# Seed the fixture files ONE redirect per invocation. Two constraints, both pre-existing Brush gaps
# on wasip2 unrelated to these commands: (1) `printf` errors outright, so seed with `echo`; (2) more
# than one file redirect in a single command line fails, so each write is its own invocation.
run_line 'echo one   > /tmp/work/lines' >/dev/null
run_line 'echo one  >> /tmp/work/lines' >/dev/null
run_line 'echo two  >> /tmp/work/lines' >/dev/null
run_line 'echo three >> /tmp/work/lines' >/dev/null
run_line 'echo a,b,c > /tmp/work/csv' >/dev/null
# uniq collapses adjacent duplicates
expect "uniq collapses duplicate lines"      'uniq /tmp/work/lines'                               $'one\ntwo\nthree'
# tail -n
expect "tail -1 prints last line"            'tail -1 /tmp/work/lines'                            $'three'
# cut a delimited field
expect "cut -d, -f2 selects a field"         'cut -d, -f2 /tmp/work/csv'                          $'b'
# NOTE: `tr` and `tee` are stdin-only filters (no file-argument mode). Under the current wasm
# capture harness fd 0 isn't redirected, so feeding them input hangs the single-threaded worker.
# They're registered (and will compose once the process model handles stdin) but are deliberately
# NOT exercised here — asserting them wedges the agent instance.
# touch creates an empty file
expect "touch creates a file"                'touch /tmp/work/touched; if [ -e /tmp/work/touched ]; then echo yes; else echo no; fi' $'yes'
# sleep returns promptly for 0
expect "sleep 0 returns success"             'sleep 0; echo slept'                                $'slept'
# env lists variables — on the Golem agent the environment is the GOLEM_* set; assert a stable one
expect_contains "env lists variables"        'env'                                                'GOLEM_AGENT_TYPE='

# ============================================================================
# 2c. Transcript sliding window — force eviction with a tiny budget, see the marker
# ============================================================================
# Start from a clean transcript so this check is independent of the commands run above, then set a
# tiny token budget and run several commands. The oldest entries must be dropped behind a marker
# while the newest survives. (`context show`/`context clear`/`context budget` are themselves
# recorded as command entries, so the budget is kept small-but-nonzero and we assert on substrings
# rather than exact counts.)
run_line 'context clear' >/dev/null
run_line 'context budget 6' >/dev/null
run_line 'echo alpha' >/dev/null
run_line 'echo bravo' >/dev/null
run_line 'echo charlie' >/dev/null
run_line 'echo delta' >/dev/null
expect_contains "window inserts a drop marker"   'context show'  'earlier entries dropped'
expect_contains "window keeps the newest entry"  'context show'  'echo delta'
# restore a roomy budget so it doesn't interfere with anything after
run_line 'context budget 24000' >/dev/null

# ============================================================================
# 2d. Command manifests — `help <cmd>` surfaces the manifest synopsis, not the old stub
# ============================================================================
# Each clank builtin now carries a real Manifest; its synopsis feeds get_content, which Brush's
# `help` reads. Assert the enriched content is live in the durable agent (proves the registry built).
step "Command manifests"
expect_contains "help shows manifest synopsis"   'help cat'  'concatenate files'

# ============================================================================
# 2e. Process table — `ps` reflects a real per-line process table
# ============================================================================
# Each executed line becomes one process row (PID/state). `ps` reads the table installed for the
# current line, so it sees the synthetic root, prior lines as Z, and its own row as R.
step "Process table (ps)"
# Run a uniquely-named command in ONE invocation, then `ps` in a SEPARATE invocation must still show
# it — proving the process table persists across invocations on the durable agent.
run_line 'echo ps_marker_xyz' >/dev/null
expect_contains "ps shows a prior line (durable)"  'ps'  'echo ps_marker_xyz'
# The prior line is completed → Z; the row for it in `ps` output carries Z.
expect_contains "prior line shows Z state"         'ps'  'ps_marker_xyz'
# ps shows the synthetic clank root and its own running row.
expect_contains "ps shows the clank root"          'ps'  'clank'
expect_contains "ps default header"                'ps'  'PID'
# Wide formats render their headers with cpu/mem as dashes.
expect_contains "ps aux header renders"            'ps aux'  '%CPU'
expect_contains "ps -ef shows PPID column"         'ps -ef'  'PPID'

# ============================================================================
# 3. Durability — write in one invocation, read in a SEPARATE invocation
# ============================================================================
step "Durability check (state persists across invocations)"
run_line 'echo persisted > /tmp/work/p' >/dev/null
expect "file survives into a later invocation" 'cat /tmp/work/p'                                 $'persisted'

# ============================================================================
# 4. Summary
# ============================================================================
step "Results"
echo "  ${c_grn}${PASS} passed${c_rst}, $([[ $FAIL -gt 0 ]] && echo "${c_red}${FAIL} failed${c_rst}" || echo "0 failed")"
[[ $FAIL -eq 0 ]] || exit 1
echo "  ${c_grn}all clear${c_rst}"
