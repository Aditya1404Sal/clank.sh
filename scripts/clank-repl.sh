#!/usr/bin/env bash
#
# clank-repl.sh — an interactive REPL against a deployed clank Golem agent.
#
# Each line you type is sent to the agent's single entry point, `eval`, and the result is printed.
# Because the agent is durable and addressed by a stable name, the whole session — cwd, files,
# transcript, installed packages — persists across lines AND across REPL restarts (same --name
# resumes the same live instance). It also transparently drives clank's human-in-the-loop pauses:
# when a command surfaces a confirmation (a bare `rm`, `curl`, `ask`, or `prompt-user`), the REPL
# shows the question, reads your answer, and resolves it via `answer_prompt`/`abort_prompt`.
#
# Usage:   scripts/clank-repl.sh [--name <agent>] [--deploy] [--takeover] [--keep]
#   --name <agent>  agent instance name = its durable identity (default: "repl"). A new name is a
#                   fresh, isolated shell; reusing a name resumes that shell's persisted state.
#   --deploy        build + start a throwaway local Golem server and deploy clank first, then REPL,
#                   then tear it all down on exit. Without this the REPL ATTACHES to whatever server
#                   is already running + deployed (do `golem -Y build && golem server run &` then
#                   `golem -Y deploy` first — see USAGE.md "Running clank on a Golem agent").
#   --takeover      (--deploy only) kill any golem server already bound to the port instead of refusing.
#   --keep          (--deploy only) leave the server + data dir up after the REPL exits.
#
# Non-interactive: pipe commands on stdin (`echo 'ls /bin' | scripts/clank-repl.sh`) to run them in
# order against one agent and print each result — a scriptable driver, no prompt.
#
# Meta-commands (interactive):  exit | quit | :q  → leave    ·    :help → this help    ·    Ctrl-D → leave
# Line editing (arrows/backspace) is on via readline. Prefix a command with `sudo` to pre-authorize
# it and skip the confirmation pause.
#
# Requires: golem (>=1.5), jq  (and cargo, only for --deploy). Run from anywhere inside the repo.

set -uo pipefail

# --- locate the repo root (dir containing golem.yaml) so the script is cwd-independent ---
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT" || { echo "cannot cd to repo root $REPO_ROOT" >&2; exit 1; }

AGENT_NAME="repl"
DEPLOY_MODE=0
TAKEOVER=0
KEEP=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --name)     AGENT_NAME="${2:?--name needs a value}"; shift 2 ;;
    --name=*)   AGENT_NAME="${1#*=}"; shift ;;
    --deploy)   DEPLOY_MODE=1; shift ;;
    --takeover) TAKEOVER=1; shift ;;
    --keep)     KEEP=1; shift ;;
    -h|--help)  sed -n '2,29p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; echo "usage: $0 [--name <agent>] [--deploy] [--takeover] [--keep]" >&2; exit 2 ;;
  esac
done

# Same default router port as the other scripts — the CLI profile targets it.
ROUTER_PORT=9881
AGENT_TYPE="ClankAgent"
AGENT_ID="${AGENT_TYPE}(\"${AGENT_NAME}\")"

c_grn=$'\033[32m'; c_red=$'\033[31m'; c_dim=$'\033[2m'; c_cyn=$'\033[36m'; c_yel=$'\033[33m'; c_rst=$'\033[0m'
note() { echo "${c_dim}·· $*${c_rst}"; }
step() { echo; echo "▸ $*"; }
warn() { echo "${c_red}$*${c_rst}" >&2; }

command -v golem >/dev/null 2>&1 || { warn "golem CLI not found (need >=1.5)"; exit 1; }
command -v jq    >/dev/null 2>&1 || { warn "jq not found (needed to parse agent results)"; exit 1; }

# Where `golem agent invoke` stderr (invocation markers, agent-side panics/tracing) is captured, so
# it doesn't clutter the REPL. In --deploy mode this is the throwaway server's log; otherwise a temp.
SERVER_PID=""
DATA_DIR=""
if [[ $DEPLOY_MODE -eq 1 ]]; then
  DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/clank-repl.XXXXXX")"
  SESSION_LOG="$DATA_DIR/server.log"
else
  SESSION_LOG="$(mktemp "${TMPDIR:-/tmp}/clank-repl-log.XXXXXX")"
fi

# --- teardown: tear down the throwaway server (deploy mode) / clean the temp log (attach mode) ---
AGENTS_BACKUP=""
cleanup() {
  local ec=$?
  [[ -n "$AGENTS_BACKUP" && -f "$AGENTS_BACKUP" ]] && cp "$AGENTS_BACKUP" AGENTS.md
  if [[ $DEPLOY_MODE -eq 1 && $KEEP -eq 1 ]]; then
    note "--keep: server pid ${SERVER_PID:-?}, data dir $DATA_DIR"
    note "stop it later with:  kill ${SERVER_PID:-<pid>}"
    exit $ec
  fi
  if [[ $DEPLOY_MODE -eq 1 && -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null
    for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$SERVER_PID" 2>/dev/null || break; sleep 0.3; done
    kill -9 "$SERVER_PID" 2>/dev/null
    note "server stopped"
  fi
  [[ $DEPLOY_MODE -eq 1 ]] && rm -rf "$DATA_DIR" || rm -f "$SESSION_LOG"
  # `exit` (not `return`) in an EXIT trap is what actually sets the final status.
  exit $ec
}
trap cleanup EXIT INT TERM

# ============================================================================
# --deploy: stand up a throwaway server + deploy clank (mirrors golem-probe.sh)
# ============================================================================
if [[ $DEPLOY_MODE -eq 1 ]]; then
  command -v cargo >/dev/null 2>&1 || { warn "--deploy needs cargo (to build the wasm component)"; exit 1; }
  # ask's golem.yaml uses strict Jinja substitution for the key; empty is fine for a no-LLM REPL.
  export ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}"

  # Snapshot AGENTS.md so build/deploy churn doesn't leak into the tree.
  AGENTS_BACKUP="$DATA_DIR/AGENTS.md.orig"
  [[ -f AGENTS.md ]] && cp AGENTS.md "$AGENTS_BACKUP"

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
      warn "A golem server is already on port $ROUTER_PORT (pid $existing). Re-run with --takeover, or drop --deploy to attach to it."
      exit 1
    fi
  fi

  step "Building the wasm component (golem build)"
  golem -Y build 2>&1 | tail -4 || { warn "golem build failed"; exit 1; }

  step "Starting throwaway golem server (data dir: $DATA_DIR)"
  golem server run --clean --router-port "$ROUTER_PORT" --data-dir "$DATA_DIR" --ports-file "$DATA_DIR/ports.json" \
    >"$SESSION_LOG" 2>&1 &
  SERVER_PID=$!
  note "server pid $SERVER_PID — waiting for it to come up..."
  for i in $(seq 1 60); do
    kill -0 "$SERVER_PID" 2>/dev/null || { warn "server exited early — see $SESSION_LOG"; tail -20 "$SESSION_LOG" >&2; exit 1; }
    if [[ -f "$DATA_DIR/ports.json" ]] && golem component list >/dev/null 2>&1; then note "server ready after ${i}s"; break; fi
    sleep 1
    [[ $i -eq 60 ]] && { warn "server did not become ready in 60s"; tail -20 "$SESSION_LOG" >&2; exit 1; }
  done

  step "Deploying clank:agent (golem deploy)"
  golem -Y deploy 2>&1 | tail -5 || { warn "golem deploy failed"; exit 1; }
else
  # Attach mode: verify a server is actually reachable before we start the loop.
  if ! golem component list >/dev/null 2>&1; then
    warn "No golem server reachable on port $ROUTER_PORT."
    echo "  Start one and deploy clank first (see USAGE.md 'Running clank on a Golem agent'):" >&2
    echo "    golem -Y build && golem server run &        # then, once it is ready:" >&2
    echo "    golem -Y deploy" >&2
    echo "  …or re-run this script with --deploy to do all of that in a throwaway server." >&2
    exit 1
  fi
fi

# ============================================================================
# Invocation helpers
# ============================================================================
# Render a bash string as a WIT string literal argument (escape embedded quotes, like the e2e/probe).
wit_str() { printf '"%s"' "${1//\"/\\\"}"; }

# Invoke an agent method and echo the structured EvalResult object (.result_json.value). The JSON
# result document is the LAST result_json line (invocation markers/log lines print first).
invoke() {
  local method="$1"; shift
  golem agent invoke -q --format json "$AGENT_ID" "$method" "$@" 2>>"$SESSION_LOG" \
    | grep '"result_json"' | tail -1 | jq -c '.result_json.value // empty' 2>/dev/null
}

# Print an EvalResult's stdout/stderr, and surface a non-zero exit. stdout is emitted verbatim (it
# already carries its own newlines); stderr is dimmed. Returns nothing about pending — see below.
render() {
  local j="$1"
  local out err code
  out="$(jq -r '.stdout // ""' <<<"$j")"
  err="$(jq -r '.stderr // ""' <<<"$j")"
  code="$(jq -r '.exit_code // 0' <<<"$j")"
  [[ -n "$out" ]] && { printf '%s' "$out"; [[ "$out" == *$'\n' ]] || echo; }
  [[ -n "$err" ]] && printf '%s%s%s' "$c_dim" "$err" "$c_rst" >&2 && { [[ "$err" == *$'\n' ]] || echo >&2; }
  [[ "$code" != "0" ]] && note "exit $code"
}

# Resolve clank's human-in-the-loop pause: show the pending question (+ choices), read the human's
# answer, deliver it via answer_prompt (or abort_prompt on :abort / EOF), and loop until it clears
# (an out-of-set --choices answer leaves the prompt pending, so we re-ask).
resolve_prompt() {
  local pend="$1" j q choices ans
  while [[ -n "$pend" ]]; do
    q="$(jq -r '.question // ""' <<<"$pend")"
    choices="$(jq -r 'if .choices then " [" + (.choices | join(", ")) + "]" else "" end' <<<"$pend")"
    printf '%s⏸  %s%s%s\n' "$c_yel" "$q" "$choices" "$c_rst"
    if [[ $INTERACTIVE -eq 1 ]]; then
      IFS= read -e -r -p "  answer (:abort to cancel)> " ans || { echo; ans=":abort"; }
    else
      IFS= read -r ans || ans=":abort"
    fi
    if [[ "$ans" == ":abort" ]]; then
      render "$(invoke abort_prompt)"
      return
    fi
    j="$(invoke answer_prompt "$(wit_str "$ans")")"
    pend="$(jq -c '.pending_prompt // empty' <<<"$j")"
    if [[ -n "$pend" ]]; then
      # Invalid answer — the agent kept the prompt pending. Show just the reason; the loop re-asks.
      printf '%s%s%s\n' "$c_red" "$(jq -r '.stderr // ""' <<<"$j")" "$c_rst" >&2
    else
      render "$j"   # resolved — print the command's real output/exit
    fi
  done
}

# Run one command line: eval it, then either resolve a pause it raised or print its result. clank's
# authz/prompt-user gate pauses BEFORE the command runs, so a pending result's stdout is just the
# question — `resolve_prompt` owns displaying it (styled, with choices), so we don't also `render` it.
run_command() {
  local j; j="$(invoke eval "$(wit_str "$1")")"
  if [[ -z "$j" ]]; then
    warn "no result from the agent — is clank deployed on this server? (tail: $SESSION_LOG)"
    return
  fi
  local pend; pend="$(jq -c '.pending_prompt // empty' <<<"$j")"
  if [[ -n "$pend" ]]; then resolve_prompt "$pend"; else render "$j"; fi
}

# ============================================================================
# The REPL loop
# ============================================================================
INTERACTIVE=0; [[ -t 0 ]] && INTERACTIVE=1
if [[ $INTERACTIVE -eq 1 ]]; then
  set -o history 2>/dev/null || true   # enable in-session up-arrow recall for `read -e`
  step "clank REPL — agent ${c_cyn}${AGENT_ID}${c_rst}"
  note "durable session (state persists across lines & restarts for this --name). :help · exit/Ctrl-D to quit."
fi
PROMPT="clank:${AGENT_NAME}$ "

while true; do
  if [[ $INTERACTIVE -eq 1 ]]; then
    IFS= read -e -r -p "$PROMPT" line || { echo; break; }
  else
    IFS= read -r line || break
  fi
  case "$line" in
    "")            continue ;;
    exit|quit|:q)  break ;;
    :help)         sed -n '2,29p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; continue ;;
  esac
  [[ $INTERACTIVE -eq 1 ]] && history -s "$line" 2>/dev/null || true
  run_command "$line"
done

if [[ $INTERACTIVE -eq 1 ]]; then echo; note "bye"; fi
