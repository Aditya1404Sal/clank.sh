# shellcheck shell=bash
#
# golem-json.sh — the ONE invoke-JSON decoder, and the artifact-freshness check.
#
# Source this from any script that drives a deployed clank agent:
#
#   . "$(dirname "${BASH_SOURCE[0]}")/lib/golem-json.sh"
#
# Callers set, before use:
#   AGENT_ID        the agent to invoke (e.g. ClankAgent("demo"))
#   GOLEM_JSON_LOG  where to append the CLI's stderr        (default: /dev/null)
#   GOLEM_BIN       the golem binary                        (default: golem)
#
# ---------------------------------------------------------------------------------------------
# WHY THIS FILE EXISTS
#
# The decode below lived in four places at once and drifted in three of them. Both mismatch modes
# fail SILENTLY — the wrong key greps nothing, the wrong shape finds no path — so a perfectly
# healthy agent reads back as entirely blank, which is indistinguishable from a wedged instance.
# That has already cost real debugging time twice: a documented "4 passed, 288 failed" false
# catastrophe when the dev SDK first landed, and again in 2026-09 when `golem-probe.sh` was found
# still carrying the released-only reader while `clank-repl.sh` was outright broken by it.
#
# So: one implementation, and it accepts BOTH wire shapes rather than betting on either.
#
#   released CLI : {"result_json": {"value": {"stdout": …, "stderr": …, …}}}   -- named fields
#   dev CLI      : {"resultJson":  {"value": {"value": {"fields": [ … ]}}}}    -- POSITIONAL
#
# In the positional shape the field NAMES live in the type graph, not the value, so the order in
# `clank_agent.rs`'s `EvalResult` / `PendingPromptView` is a WIRE CONTRACT for this decoder:
# [0]=stdout [1]=stderr [2]=exit_code [3]=pending_prompt. Reordering those fields silently
# mis-assigns every value here. `crates/clank-conformance/src/backend/golem.rs::decode_invoke` is
# the Rust twin of this function and must stay in step.
# ---------------------------------------------------------------------------------------------

# Try the named shape first (so a released CLI is untouched), then the positional dev-SDK shape.
GOLEM_EVAL_REMAP='
  if has("result_json") then
    .result_json.value
  else
    .resultJson.value.value.fields as $f
    | { stdout: $f[0].value,
        stderr: $f[1].value,
        exit_code: $f[2].value,
        pending_prompt:
          (if $f[3].value.inner == null then null
           else ($f[3].value.inner.value.fields as $p
                 | { question: $p[0].value,
                     choices: (if $p[1].value.inner == null then null
                               else [ $p[1].value.inner.value.elements[].value ] end) })
           end) }
  end'

# Invoke a method returning the structured `EvalResult` and print it as a flat
# {stdout, stderr, exit_code, pending_prompt} JSON object.
#   golem_eval_json <method> [wit-arg-literal...]
golem_eval_json() {
  local method="$1"; shift
  "${GOLEM_BIN:-golem}" agent invoke -q --format json "$AGENT_ID" "$method" "$@" \
    2>>"${GOLEM_JSON_LOG:-/dev/null}" \
    | grep -E '"result_json"|"resultJson"' \
    | tail -1 \
    | jq -c "$GOLEM_EVAL_REMAP" 2>/dev/null
}

# Invoke `eval` for one command line and print stdout+stderr merged, as a terminal would show it.
#   golem_run_line <command line>
golem_run_line() {
  local cmd="$1"
  # ONE positional per function arg; the arg is a WIT string literal → wrap in quotes.
  "${GOLEM_BIN:-golem}" agent invoke -q --format json "$AGENT_ID" eval "\"${cmd//\"/\\\"}\"" \
    2>>"${GOLEM_JSON_LOG:-/dev/null}" \
    | grep -E '"result_json"|"resultJson"' \
    | tail -1 \
    | jq -r "$GOLEM_EVAL_REMAP | (.stdout // \"\") + (.stderr // \"\")" 2>/dev/null
}

# ---------------------------------------------------------------------------------------------
# Artifact freshness.
#
# `golem build` tracks the `clank-agent` COMPONENT DIRECTORY, not its path dependencies — so an
# edit to `clank-core` or `clank-embed` does not invalidate it and the build reports `[UP-TO-DATE]`
# while deploying a wasm that predates the change. The harness then runs a full suite against code
# it never compiled and prints an authoritative-looking tally.
#
# That is not hypothetical. In one 2026-09-11 session it produced a fabricated 80/272 FAILURE
# against a binary that never contained the code under test, and later a false failure against a
# fix that was never deployed. An assertion that happened to match the stale behaviour would have
# produced a false PASS instead — the same defect, silent.
#
# The remedy was documented in four markdown files and still bit three times in that one session,
# because a comment is not a mechanism. This is the mechanism: delete the stale artifact so
# `golem build` must re-run cargo, which then rebuilds exactly what changed.
# ---------------------------------------------------------------------------------------------
golem_assert_fresh_artifact() {
  local wasm newer stale=() culprit=""
  # Enumerate with `find`, not a glob: an unmatched glob behaves differently across shells, and a
  # missing golem-temp/ must be a silent no-op, not an error. Likewise `find -newer` rather than
  # stat(1), whose mtime flag differs between BSD and GNU.
  while IFS= read -r wasm; do
    # `fork` too: the coreutils submodule is a path dependency compiled into the agent, so an edit
    # there changes the component while leaving every file under crates/ untouched.
    newer="$(find crates utilities fork \( -name '*.rs' -o -name '*.toml' \) \
               -newer "$wasm" -print -quit 2>/dev/null)"
    [[ -n "$newer" ]] || continue
    stale+=("$wasm")
    [[ -n "$culprit" ]] || culprit="$newer"
  done < <(find golem-temp/agents -name '*.wasm' 2>/dev/null)

  [[ ${#stale[@]} -gt 0 ]] || return 0

  echo "  stale: ${#stale[@]} component artifact(s) predate $culprit — removing so the build is real" >&2
  rm -f "${stale[@]}"
  # Drop cargo's staged copies too. Deleting only golem-temp's would let `golem build` re-stage the
  # same stale wasm from target/ without ever re-running cargo, which is the whole failure mode.
  # (cargo itself tracks path deps correctly — the gap is purely whether it gets invoked at all.)
  while IFS= read -r wasm; do
    rm -f "$wasm"
  done < <(find target/wasm32-wasip2 -maxdepth 2 -name '*.wasm' 2>/dev/null)
}
