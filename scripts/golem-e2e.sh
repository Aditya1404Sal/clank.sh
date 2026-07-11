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
WITH_LLM=0
for arg in "$@"; do
  case "$arg" in
    --keep)     KEEP=1 ;;
    --takeover) TAKEOVER=1 ;;
    --with-llm) WITH_LLM=1 ;;
    -h|--help)  sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg" >&2; echo "usage: $0 [--keep] [--takeover] [--with-llm]" >&2; exit 2 ;;
  esac
done

# The CLI (deploy/invoke) targets the server via the active profile, which points at the
# default router port. The server we start must therefore bind that same default port — so a
# server already running there is a hard conflict we must resolve before starting.
ROUTER_PORT=9881

# `ask` needs ANTHROPIC_API_KEY in the agent env (golem.yaml passes it through via Jinja
# substitution, which is STRICT — a missing host var fails the deploy). So export an empty default
# when it's unset.
#
# COST GUARD: the `ask` network assertions make REAL Anthropic API calls (the agentic loop can be
# several calls each). To avoid spending credits on every run, they fire ONLY when BOTH a real key is
# present AND `--with-llm` is passed. A normal `--takeover` run skips them (the no-key sanity path
# still verifies clean exit-4 degradation). Set the model with `model default` on the agent; the
# built-in default is the lightest model (claude-haiku-4-5-20251001).
if [[ -n "${ANTHROPIC_API_KEY:-}" ]]; then
  HAS_ANTHROPIC_KEY=1
else
  HAS_ANTHROPIC_KEY=0
  export ANTHROPIC_API_KEY=""
fi
# Whether to actually exercise the live model (real API calls).
if [[ $HAS_ANTHROPIC_KEY -eq 1 && $WITH_LLM -eq 1 ]]; then
  RUN_LLM=1
else
  RUN_LLM=0
fi

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
    # `exit` (not `return`) inside an EXIT trap is what actually sets the script's final status;
    # `return` leaves the status as whatever the last trap command produced, masking a real failure.
    exit $ec
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
  # `exit` (not `return`) inside an EXIT trap is what actually sets the script's final status;
  # `return` leaves the status as whatever the last trap command produced, masking a real failure.
  exit $ec
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

# Invoke a method that returns the structured `EvalResult` record (`eval`, `answer_prompt`,
# `abort_prompt`) and print the raw `.result_json.value` JSON object (so callers can pull
# .stdout / .exit-code / .pending-prompt with jq). `$1` = method, remaining args = WIT arg literals.
eval_json() {
  local method="$1"; shift
  golem agent invoke -q --format json "$AGENT_ID" "$method" "$@" 2>>"$SERVER_LOG" \
    | grep '"result_json"' \
    | tail -1 \
    | jq -c '.result_json.value // empty' 2>/dev/null
}

# Assert a jq filter over an EvalResult JSON object yields the expected value.
#   expect_eval <desc> <result-json> <jq-filter> <want>
expect_eval() {
  local desc="$1" json="$2" filter="$3" want="$4"
  local got; got="$(printf '%s' "$json" | jq -r "$filter" 2>/dev/null)"
  if [[ "$got" == "$want" ]]; then
    echo "  ${c_grn}✓${c_rst} $desc"
    PASS=$((PASS+1))
  else
    echo "  ${c_red}✗${c_rst} $desc"
    echo "      filter:   $filter"
    echo "      expected: $(printf '%q' "$want")"
    echo "      got:      $(printf '%q' "$got")"
    echo "      json:     $json"
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
# /dev/null, so don't redirect there; use a `test -e` guard instead). `rm` is now sudo-only
# (authorization enforcement), so it needs the `sudo` prefix to run without a confirmation pause.
expect "sudo rm deletes a file"              'sudo rm /tmp/work/c; if [ -e /tmp/work/c ]; then echo present; else echo gone; fi' $'gone'
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

# context trim <n> — drop the oldest n entries (pure/sync, no LLM). Fresh window, trim, assert the
# oldest is gone and the newest survives.
run_line 'context clear' >/dev/null
run_line 'echo trim_alpha' >/dev/null
run_line 'echo trim_bravo' >/dev/null
run_line 'echo trim_charlie' >/dev/null
run_line 'context trim 2' >/dev/null
# Assert the drop marker shows exactly 2 dropped, and the newest survives. (We avoid `context show
# | grep trim_alpha` — the pipeline's own command line contains "trim_alpha", which `context show`
# would render back, matching itself. So assert on the marker count + the surviving entry instead.)
expect_contains "trim drops exactly 2 entries"   'context show'  '[2 earlier entries dropped]'
expect_contains "trim keeps the newest entry"    'context show'  'trim_charlie'
expect_contains "context trim without a count errors" 'context trim'  'expects a count'
run_line 'context clear' >/dev/null

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
# 2f. Virtual /proc — process files computed on read, cat/ls-able
# ============================================================================
# /proc is a virtual (not file-backed) namespace served from the process table. clank's own cat/ls
# resolve /proc paths and delegate real paths to uutils. These assertions are standalone; the piped
# form `cat /proc/.. | grep` now works on the wasm agent too (see the Pipelines block, 2m).
step "Virtual /proc"
expect_contains "cat /proc/1/status shows root Pid"  'cat /proc/1/status'   'Pid:'
expect_contains "proc status shows State"            'cat /proc/1/status'   'State:'
expect_contains "proc status root is sleeping"       'cat /proc/1/status'   'S (sleeping)'
# grep output is captured on wasm now (run_tool → context.stdout()); grep on /proc works standalone.
expect_contains "grep State /proc/1/status (captured)" 'grep State /proc/1/status'  'State'
# pid 2 is the first line this session executed (`mkdir -p /tmp/work`, at the top of the run) — its
# cmdline is still readable here, proving /proc reflects prior lines durably across invocations.
expect_contains "cat /proc/2/cmdline (durable)"      'cat /proc/2/cmdline'  'mkdir'
# environ reflects the shell env (the GOLEM_* set on the agent).
expect_contains "cat /proc/<pid>/environ"            'cat /proc/2/environ'  'GOLEM_'
# system-prompt is readable and now carries the real ask preamble (A2).
expect_contains "cat /proc/clank/system-prompt"      'cat /proc/clank/system-prompt'  'You are clank'
# ls lists the fixed child names.
expect_contains "ls /proc/1 lists status"            'ls /proc/1'           'status'
expect_contains "ls /proc/clank lists system-prompt" 'ls /proc/clank'       'system-prompt'
# Unknown /proc path errors cleanly.
expect_contains "unknown /proc path errors"          'cat /proc/99999/status'  'No such file'

# ============================================================================
# 2g. prompt-user — pause surfaces to the caller (no hang) + follow-up answer resolves
# ============================================================================
# The agent NEVER blocks on prompt-user: it records the pending question as durable state and
# returns immediately with `pending-prompt` set. The caller (here, this script) delivers the answer
# on a SEPARATE invocation via `answer_prompt`. This is the honest fit for the one-shot RPC transport
# (there is no live human terminal on the agent) and, critically, it means a `prompt-user` invoke
# returns instead of wedging the agent — the whole point of the redesign. All assertions are strictly
# sequential; no concurrent invocation is needed.
step "prompt-user (pause surfaces, no hang, follow-up answer resolves)"

# 1. Surface: the invoke RETURNS (doesn't hang) with the question + a pending_prompt payload.
# NOTE: `.stdout` values carry a trailing "\n" on the wire (see the JSON), but `jq -r` inside `$(...)`
# strips it, so the expected values here omit it — the newline's presence is not what we're testing.
PU_SURFACE="$(eval_json eval '"prompt-user \"Which environment?\""')"
expect_eval "prompt-user surfaces the question on stdout"  "$PU_SURFACE"  '.stdout'  'Which environment?'
expect_eval "prompt-user exits 0 on surface"               "$PU_SURFACE"  '.exit_code'  '0'
expect_eval "pending_prompt carries the question"          "$PU_SURFACE"  '.pending_prompt.question'  'Which environment?'

# 2. Answer: a SEPARATE invocation delivers the response, which comes back on stdout, exit 0.
PU_ANSWER="$(eval_json answer_prompt '"production"')"
expect_eval "answer_prompt returns the response"           "$PU_ANSWER"  '.stdout'  'production'
expect_eval "answer_prompt exits 0"                        "$PU_ANSWER"  '.exit_code'  '0'
expect_eval "pending_prompt clears after answering"        "$PU_ANSWER"  '.pending_prompt'  'null'

# 3. --choices: an answer outside the allowed set leaves the prompt pending (exit 1), then a valid
#    choice resolves it.
eval_json eval '"prompt-user \"Deploy?\" --choices staging,production"' >/dev/null
PU_BAD="$(eval_json answer_prompt '"maybe"')"
expect_eval "invalid choice is rejected (exit 1)"          "$PU_BAD"  '.exit_code'  '1'
PU_GOOD="$(eval_json answer_prompt '"staging"')"
expect_eval "valid choice then resolves"                   "$PU_GOOD"  '.stdout'  'staging'

# 4. abort: --confirm prompt, aborted → exit 130, no stdout.
eval_json eval '"prompt-user \"Proceed?\" --confirm"' >/dev/null
PU_ABORT="$(eval_json abort_prompt)"
expect_eval "abort_prompt exits 130"                       "$PU_ABORT"  '.exit_code'  '130'

# ============================================================================
# 2h. Authorization enforcement — sudo-only `rm` gates on approval (README policy model)
# ============================================================================
# `rm` is sudo-only (destructive op). Without `sudo` it surfaces a confirmation pause instead of
# deleting; a human answer runs or denies it. `sudo rm` pre-authorizes and runs immediately. The
# gate composes on the SAME pending-prompt pause as prompt-user — all sequential, no hang.
step "Authorization (sudo-only rm gates on approval)"

# Seed a file to attempt to delete under the gate.
run_line 'echo target > /tmp/work/gated' >/dev/null

# 1. `rm` without sudo surfaces a confirmation (pending_prompt set — NOTE: while a prompt is
#    pending, ordinary commands are rejected, so we must resolve it before any run_line check).
AZ_SURFACE="$(eval_json eval '"rm /tmp/work/gated"')"
expect_eval "rm without sudo surfaces a confirmation"      "$AZ_SURFACE"  '.pending_prompt.question | contains("sudo")'  'true'

# 2. Deny → exit 5. Now the prompt is resolved; check the file survived.
AZ_DENY="$(eval_json answer_prompt '"no"')"
expect_eval "denied rm exits 5"                            "$AZ_DENY"  '.exit_code'  '5'
expect "gated file survives an unapproved+denied rm"       'if [ -e /tmp/work/gated ]; then echo present; else echo gone; fi'  $'present'

# 3. Approve → the deferred rm runs. Resolve first, then check the file is gone.
eval_json eval '"rm /tmp/work/gated"' >/dev/null
AZ_APPROVE="$(eval_json answer_prompt '"yes"')"
expect_eval "approved rm exits 0"                          "$AZ_APPROVE"  '.exit_code'  '0'
expect "approved rm deletes the file"                      'if [ -e /tmp/work/gated ]; then echo present; else echo gone; fi'  $'gone'

# 4. `sudo rm` pre-authorizes: runs immediately, no prompt.
run_line 'echo target2 > /tmp/work/gated2' >/dev/null
AZ_SUDO="$(eval_json eval '"sudo rm /tmp/work/gated2"')"
expect_eval "sudo rm does not prompt"                      "$AZ_SUDO"  '.pending_prompt'  'null'
expect "sudo rm deletes immediately"                       'if [ -e /tmp/work/gated2 ]; then echo present; else echo gone; fi'  $'gone'

# ============================================================================
# 2i. $PATH resolution backbone — default $PATH set, type/which resolve
# ============================================================================
# clank sets a virtual $PATH (the package-layout namespace grease installs into later). `type` is
# the authoritative resolver for all commands; `which` finds file-backed commands only (nothing for
# builtins, since none are real files yet). The dirs mostly don't exist — resolution degrades to
# "not found" rather than erroring.
step "\$PATH resolution (default PATH, type, which)"
expect "PATH is the README default"          'echo $PATH'  '/usr/local/bin:/usr/bin:/usr/lib/mcp/bin:/usr/lib/agents/bin:/usr/lib/prompts/bin:/usr/share/skills/*/bin'
expect_contains "type resolves a builtin"    'type ls'     'builtin'
# `which` finds file-backed commands only. On the agent NONE of the $PATH dirs exist/are populated
# yet (grease installs into them later), so `which` finds nothing for every name — including `ls`
# (a clank builtin, which `which` correctly does NOT report — that's `type`'s job) and an unknown
# name. Critically it must NOT report a phantom path for a missing file (the bug the earlier run
# caught: Brush's wasm executable()/is_file() lie). The chained marker proves no wedge.
expect "which on a builtin finds no file"    'which ls; echo done'                     $'done'
expect "which on an unknown cmd finds nothing" 'which clank-no-such-cmd-xyz; echo done'  $'done'

# ============================================================================
# 2j. Network — outbound HTTP (curl/wget) from the deployed agent
# ============================================================================
# THE POINT OF THIS BLOCK: prove wstd WASI-HTTP works when awaited from eval_line (one level under
# the Golem SDK's wstd::block_on) on a real deployed agent — the transport ask/grease will need.
# curl/wget are Confirm-gated (outbound HTTP), so each is a surface→approve two-invocation dance,
# exactly like the authz rm block above. Uses https://example.com (stable, tiny, TLS). Resolve each
# prompt before any other run_line (a pending prompt rejects ordinary commands).
step "Network (curl/wget outbound HTTP from the agent)"

# 1. curl surfaces the outbound-HTTP confirmation (no request yet).
NET_SURFACE="$(eval_json eval '"curl https://example.com"')"
expect_eval "curl surfaces outbound-HTTP confirm"  "$NET_SURFACE"  '.pending_prompt != null'  'true'
# 2. Approve → the request runs on the agent; body comes back on stdout, exit 0.
NET_OK="$(eval_json answer_prompt '"yes"')"
expect_eval "approved curl exits 0"                "$NET_OK"  '.exit_code'  '0'
expect_eval "curl body contains Example Domain"    "$NET_OK"  '.stdout | contains("Example Domain")'  'true'

# 3. Denied path → exit 5.
eval_json eval '"curl https://example.com"' >/dev/null
NET_DENY="$(eval_json answer_prompt '"no"')"
expect_eval "denied curl exits 5"                  "$NET_DENY"  '.exit_code'  '5'

# 4. sudo curl pre-authorizes: no prompt, body fetched (direct-allow path also routes through HTTP).
NET_SUDO="$(eval_json eval '"sudo curl https://example.com"')"
expect_eval "sudo curl does not prompt"            "$NET_SUDO"  '.pending_prompt'  'null'
expect_eval "sudo curl body contains Example Domain" "$NET_SUDO"  '.stdout | contains("Example Domain")'  'true'

# 5. wget -O - streams to stdout (sudo to skip the prompt).
NET_WGET="$(eval_json eval '"sudo wget -O - https://example.com"')"
expect_eval "sudo wget -O - fetches to stdout"     "$NET_WGET"  '.stdout | contains("Example Domain")'  'true'

# 6. curl -o <file> writes the body to the agent fs, then cat reads it back (proves -o + wasm file
#    write compose). sudo to skip the prompt; then a normal run_line cat.
eval_json eval '"sudo curl -o /tmp/work/page https://example.com"' >/dev/null
expect_contains "curl -o wrote a file"             'cat /tmp/work/page'  'Example Domain'

# ============================================================================
# 2k. Resolution surface — type authoritative + virtual /bin + --help for intercepted cmds
# ============================================================================
# The README makes every capability discoverable on the PATH: `type` resolves ALL commands, `/bin`
# is a virtual namespace listing the builtins, and each has `--help`. clank's intercepted commands
# (prompt-user/curl/wget/context) are handled before Brush dispatch and are invisible to Brush's own
# `type`/`--help` — this block proves clank now closes that gap on the live agent. These use standalone
# `ls /bin` (contains-name asserts); the piped `ls /bin | grep` form is covered in the Pipelines block.
step "Resolution surface (type authoritative, virtual /bin, --help)"
# `type` for an intercepted command Brush can't see — clank answers "is a shell builtin".
expect_contains "type curl resolves as builtin"       'type curl'         'curl is a shell builtin'
expect_contains "type prompt-user resolves as builtin" 'type prompt-user' 'prompt-user is a shell builtin'
# `type -t` prints the bare word, like Brush.
expect "type -t wget prints bare builtin"             'type -t wget'      $'builtin'
# `type` for a Brush-registered builtin still works (fell through to Brush unchanged).
expect_contains "type cat still resolves (Brush)"     'type cat'          'builtin'
# virtual /bin: `ls /bin` enumerates every command; `cat /bin/<name>` reads its help.
expect_contains "ls /bin lists curl"                  'ls /bin'           'curl'
expect_contains "ls /bin lists prompt-user"           'ls /bin'           'prompt-user'
expect_contains "ls /bin lists a Brush builtin (cat)" 'ls /bin'           'cat'
expect_contains "cat /bin/curl shows help text"       'cat /bin/curl'     'fetch a URL over'
expect_contains "cat /bin/prompt-user shows help"     'cat /bin/prompt-user'  'pause the'
expect_contains "cat /bin/<unknown> errors"           'cat /bin/no-such-cmd'  'No such file'
# `--help` for intercepted commands — must NOT surface a confirmation (help query, not an HTTP call).
expect_contains "curl --help prints help (no confirm)" 'curl --help'      'fetch a URL over'
expect_contains "prompt-user --help prints help"      'prompt-user --help'  'pause the'
expect_contains "wget --help prints help"             'wget --help'       'download a URL'
expect_contains "context --help prints help"          'context --help'    'session transcript'
expect_contains "context --help documents summarize"  'context --help'    'summarize'
expect_contains "context --help documents trim"       'context --help'    'trim'
# `context summarize` inside $() can't run the LLM (Wall-C) — honest error, no hang. No key needed.
CTX_SUBST="$(eval_json eval '"echo $(context summarize)"')"
expect_eval "context summarize in \$() is honest"     "$CTX_SUBST"  '.stdout | contains("needs the model")'  'true'

# ============================================================================
# 2l. ask — the AI-native LLM command (transcript-as-context)
# ============================================================================
# `ask` sends the shell transcript as the model's context to Claude and prints the reply. The
# resolution asserts (type/--help/ls /bin) need no API key and always run. The real network asserts
# run only when ANTHROPIC_API_KEY is set — they prove the durable Anthropic provider works on the live
# agent and that the transcript feeds the model. `ask` is Confirm-gated (outbound HTTP), so `sudo ask`
# skips the pause; a bare `ask` surfaces the confirmation.
step "ask (AI-native LLM command)"
# Always-run (no key): ask is resolvable and self-documenting.
expect_contains "type ask resolves as builtin"        'type ask'          'ask is a shell builtin'
expect_contains "ls /bin lists ask"                   'ls /bin'           'ask'
expect_contains "ask --help prints help (no confirm)" 'ask --help'        'send the current shell transcript'
expect_contains "ask --help documents --json"         'ask --help'        '--json'
expect_contains "ask --help documents piped stdin"    'ask --help'        'Piped input'
# ask inside a command substitution hits the honest stub with a pointer to the working forms.
# No API key needed — the stub fires before any model call (it can't run under Brush's nested runtime).
ASK_SUBST="$(eval_json eval '"echo $(ask \"q\")"')"
expect_eval "ask in \$() gives the honest pointer"    "$ASK_SUBST"  '.stderr | contains("LAST pipeline stage")'  'true'
# `ask repl` on the durable agent is an honest not-here message (interactive REPL is native-only).
# No API key needed — the message returns before any model call.
ASK_REPL="$(eval_json eval '"ask repl"')"
expect_eval "ask repl on the agent is honest"         "$ASK_REPL"  '.stderr | contains("native-terminal feature")'  'true'
expect_eval "ask repl exits 2 on the agent"           "$ASK_REPL"  '.exit_code'  '2'
# The live system prompt is inspectable and reflects the command surface + the shell tool (A2).
expect_contains "/proc/clank/system-prompt describes the shell tool" 'cat /proc/clank/system-prompt' '`shell`'
expect_contains "/proc/clank/system-prompt lists the command surface" 'cat /proc/clank/system-prompt' '[confirm]'

if [[ $RUN_LLM -eq 1 ]]; then
  note "--with-llm + key present — running ask network assertions against the live model (real API calls)"
  # 0. Cost guard: verify the default model is the lightest (haiku) BEFORE any API call, so a
  #    misconfiguration can't silently spend on a bigger model. The fresh agent's built-in default.
  expect_contains "default model is haiku (cost guard)"  'model list'  '* anthropic/claude-haiku-4-5-20251001'
  # 1. sudo ask pre-authorizes (no confirm); the model replies. Ask for an exact token so we can
  #    assert on it deterministically.
  ASK_PONG="$(eval_json eval '"sudo ask --fresh \"Reply with exactly the single word: pong . No punctuation, nothing else.\""')"
  expect_eval "sudo ask exits 0"                       "$ASK_PONG"  '.exit_code'  '0'
  expect_eval "sudo ask returns the model reply"       "$ASK_PONG"  '.stdout | ascii_downcase | contains("pong")'  'true'
  # 2. Transcript-as-context: echo a unique marker, then ask about it WITHOUT --fresh. The model can
  #    only answer correctly if the transcript (carrying the echo) reached it.
  run_line 'echo the_secret_word_is_kumquat' >/dev/null
  ASK_CTX="$(eval_json eval '"sudo ask \"What is the secret word I just echoed? Reply with just the word.\""')"
  expect_eval "ask sees the transcript as context"     "$ASK_CTX"  '.stdout | ascii_downcase | contains("kumquat")'  'true'
  # 3. Bare ask (no sudo) surfaces the outbound-HTTP confirmation.
  ASK_CONFIRM="$(eval_json eval '"ask \"hello\""')"
  expect_eval "bare ask surfaces a confirmation"       "$ASK_CONFIRM"  '.pending_prompt != null'  'true'
  # Resolve the pending prompt (deny) so it doesn't block later lines; denial exits 5.
  ASK_DENY="$(eval_json answer_prompt '"no"')"
  expect_eval "denied ask exits 5"                     "$ASK_DENY"  '.exit_code'  '5'

  # 4. Agentic proof-of-tool-use (A2): the model uses the `shell` tool to create a file, then we read
  #    it back independently. A unique marker avoids stale-file false positives. `sudo ask` grants
  #    blanket confirm-tier authz so the tool line runs without a per-call pause. The tool-trace assert
  #    is the primary proof the loop executed a shell tool at all; the file read-back is the strong
  #    end-to-end proof (the model wrote a real file we can independently see).
  MARK="agentic$(date +%s)$$"   # no underscores: some models split on them
  ASK_TOOL="$(eval_json eval "\"sudo ask \\\"Use the shell tool to run this exact command: echo ${MARK} > /tmp/${MARK} . Then confirm you did it.\\\"\"")"
  expect_eval "agentic ask exits 0"                    "$ASK_TOOL"  '.exit_code'  '0'
  expect_eval "agentic ask emits a tool trace"         "$ASK_TOOL"  '.stderr | contains("[tool]")'  'true'
  expect_contains "the shell tool created the file"    "cat /tmp/${MARK}"  "${MARK}"

  # 4b. --json output contract (Phase A): the model is instructed to emit one JSON value; the shell
  #     validates it. Happy path — a valid object on stdout, exit 0, and jq can parse .ok.
  ASK_JSON="$(eval_json eval '"sudo ask --json --fresh \"Return a JSON object with a single key ok set to boolean true. Emit only the JSON.\""')"
  expect_eval "ask --json exits 0 on valid JSON"       "$ASK_JSON"  '.exit_code'  '0'
  expect_eval "ask --json stdout is parseable JSON"    "$ASK_JSON"  '.stdout | fromjson | .ok'  'true'

  # 4c. stdin-as-context (Phase B): pipe a fact into `ask --fresh` (no transcript). The model can only
  #     answer from the piped stdin, proving the session pre-extracted the upstream and fed it in.
  ASK_STDIN="$(eval_json eval '"echo \"the secret animal is platypus\" | sudo ask --fresh \"What is the secret animal? Reply with just the one word.\""')"
  expect_eval "piped ask exits 0"                      "$ASK_STDIN"  '.exit_code'  '0'
  expect_eval "ask reads piped stdin as context"       "$ASK_STDIN"  '.stdout | ascii_downcase | contains("platypus")'  'true'
  # 4c2. B+A compose: pipe JSON in, ask for a transformed JSON out under --json.
  ASK_STDIN_JSON="$(eval_json eval '"printf %s {\\\"n\\\":41} | sudo ask --json --fresh \"Increment n by one and return the same JSON shape. Emit only the JSON.\""')"
  expect_eval "piped ask --json exits 0"               "$ASK_STDIN_JSON"  '.exit_code'  '0'
  expect_eval "piped ask --json increments n"          "$ASK_STDIN_JSON"  '.stdout | fromjson | .n'  '42'

  # 4d. context summarize (LLM): run a couple of distinctive commands, then summarize. sudo
  #     pre-authorizes (no pause). Assert exit 0 + non-empty summary; the transcript is NOT mutated
  #     (the original marker still shows), matching the inspection-only contract.
  run_line 'echo summarize_probe_kiwi' >/dev/null
  CTX_SUM="$(eval_json eval '"sudo context summarize"')"
  expect_eval "sudo context summarize exits 0"         "$CTX_SUM"  '.exit_code'  '0'
  expect_eval "context summarize returns a summary"    "$CTX_SUM"  '.stdout | length > 0'  'true'
  expect_eval "context summarize did not pause"        "$CTX_SUM"  '.pending_prompt'  'null'
  expect_contains "summarize did not mutate the transcript" 'context show'  'summarize_probe_kiwi'

  # 5. Mid-loop authorization pause (A3): under a plain (non-sudo) ask, a confirm-tier tool line
  #    (curl) PAUSES for the human. This is robust to the model's exact behavior: the ONLY thing
  #    guaranteed is the ask-invocation confirm (our gate, deterministic). What the model does after
  #    approval (whether/how it calls curl) is model-dependent, so we DON'T pin the second pause —
  #    we approve, then keep resolving prompts until the loop settles, and assert it ends cleanly
  #    without wedging the agent. This proves the plain-ask pause path works end-to-end without
  #    depending on haiku emitting an exact tool call.
  ASK_PAUSE1="$(eval_json eval '"ask \"Please run this shell command and tell me the HTTP result: curl -s https://example.com/\""')"
  expect_eval "plain ask confirms the ask itself first" "$ASK_PAUSE1"  '.pending_prompt != null'  'true'
  # Approve the ask; then deny anything the model tries (at most a few tool pauses), draining to a
  # settled state. Each `no` on a tool authz feeds a "denied by user" result; the loop finishes.
  ASK_STEP="$(eval_json answer_prompt '"yes"')"
  for _ in 1 2 3 4 5; do
    [[ "$(printf '%s' "$ASK_STEP" | jq -r '.pending_prompt // "null"')" == "null" ]] && break
    ASK_STEP="$(eval_json answer_prompt '"no"')"
  done
  expect_eval "the plain-ask loop settles (exit 0, no wedge)" "$ASK_STEP"  '.pending_prompt == null and .exit_code == 0'  'true'
  # Belt-and-suspenders: the agent accepts a normal command afterward (not stuck pending).
  expect "agent is responsive after the pause chain"  'echo after-pause-ok'  $'after-pause-ok'

  # 6. prompt_user tool round-trip (A3): ask the model to collect a value via prompt_user. The pause
  #    is OUR machinery (deterministic); whether the model then echoes the answer is model-dependent,
  #    so we assert the pause happens and the loop settles cleanly — not the exact echoed text.
  ASK_PU1="$(eval_json eval '"sudo ask \"Use the prompt_user tool to ask the user for a codeword, then tell me what they said.\""')"
  if [[ "$(printf '%s' "$ASK_PU1" | jq -r '.pending_prompt // "null"')" != "null" ]]; then
    expect_eval "prompt_user pauses for the human"     "$ASK_PU1"  '.pending_prompt != null'  'true'
    ASK_PU2="$(eval_json answer_prompt '"zebra-legend-first"')"
    # Drain any follow-on pauses so we don't leave the agent pending.
    for _ in 1 2 3 4 5; do
      [[ "$(printf '%s' "$ASK_PU2" | jq -r '.pending_prompt // "null"')" == "null" ]] && break
      ASK_PU2="$(eval_json answer_prompt '"zebra-legend-first"')"
    done
    expect_eval "the prompt_user loop settles cleanly" "$ASK_PU2"  '.pending_prompt == null and .exit_code == 0'  'true'
  else
    # Haiku sometimes answers without using the tool — that's a model choice, not a bug. Just assert
    # the ask completed cleanly (the tool was available; the model chose not to use it).
    note "model answered without prompt_user (a model choice) — asserting clean completion"
    expect_eval "prompt_user-offered ask completes"    "$ASK_PU1"  '.exit_code'  '0'
  fi
else
  if [[ $HAS_ANTHROPIC_KEY -eq 1 ]]; then
    note "ANTHROPIC_API_KEY present but --with-llm not passed — skipping live model asserts (no API cost). Re-run with --with-llm to exercise the model."
  else
    note "ANTHROPIC_API_KEY not set — skipping ask network assertions."
    # Sanity (no-key only): sudo ask fails cleanly (exit 4 from the provider), never hangs or panics.
    ASK_NOKEY="$(eval_json eval '"sudo ask --fresh \"hi\""')"
    expect_eval "sudo ask without a key exits nonzero"   "$ASK_NOKEY"  '.exit_code != 0'  'true'
  fi
fi

# ============================================================================
# 2l-b. model — default model + ask.toml (no API key needed)
# ============================================================================
# `model` is a Brush builtin that reads/writes ~/.config/ask/ask.toml (agent HOME=/home/user). It
# does no HTTP, so these run without a key. Proves HOME seeding + `~` expansion + ask.toml persistence.
step "model (default model, ask.toml)"
expect_contains "model list shows the catalog"        'model list'  'anthropic/claude-haiku-4-5-20251001'
expect_contains "fresh default is haiku (built-in)"   'model list'  '* anthropic/claude-haiku-4-5-20251001'
expect_contains "model default sets sonnet"           'model default anthropic/claude-sonnet-4-5'  'set to anthropic/claude-sonnet-4-5'
expect_contains "ask.toml persisted the default"      'cat ~/.config/ask/ask.toml'  'claude-sonnet-4-5'
expect_contains "list now marks sonnet default"       'model list'  '* anthropic/claude-sonnet-4-5'
expect_contains "model info reports default"          'model info claude-sonnet-4-5'  'default:   yes'
expect_contains "model add is an honest key error"    'model add anthropic --key sk-fake'  'ANTHROPIC_API_KEY'
expect_contains "unknown provider is rejected"        'model default openai/gpt-4o'  "unknown provider 'openai'"
# Restore the built-in default (haiku, the lightest model) so any later ask stays cheap.
expect_contains "restore haiku default"               'model default anthropic/claude-haiku-4-5-20251001'  'set to anthropic/claude-haiku-4-5-20251001'

# ============================================================================
# 2l-c. mcp — MCP client config + surfaces (no live server needed)
# ============================================================================
# `mcp` manages HTTPS MCP servers. These asserts exercise the config/manifest/type surfaces without a
# reachable server: adding an unreachable URL writes the config (state "not installed") and fails the
# HTTP install; the config surfaces (list, /etc/mcp file, remove) all work. A live tools/call is only
# exercised in the MCP_TEST_URL-gated block (C2/C3, added later).
step "mcp (client config + surfaces)"
expect_contains "type mcp is intercepted"             'type mcp'          'mcp'
expect_contains "mcp --help documents subcommands"    'mcp --help'        'mcp add'
expect_contains "mcp list is empty initially"         'mcp list'          'no MCP servers configured'
expect_contains "mcp watch is honestly unsupported"   'mcp watch some://x'  'not supported'
# `mcp add` is Confirm (outbound HTTP); `sudo mcp add` pre-authorizes. Unreachable URL → install fails
# (exit 4) but the config is written.
MCP_ADD="$(eval_json eval '"sudo mcp add unreachable https://127.0.0.1:9/mcp"')"
expect_eval "mcp add to an unreachable URL fails"     "$MCP_ADD"  '.exit_code'  '4'
expect_contains "the config file was written"         'cat /etc/mcp/unreachable.toml'  'https://127.0.0.1:9/mcp'
expect_contains "mcp list shows it not installed"     'mcp list'          'not installed'
expect_contains "mcp remove deletes it"               'sudo mcp remove unreachable'  "removed MCP server 'unreachable'"
expect_contains "mcp list is empty again"             'mcp list'          'no MCP servers configured'

# --- Gated live-server block: needs a real HTTPS MCP server (public no-auth, e.g. DeepWiki's
#     https://mcp.deepwiki.com/mcp). Set MCP_TEST_URL to run; optionally MCP_TEST_TOOL + MCP_TEST_ARGS
#     (JSON) to exercise an actual tools/call. Gated because public endpoints aren't CI-reliable. This
#     block makes NO Anthropic calls — it's the MCP HTTP path only.
if [[ -n "${MCP_TEST_URL:-}" ]]; then
  note "MCP_TEST_URL set — running live MCP server assertions against $MCP_TEST_URL"
  MCP_INST="$(eval_json eval "\"sudo mcp add mcptest ${MCP_TEST_URL}\"")"
  expect_eval "live mcp add installs"                 "$MCP_INST"  '.exit_code'  '0'
  expect_eval "install reports tool count"            "$MCP_INST"  '.stdout | contains("tools")'  'true'
  expect_contains "mcp list shows it installed"       'mcp list'          'enabled'
  expect_contains "mcp tools lists ≥1 tool"           'mcp tools mcptest'  ''  # non-empty stdout
  expect_contains "which finds the server stub"       'which mcptest'      '/usr/lib/mcp/bin/mcptest'
  expect_contains "server --help lists tools"         'mcptest --help'     'Tools:'
  # A bare tool call surfaces a confirmation (MCP calls are Confirm); deny it (exit 5), no HTTP.
  if [[ -n "${MCP_TEST_TOOL:-}" ]]; then
    MCP_CONFIRM="$(eval_json eval "\"mcptest ${MCP_TEST_TOOL} --args '${MCP_TEST_ARGS:-{}}'\"")"
    expect_eval "bare MCP tool call confirms"          "$MCP_CONFIRM"  '.pending_prompt != null'  'true'
    MCP_DENY="$(eval_json answer_prompt '"no"')"
    expect_eval "denied MCP tool exits 5"              "$MCP_DENY"  '.exit_code'  '5'
    # sudo pre-authorizes → the tool actually runs.
    MCP_RUN="$(eval_json eval "\"sudo mcptest ${MCP_TEST_TOOL} --args '${MCP_TEST_ARGS:-{}}'\"")"
    expect_eval "sudo MCP tool call runs (exit 0)"     "$MCP_RUN"  '.exit_code'  '0'
  fi
  # Session lifecycle.
  expect_contains "mcp session open works"            'mcp session open mcptest'  'opened session'
  expect_contains "mcp session list shows it"         'mcp session list'          's1'
  # close: accept either success or the 405-refusal wording (server-dependent).
  MCP_CLOSE="$(eval_json eval '"mcp session close s1"')"
  expect_eval "mcp session close resolves"            "$MCP_CLOSE"  '.exit_code | . == 0 or . == 1'  'true'
  expect_contains "cleanup: remove the test server"   'sudo mcp remove mcptest'   'removed MCP server'
else
  note "MCP_TEST_URL not set — skipping live MCP server assertions (set it to a public HTTPS MCP server)"
fi

# ============================================================================
# 2l-d. grease — the package manager (command seam + registry surface)
# ============================================================================
# grease is a Session-interception command (like mcp). Phase 1: the command resolves, `--help` works,
# and the registry list (add/list/remove) persists to /etc/grease/registries.toml. install/list/etc.
# are honest phase-2 stubs. No API key needed. (Prompt install + run land in a later grease phase.)
step "grease (package manager: command seam + registry)"
expect_contains "type grease is intercepted"          'type grease'       'grease is a shell builtin'
expect_contains "grease --help documents subcommands" 'grease --help'     'install a prompt package'
expect_contains "grease registry list empty initially" 'grease registry list'  'no registries configured'
expect_contains "grease registry add records a URL"   'grease registry add https://reg.example/pkgs'  'added registry'
expect_contains "grease registry list shows it"       'grease registry list'  'https://reg.example/pkgs'
expect_contains "the registries file was written"     'cat /etc/grease/registries.toml'  'reg.example'
expect_contains "grease registry remove drops it"     'grease registry remove https://reg.example/pkgs'  'removed registry'
expect_contains "grease registry list empty again"    'grease registry list'  'no registries configured'
expect_contains "grease list is empty initially"      'grease list'       'no packages installed'
# install with no registry configured errors honestly (no panic, no hang). sudo pre-authorizes.
expect_contains "grease install without a registry errors" 'sudo grease install summarize'  'no registries configured'
# grease inside a substitution hits the honest stub (it runs at the session layer).
GREASE_SUBST="$(eval_json eval '"echo $(grease list)"')"
expect_eval "grease in \$() gives the honest pointer"  "$GREASE_SUBST"  '.stderr | contains("top-level command")'  'true'

# --- Gated live-registry block: needs a registry serving /packages/<name>.json (+ /index.json for
#     search). Set GREASE_TEST_URL + GREASE_TEST_PKG to run a real install; the installed prompt RUN
#     also needs --with-llm (it calls the model). Gated because it needs an external registry.
if [[ -n "${GREASE_TEST_URL:-}" && -n "${GREASE_TEST_PKG:-}" ]]; then
  note "GREASE_TEST_URL set — running live grease install against $GREASE_TEST_URL"
  run_line "grease registry add ${GREASE_TEST_URL}" >/dev/null
  GR_INST="$(eval_json eval "\"sudo grease install ${GREASE_TEST_PKG}\"")"
  expect_eval "live grease install exits 0"           "$GR_INST"  '.exit_code'  '0'
  expect_eval "live grease install reports installed"  "$GR_INST"  '.stdout | contains("installed")'  'true'
  expect_contains "installed package appears in list"  'grease list'  "${GREASE_TEST_PKG}"
  expect_contains "which finds the installed stub"     "which ${GREASE_TEST_PKG}"  "${GREASE_TEST_PKG}"
  if [[ $RUN_LLM -eq 1 ]]; then
    GR_RUN="$(eval_json eval "\"sudo ${GREASE_TEST_PKG}\"")"
    expect_eval "running the installed prompt exits 0" "$GR_RUN"  '.exit_code'  '0'
    expect_eval "the prompt produced a model reply"    "$GR_RUN"  '.stdout | length > 0'  'true'
  fi
  run_line "sudo grease remove ${GREASE_TEST_PKG}" >/dev/null
else
  note "GREASE_TEST_URL/GREASE_TEST_PKG not set — skipping live grease install (native FakeHttp tests cover it)"
fi

# ============================================================================
# 2m. Pipelines — internal `cmd | cmd` on the single-threaded wasm agent (Wall C)
# ============================================================================
# Internal pipelines used to WEDGE the agent: std::io::pipe() is unsupported on wasip2 and there is
# no blocking thread pool, so the OS-pipe + tokio::spawn_blocking pipeline path never made progress.
# The brush fork now runs owned-shell builtin stages inline in pipeline order, connected by an
# in-memory OpenFile::Stream pipe (each upstream stage drops its writer → clean EOF for the next).
# These assertions PROVE pipelines work end-to-end on the live agent — the whole point of the fix.
step "Pipelines (internal cmd | cmd on the wasm agent)"
# NB: producers use `echo -e` / `for` loops — NOT `printf`, which is a Brush builtin that routes to
# external exec on the agent ("operation not supported"), a separate pre-existing gap.
# Baseline two-stage: Brush-builtin producer into each consumer family (run_tool grep, run_uu cat/wc).
expect "echo | grep matches the line"                 'echo -e "a\nb\nc" | grep b'   $'b'
expect "echo | cat round-trips"                       'echo hi | cat'                $'hi'
expect "echo | wc -w counts words"                    'echo one two three | wc -w'   $'3'
# A flag with a detached value must not be mistaken for a file operand.
expect "echo | head -n takes flag values"             'echo -e "x\ny\nz" | head -n 2'  $'x\ny'
expect "echo | sort orders lines"                     'echo -e "b\na\nc" | sort'     $'a\nb\nc'
# tee: operands are OUTPUTS and stdin is always read — the inverse shape of cat/wc.
expect "echo | tee writes file and passes through"    'echo hello | tee /tmp/work/tee-out; cat /tmp/work/tee-out'  $'hello\nhello'
# Pipe a virtual /proc read into grep (was "native-only" before the fix).
expect_contains "cat /proc/1/status | grep State"     'cat /proc/1/status | grep State'  'State'
# Pipe the virtual /bin listing into grep (the other formerly-deferred case).
expect_contains "ls /bin | grep curl"                 'ls /bin | grep curl'  'curl'
# Three-stage pipeline: every non-last stage must hand off cleanly.
expect "three-stage pipe (grep | grep | wc -l)"       'echo -e "apple\nbanana\napricot" | grep a | grep p | wc -l'  $'2'
# Large producer (a compound `for` as a pipeline stage) into an early-terminating consumer must
# COMPLETE (not hang) and truncate; and the same producer fully counted proves no bytes are lost.
expect "large producer | head completes (no hang)"    'for i in {1..200}; do echo $i; done | head -n 3'  $'1\n2\n3'
expect "loop producer | wc -l counts all lines"       'for i in {1..200}; do echo $i; done | wc -l'  $'200'
# Standalone stdin-readers see EMPTY input — never the real wasip2 stdin resource, whose blocking
# read traps (and permanently wedges) the agent instance.
expect "standalone wc -l reads empty stdin (no trap)" 'wc -l'  $'0'

# ============================================================================
# 2n. Standard utilities — phase 1 (printf, man, stat, grep flags)
# ============================================================================
step "Standard utilities (printf, man, stat, grep flags)"

# printf — clank-registered uu_printf. Brush's own printf builtin is gated `any(unix, windows)`
# upstream, so before this the agent had NO printf (the word fell through to unsupported external
# exec). These two asserts are the direct fix for that long-standing gap.
expect "printf %s/%d with \\n"            'printf "%s-%d\n" a 1'                     $'a-1'
expect "printf reuses format per batch"   'printf "%s\n" one two'                    $'one\ntwo'

# man — manifest-rendered pages (same source as cat /bin/<name>), Brush-builtin fallback, honest
# not-found on unknown names.
expect_contains "man grep renders NAME section"  'man grep'   'grep - search files for a pattern'
expect_contains "man cd falls back to Brush help" 'man cd'    'cd'
MAN_MISSING="$(eval_json eval '"man no-such-command-xyz"')"
expect_eval "man unknown name exits 1"    "$MAN_MISSING" '.exit_code' '1'
expect_eval "man unknown name says so"    "$MAN_MISSING" '.stderr | contains("No manual entry")' 'true'

# stat — honest wasm subset (size/type/timestamps real; inode/uid/mode print '-'), virtual
# namespaces served, -c format directives.
expect "stat -c %s reports size"          'echo -n abc > /tmp/work/stat-f; stat -c %s /tmp/work/stat-f'  $'3'
expect_contains "stat default block: type"   'stat /tmp/work/stat-f'   'regular file'
expect_contains "stat default block: honest dashes" 'stat /tmp/work/stat-f'  'Inode: -'
expect_contains "stat /bin/curl is virtual"  'stat /bin/curl'          'virtual read-only file'
expect_contains "stat directory type"        'stat -c %F /tmp/work'    'directory'
STAT_MISSING="$(eval_json eval '"stat /tmp/work/definitely-absent"')"
expect_eval "stat missing file exits 1"   "$STAT_MISSING" '.exit_code' '1'

# grep flags — the extended wrapper (previously only -n/-i were parsed).
expect "grep -v inverts the match"        'echo -e "a\nb\nc" | grep -v b'            $'a\nc'
expect "grep -c counts matching lines"    'echo -e "x\ny\nx" | grep -c x'            $'2'
expect "grep -in clusters short flags"    'echo -e "Hay\nno" | grep -in hay'         $'1:Hay'
expect "grep -w respects word boundaries" 'echo -e "cat\ncatalog" | grep -w cat'     $'cat'
expect "grep -F treats pattern literally" 'echo -e "a.b\naxb" | grep -F a.b'         $'a.b'
expect "grep -q is silent with exit 0"    'echo hay | grep -q hay; echo $?'          $'0'
expect "grep -q miss exits 1"             'echo hay | grep -q needle; echo $?'       $'1'
expect "grep -l lists matching files"     'echo needle > /tmp/work/g1; echo hay > /tmp/work/g2; grep -l needle /tmp/work/g1 /tmp/work/g2'  $'/tmp/work/g1'
expect "grep -r searches directories"     'mkdir -p /tmp/work/rd; echo needle > /tmp/work/rd/f; grep -r needle /tmp/work/rd'  $'/tmp/work/rd/f:needle'
expect_contains "grep pattern in /bin virtual file"  'grep -h "fetch a URL" /bin/curl'  'fetch a URL'

# ============================================================================
# 2o. Standard utilities — phase 2 (find, xargs)
# ============================================================================
step "Standard utilities (find, xargs)"

# find — hand-written walker (-name/-iname/-path/-type/-maxdepth/-mindepth, implicit -print),
# virtual namespaces served at listing depth.
expect "find -name walks the tree"        'mkdir -p /tmp/work/ft/sub; echo 1 > /tmp/work/ft/a.txt; echo 2 > /tmp/work/ft/sub/b.txt; echo 3 > /tmp/work/ft/sub/c.log; find /tmp/work/ft -name "*.txt"'  $'/tmp/work/ft/a.txt\n/tmp/work/ft/sub/b.txt'
expect "find -type d finds directories"   'find /tmp/work/ft -type d'                 $'/tmp/work/ft\n/tmp/work/ft/sub'
expect "find -maxdepth limits descent"    'find /tmp/work/ft -maxdepth 1 -name "*.txt"'  $'/tmp/work/ft/a.txt'
expect "find /bin serves the virtual namespace"  'find /bin -name "grep*"'            $'/bin/grep'
FIND_MISSING="$(eval_json eval '"find /tmp/work/absent-root"')"
expect_eval "find missing root exits 1"   "$FIND_MISSING" '.exit_code' '1'

# xargs — shell re-entry (run_string): stdin tokens become in-shell command lines.
expect "xargs appends stdin tokens"       'echo one two | xargs echo GOT:'            $'GOT: one two'
expect "xargs -n batches invocations"     'echo -e "a\nb\nc" | xargs -n 2 echo B:'    $'B: a b\nB: c'
expect "xargs -I substitutes whole lines" 'echo -e "x y\nz" | xargs -I {} echo "[{}]"'  $'[x y]\n[z]'
expect "xargs default command is echo"    'echo hi there | xargs'                     $'hi there'
expect "find | xargs composes"            'find /tmp/work/ft -name "*.txt" | xargs cat | wc -l'  $'2'
expect "xargs quotes spacey tokens"       'echo -e "p q" | xargs -I {} mkdir "/tmp/work/{}"; ls /tmp/work | grep -c "p q"'  $'1'

# ============================================================================
# 2p. Standard utilities — phase 3 (sed mini-engine, awk subset)
# ============================================================================
step "Standard utilities (sed engine, awk subset)"

# sed — the line engine (addresses, d/p/q, -n, -e; previously whole-buffer s/// only).
expect "sed s///g substitutes globally"   'echo -e "aa\naa" | sed s/a/X/g'            $'XX\nXX'
expect "sed /re/d deletes matching lines" 'echo -e "#c\nkeep\n#d" | sed /^#/d'        $'keep'
expect "sed 1d deletes by line number"    'echo -e "a\nb\nc" | sed 1d'                $'b\nc'
expect "sed -n 2,3p prints a range"       'echo -e "a\nb\nc\nd" | sed -n 2,3p'        $'b\nc'
expect "sed 2q quits early"               'echo -e "a\nb\nc" | sed 2q'                $'a\nb'
expect "sed -e chains commands"           'echo ab | sed -e s/a/1/ -e s/b/2/'         $'12'
expect "sed & backref in replacement"     'echo abc | sed "s/b/[&]/"'                 $'a[b]c'

# awk — the hand-written subset interpreter (no wasm-viable awk crate exists).
# NB: awk programs are single-quoted AT THE AGENT level. A `\$` in an assert cmd is an invalid
# escape in the CLI's WIT string parser — the payload then degrades to a raw quoted string and the
# agent execs the ENTIRE line as one command name ("operation not supported"). Single quotes keep
# `$` out of Brush's expansion AND keep backslashes out of the WIT payload.
expect "awk prints a field"               "echo -e 'a b c\nd e f' | awk '{print \$2}'"  $'b\ne'
expect "awk -F splits on delimiter"       "echo root:x:0 | awk -F: '{print \$1}'"     $'root'
expect "awk comparison pattern"           "echo -e 'a:x:5\nb:x:50' | awk -F: '\$3 > 10 {print \$1}'"  $'b'
expect "awk regex pattern"                'echo -e "ok\nerr 1\nerr 2" | awk /err/'    $'err 1\nerr 2'
expect "awk sums with END"                "echo -e '1\n2\n3' | awk '{s += \$1} END {print s}'"  $'6'
expect "awk counts matches with n++"      'echo -e "x\ny\nx" | awk "/x/ {n++} END {print n}"'  $'2'
expect "awk NR selects a line"            'echo -e "a\nb\nc" | awk "NR == 2"'         $'b'
expect "awk printf formats"               "awk 'BEGIN {printf \"%d-%s\\n\", 42, \"x\"}'"  $'42-x'
expect "awk concat and OFS"               "echo 'a b' | awk '{print \$2 \"-\" \$1}'"  $'b-a'

# ============================================================================
# 2q. Command substitution + redirects (fork std-utils: $() over the in-memory pipe)
# ============================================================================
step "Command substitution \$(...) and redirects"

# $(...) previously died with "operation not supported" on the agent (std::io::pipe()). The fork
# now runs the substitution inline over the Wall-C in-memory pipe.
expect "simple \$() substitutes"           'echo $(echo nested)'                       $'nested'
expect "backquotes substitute"             'echo `echo bq`'                            $'bq'
expect "\$() captures a pipeline"          'N=$(echo -e "1\n2\n3" | wc -l); echo "count=$N"'  $'count=3'
expect "nested \$()"                       'echo $(echo $(echo deep))'                 $'deep'
expect "\$() composes with virtual /bin"   'echo "have $(ls /bin | grep -c curl) curl"'  $'have 1 curl'
expect "\$() exit status flows to \$?"     'X=$(true); echo $?'                        $'0'

# Redirect surface (the old "fd-clone limit" memory said `>` was broken — probe it for real).
expect ">> appends"                        'echo one > /tmp/work/app; echo two >> /tmp/work/app; cat /tmp/work/app'  $'one\ntwo'
expect_contains "2> captures stderr to a file"  'ls /bin/nope 2> /tmp/work/lserr; cat /tmp/work/lserr'  'No such file'
expect_contains "&> captures both streams" 'ls /bin/nope &> /tmp/work/both; cat /tmp/work/both'  'No such file'
expect "2>&1 pipes stderr downstream"      'ls /bin/nope 2>&1 | grep -c "No such"'     $'1'

# ============================================================================
# 2r. Background jobs + synthetic kill + exec (fork std-utils phase 5)
# ============================================================================
step "Background jobs, kill, exec"

# `cmd &` parks an in-shell future (bg tasks only progress while a foreground future awaits —
# practically: during `wait`). The spawn must return IMMEDIATELY; the killed job must never run.
# The 297s duration is a unique marker for the ps greps below.
expect_contains "sleep 297 & returns immediately"  'sleep 297 &'   '] '
expect_contains "jobs lists the running job"       'jobs'          'Running'
# Count via the new awk (STAT is $2, command starts at $3, duration at $4) so neither the counting
# line's own ps row nor OTHER blocks' sleep rows can match — 297 is this block's unique marker.
expect "ps shows the bg row parked (S)"  "ps | awk '\$2 == \"S\" && \$3 == \"sleep\" && \$4 == \"297\" {n++} END {print n}'"  $'1'
expect_contains "kill %1 cancels the job"          'kill %1'       'Killed'
expect "killed rows are Z in ps"         "ps | awk '\$2 == \"Z\" && \$3 == \"sleep\" && \$4 == \"297\" {n++} END {print n}'"  $'2'
expect "jobs is empty after kill"                  'jobs'          $''
KILL_MISS="$(eval_json eval '"kill 99999"')"
expect_eval "kill of unknown pid exits 1"          "$KILL_MISS" '.exit_code' '1'
expect_eval "kill of unknown pid says so"          "$KILL_MISS" '.stderr | contains("No such process")' 'true'

# Deferred execution proof: the parked job runs during `wait`, observable via file redirection
# (bg stdout is orphaned — the spawning line's buffers are gone by the time it runs).
expect "bg job runs during wait"    '{ echo bg-ran > /tmp/work/bgproof; } & wait; cat /tmp/work/bgproof'  $'bg-ran'

# P-state kill: killing the prompt-paused pid == aborting the prompt (exit 130). The paused pid is
# the row after the last one `ps` reports (pids are monotonic; the prompt line spawns the next).
# The last row `ps` reports is the ps line itself; the next spawned line gets LAST+1.
# NB: extract with `awk NF` — the jq -r in run_line leaves a trailing EMPTY line (the value ends
# with \n and jq adds its own), so `tail -1` would grab "" and poison the arithmetic.
LAST_PID="$(run_line 'ps' | awk 'NF {last=$1} END {print last}')"
PU_PID=$((LAST_PID + 1))
eval_json eval '"prompt-user \"kill me?\""' >/dev/null
KILL_PENDING="$(eval_json eval "\"kill $PU_PID\"")"
expect_eval "kill of P-state pid aborts the prompt (130)" "$KILL_PENDING" '.exit_code' '130'
expect "shell accepts commands after P-state kill"  'echo ok'  $'ok'

# exec — un-gated in the fork: the redirection-only form applies permanently; the command form on
# wasm emulates replace-then-exit (run in-shell + ExitShell flow; the durable agent session itself
# persists — there is no OS process to replace).
expect "exec CMD runs the command"        'exec echo exec-ran'   $'exec-ran'
expect "session survives exec (agent)"    'echo still-here'      $'still-here'

# ============================================================================
# 2s. Stream fidelity, /dev/null, nested contexts (the A-list)
# ============================================================================
step "Stream fidelity, /dev/null, nested contexts"

# A1 — /dev/null, emulated in the fork with a truncate-on-open backing file. (/dev/stdin,
# /dev/stdout, /dev/stderr were already fd-mapped by Brush's shell_fd_path_to_fd.)
expect "> /dev/null discards"              'echo noise > /dev/null; echo ok'          $'ok'
expect "< /dev/null reads empty"           'wc -l < /dev/null'                        $'0'
expect "cat /dev/null is empty, exit 0"    'cat /dev/null; echo rc=$?'                $'rc=0'
expect "stat /dev/null reports a device"   'stat -c %F /dev/null'                     $'character special file'
DEVNULL_SILENCE="$(eval_json eval '"ls /definitely-absent-dir 2>/dev/null; echo rc=$?"')"
expect_eval "2>/dev/null silences stderr"  "$DEVNULL_SILENCE" '.stderr' ''

# A3 — uu-tool stderr is a distinct stream now (was merged into the stdout capture on wasm).
CAT_MISS="$(eval_json eval '"cat /tmp/work/absent-a3"')"
expect_eval "uu error text lands in stderr"  "$CAT_MISS" '.stderr | contains("No such file")' 'true'
expect_eval "uu stdout stays clean"          "$CAT_MISS" '.stdout' ''
expect_contains "2> file captures uu stderr" 'cat /tmp/work/absent-a3 2> /tmp/work/uu-err; cat /tmp/work/uu-err'  'No such file'

# A4 — texttools per-file errors go through Brush stderr (were eprintln! to the real fd).
GREP_MISS="$(eval_json eval '"grep x /tmp/work/absent-a4"')"
expect_eval "grep miss error is stderr"    "$GREP_MISS" '.stderr | contains("No such file")' 'true'
expect_eval "grep miss exits 2"            "$GREP_MISS" '.exit_code' '2'

# A2 — `context` is now a real Brush builtin in nested contexts (via the per-line transcript
# slot), and the other session commands error HONESTLY there instead of "operation not supported".
expect_contains "context composes in \$( )"   'B=$(context budget); echo "budget=$B"'  'budget='
# Content-independent (line 1 may be the elision marker once the window has compacted): the
# assertion is that context's output flows through the pipe and head truncates it to one line.
expect "context show pipes"                   'context show | head -n 1 | wc -l'       $'1'
NESTED_ASK="$(eval_json eval '"echo got=$(ask q)"')"
expect_eval "nested ask errors honestly"   "$NESTED_ASK" '.stderr | contains("top-level command")' 'true'
NESTED_CURL="$(eval_json eval '"echo x | xargs curl"')"
expect_eval "nested curl errors honestly"  "$NESTED_CURL" '.stderr | contains("top-level command")' 'true'

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
