#!/bin/sh
# Status: the complete live cell passed for release 0.6.2 (revision 2db699f,
# codex-cli 0.156.1); the earlier blocker — the operator launcher's accepted
# codex-managed profile record predating the installed CLI, refused by the
# managed host with the closed `execution_profile` code — was resolved by
# re-minting that record.
#
# Measured `codex exec --json` transcript shape (codex-cli 0.155.1): one JSON
# object per line. A session opens with `{"type":"thread.started","thread_id":..}`
# and `{"type":"turn.started"}`; every tool call is a pair of events carrying an
# `item` object with `type:"mcp_tool_call"`, `server:"agent-ide"`, the dotted
# `tool:"ide.start"` spelling, `arguments`, and on `item.completed` a
# `result:{content,structured_content}` plus `status:"completed"|"failed"`. A
# turn closes with `{"type":"turn.completed","usage":{...}}`. Example line,
# values redacted:
#   {"type":"item.completed","item":{"id":"item_8","type":"mcp_tool_call",
#    "server":"agent-ide","tool":"ide.edit","arguments":{...},
#    "result":{"content":[..],"structured_content":{..}},"error":null,
#    "status":"completed"}}

# Real-host driver for the direct Codex acceptance route.
#
# The runner starts this executable with AGENT_IDE_ACCEPTANCE_ROUTE=codex, the
# two isolated fixture worktrees, and a fresh result path. The driver runs real
# bounded `codex exec --json` sessions against the candidate binary's managed
# MCP server, verifies every scenario from the captured JSONL event transcripts
# plus real filesystem and telemetry effects, and only then emits the closed
# ten-line real_pass document. Any failing step records a closed code in the
# private diagnostic log and fails the whole cell honestly.
#
# Managed Codex mode binds `ide.start` directly from the trusted MCP `_meta`
# attachment, so there is no foreground Bash helper step: a pending tool answer
# must be followed by `ide.inspect` with the returned `detail_ref`, which the
# prompt rules require. Every session uses a private CODEX_HOME in a per-run
# temporary directory containing only the candidate binary as the `agent-ide`
# MCP server, the candidate's own `codex-hooks print` fragment, and the
# operator's Codex auth linked in place; the auth file is never copied, read,
# or printed.
#
# Each scenario session is attempted up to three times because a model
# occasionally drops a scripted step; every retry first restores the exact
# fixture precondition, so an attempt always starts from the same worktree
# state and a later attempt can never inherit a partial effect.
#
# Operator environment (all optional, defaults fit the release host):
#   AGENT_IDE_ACCEPTANCE_BINARY       candidate agent-ide executable; default
#                                     <repo>/target/release/agent-ide.
#   AGENT_IDE_ACCEPTANCE_CODEX        real Codex CLI; default
#                                     /Users/pluto/.nvm/versions/node/v24.4.0/bin/codex.
#   AGENT_IDE_ACCEPTANCE_LAUNCHER     launcher template carrying `allowed_roots`
#                                     that contain the fixture worktrees and the
#                                     accepted Pyright and TypeScript r3 providers;
#                                     default /Users/pluto/.config/agent-ide/launcher.json.
#   AGENT_IDE_ACCEPTANCE_OPERATOR_HOME operator home holding the authenticated
#                                     ~/.codex/auth.json; default /Users/pluto.
#   AGENT_IDE_ACCEPTANCE_MODEL        model alias; default gpt-5.6-luna.
#   AGENT_IDE_ACCEPTANCE_SESSION_SECONDS wall-clock bound per session; default
#                                     900.
#   AGENT_IDE_ACCEPTANCE_FIRST_EVENT_SECONDS first-event watchdog; a session
#                                     whose transcript is still empty this many
#                                     seconds after launch is killed and coded
#                                     E_SESSION_NO_START instead of burning the
#                                     whole session bound on a dead start.
#                                     Default 60.
#   AGENT_IDE_ACCEPTANCE_ONLY        optional comma-separated scenario filter
#                                    (l1,l1b,l2,l3,l4,l5,r5,r5b) for single-scenario
#                                    diagnostic reruns; unset runs the complete
#                                    cell, and a filtered run never emits the
#                                    closed result document.
#   AGENT_IDE_ACCEPTANCE_DRY          1 prints the exact session commands and
#                                     writes nothing.

set -eu

DRIVER_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$DRIVER_DIR/driver-common.sh"

# Maximum model-session attempts per scenario before the driver fails.
MAX_ATTEMPTS=3
# A session must record its first transcript event within this window; a dead
# start is killed instead of consuming the full session bound (T34B).
FIRST_EVENT_SECONDS=${AGENT_IDE_ACCEPTANCE_FIRST_EVENT_SECONDS:-60}
# Host-neutral acceptance prompt family shared with every other driver.
PROMPT_FAMILY=prompts

# Optional single-scenario diagnostic filter; empty means the complete cell.
ONLY=${AGENT_IDE_ACCEPTANCE_ONLY:-}
selected() {
    [ -z "$ONLY" ] && return 0
    case ",$ONLY," in *",$1,"*) return 0 ;; esac
    return 1
}

[ "${AGENT_IDE_ACCEPTANCE_ROUTE:-}" = codex ] || fail E_ROUTE "route is not codex"
[ -n "${AGENT_IDE_ACCEPTANCE_LEFT_WORKTREE:-}" ] || fail E_LEFT_WORKTREE_MISSING
[ -n "${AGENT_IDE_ACCEPTANCE_RIGHT_WORKTREE:-}" ] || fail E_RIGHT_WORKTREE_MISSING
[ -n "${AGENT_IDE_ACCEPTANCE_RESULT:-}" ] || fail E_RESULT_PATH_MISSING

BINARY=${AGENT_IDE_ACCEPTANCE_BINARY:-$DRIVER_ROOT/target/release/agent-ide}
CODEX=${AGENT_IDE_ACCEPTANCE_CODEX:-/Users/pluto/.nvm/versions/node/v24.4.0/bin/codex}
LAUNCHER=${AGENT_IDE_ACCEPTANCE_LAUNCHER:-/Users/pluto/.config/agent-ide/launcher.json}
OPERATOR_HOME=${AGENT_IDE_ACCEPTANCE_OPERATOR_HOME:-/Users/pluto}
MODEL=${AGENT_IDE_ACCEPTANCE_MODEL:-gpt-5.6-luna}
SESSION_SECONDS=${AGENT_IDE_ACCEPTANCE_SESSION_SECONDS:-900}
DRY=${AGENT_IDE_ACCEPTANCE_DRY:-0}

# Prints the exact `config.toml` the private CODEX_HOME receives: the built-in
# `:workspace` permission profile and agent-ide MCP server. One source of truth
# for the live run and the dry printout.
print_codex_config() {
    cat <<CONFIG
default_permissions = ":workspace"

[mcp_servers.agent-ide]
command = "$BINARY"
args = ["mcp", "--launcher-template", "$LAUNCHER"]
default_tools_approval_mode = "approve"
startup_timeout_sec = 120.0
CONFIG
}

# Dry mode: print every exact command the live run would execute and exit
# without writing anything, including the diagnostic log.
if [ "$DRY" = 1 ]; then
    printf '%s\n' '# codex route driver — live sessions; dry printout only'
    printf '%s\n' '# private CODEX_HOME/config.toml:'
    print_codex_config
    printf '%s\n' "# $BINARY codex-hooks print > <codex-home>/hooks.json"
    printf '%s\n' "# ln $OPERATOR_HOME/.codex/auth.json <codex-home>/auth.json  # file_link, never read or printed"
    printf '%s\n' "# $CODEX exec --help | grep -q -- --dangerously-bypass-hook-trust  # else fail hook_trust_flag_missing"
    for label in l1 l1b l2 l3 l4 l5; do
        printf 'cd "%s" && CODEX_HOME="<codex-home>" HOME="%s" PATH="/usr/bin:/bin:/usr/sbin:/sbin:%s" /usr/bin/perl -e '"'"'alarm shift; exec @ARGV'"'"' %s "%s" exec --json -C "%s" --skip-git-repo-check -m "%s" -o "<diag>/last-%s.txt" "$(cat "%s/%s.txt")" > "<diag>/transcript-%s.jsonl" 2> "<diag>/session-%s.err"\n' \
            "$AGENT_IDE_ACCEPTANCE_LEFT_WORKTREE" "$OPERATOR_HOME" "$(dirname -- "$CODEX")" \
            "$SESSION_SECONDS" "$CODEX" "$AGENT_IDE_ACCEPTANCE_LEFT_WORKTREE" \
            "$MODEL" "$label" "$DRIVER_DIR/$PROMPT_FAMILY" "$label" "$label" "$label"
    done
    for label in r5 r5b; do
        printf 'cd "%s" && CODEX_HOME="<codex-home>" HOME="%s" PATH="/usr/bin:/bin:/usr/sbin:/sbin:%s" /usr/bin/perl -e '"'"'alarm shift; exec @ARGV'"'"' %s "%s" exec --json -C "%s" --skip-git-repo-check -m "%s" -o "<diag>/last-%s.txt" "$(cat "%s/%s.txt")" > "<diag>/transcript-%s.jsonl" 2> "<diag>/session-%s.err"\n' \
            "$AGENT_IDE_ACCEPTANCE_RIGHT_WORKTREE" "$OPERATOR_HOME" "$(dirname -- "$CODEX")" \
            "$SESSION_SECONDS" "$CODEX" "$AGENT_IDE_ACCEPTANCE_RIGHT_WORKTREE" \
            "$MODEL" "$label" "$DRIVER_DIR/$PROMPT_FAMILY" "$label" "$label" "$label"
    done
    exit 0
fi

[ -x "$BINARY" ] || fail E_BINARY_MISSING "$BINARY"
[ -x "$CODEX" ] || fail E_CODEX_MISSING "$CODEX"
[ -r "$LAUNCHER" ] || fail E_LAUNCHER_MISSING "$LAUNCHER"
[ -d "$OPERATOR_HOME" ] || fail E_OPERATOR_HOME_MISSING "$OPERATOR_HOME"

LEFT=$(canonical_dir "$AGENT_IDE_ACCEPTANCE_LEFT_WORKTREE") || fail E_LEFT_CANONICAL
RIGHT=$(canonical_dir "$AGENT_IDE_ACCEPTANCE_RIGHT_WORKTREE") || fail E_RIGHT_CANONICAL
LEFT_IDENTITY=$(project_identity "$BINARY" "$LEFT") || fail E_LEFT_IDENTITY
[ "${#LEFT_IDENTITY}" = 64 ] || fail E_LEFT_IDENTITY_LENGTH

mkdir -p -- "$DIAG_DIR"

# The private CODEX_HOME lives in a per-run temporary directory removed at
# exit; it never holds a copy of the auth secret, only a link to the
# operator's file, and nothing inside it is ever printed or logged.
RUN_TMP=$(mktemp -d "${TMPDIR:-/tmp}/agent-ide-codex-driver.XXXXXX") || fail E_CODEX_HOME
CODEX_HOME_DIR=$RUN_TMP/codex-home
cleanup_run() {
    if [ -n "$RUN_TMP" ] && [ -d "$RUN_TMP" ]; then
        rm -rf -- "$RUN_TMP"
    fi
}
trap cleanup_run EXIT HUP INT TERM

mkdir -p -- "$CODEX_HOME_DIR" || fail E_CODEX_HOME

# TOML string values below are plain absolute paths; refuse anything that
# would need escaping instead of emitting a broken config.
case "$BINARY$LAUNCHER" in
    *[\"\\]*) fail E_CODEX_QUOTING "binary or launcher path needs TOML escaping" ;;
esac
print_codex_config >"$CODEX_HOME_DIR/config.toml" || fail E_CODEX_CONFIG

# Hooks exec the CANDIDATE: `codex-hooks print` renders the managed fragment
# for the running executable's canonical path.
"$BINARY" codex-hooks print >"$CODEX_HOME_DIR/hooks.json" 2>>"$DIAG_LOG" \
    || fail E_CODEX_HOOKS_PRINT
jq -e . "$CODEX_HOME_DIR/hooks.json" >/dev/null 2>&1 || fail E_CODEX_HOOKS_JSON

# The operator's Codex auth is linked in, mirroring the agent-run
# [runtimes.codex.auth] file_link pattern (source ~/.codex/auth.json, target
# auth.json). A hard link shares the operator's inode, so the link is never
# chmodded; a symlink is the fallback across devices. The content is never
# read, copied, printed, or logged.
[ -f "$OPERATOR_HOME/.codex/auth.json" ] || fail E_CODEX_AUTH_SOURCE
ln "$OPERATOR_HOME/.codex/auth.json" "$CODEX_HOME_DIR/auth.json" 2>>"$DIAG_LOG" \
    || ln -s "$OPERATOR_HOME/.codex/auth.json" "$CODEX_HOME_DIR/auth.json" 2>>"$DIAG_LOG" \
    || fail E_CODEX_AUTH_LINK

# The exact exec flags are verified against this installed CLI before any
# session runs. A private CODEX_HOME has no trusted hook hashes, so hook trust
# is bypassed for this invocation only when the installed CLI documents the
# flag; otherwise the driver fails closed. Approvals and sandbox are NEVER
# bypassed: sessions use the private home's named `:workspace` profile.
CODEX_HELP=$("$CODEX" exec --help 2>>"$DIAG_LOG") || fail E_CODEX_HELP
for flag in --json -C --skip-git-repo-check -m -o --dangerously-bypass-hook-trust; do
    printf '%s\n' "$CODEX_HELP" | grep -q -- "$flag" \
        || fail E_CODEX_FLAG "$flag is not documented by codex exec --help"
done
if printf '%s\n' "$CODEX_HELP" | grep -q -- --dangerously-bypass-hook-trust; then
    HOOK_TRUST_FLAG=--dangerously-bypass-hook-trust
else
    fail hook_trust_flag_missing "installed codex cannot run untrusted hooks"
fi

# PATH carries the system directories plus the Codex launcher's own directory:
# the launcher resolves `node` through PATH, and that directory holds no
# ambient agent-ide duplicate that could register a second host hook.
SESSION_PATH="/usr/bin:/bin:/usr/sbin:/sbin:$(dirname -- "$CODEX")"

# Runs one bounded `codex exec --json` session inside a worktree.
#
# The first argument is a short session label, the second the canonical
# worktree, and the third the prompt file. The session runs inside that
# worktree with the private CODEX_HOME and the operator's real home. The
# prompt is passed positionally and stdin is closed with `</dev/null`: a
# piped open stdin makes `codex exec` wait in "Reading additional input from
# stdin..." forever, which is exactly how the first live attempt burned three
# full 900 s alarms. A first-event watchdog kills a session that has recorded
# no transcript line after FIRST_EVENT_SECONDS and codes it E_SESSION_NO_START.
# Every inherited nested-session and managed-rendezvous variable of the
# invoking agent is removed, so the route really exercises the operator's Codex
# login instead of an enclosing wrapper. The complete JSONL event transcript
# stays in the private diagnostic directory.
# The edit ledger is durable per repository, so a retried scenario must never reuse
# an earlier attempt's operation_id (it would answer conflicting_duplicate). Each
# session gets its own suffix; verifiers never read the id.
SESSION_SEQ=0
# Called inside a command substitution (a subshell), so run_session advances
# SESSION_SEQ before the call.
session_prompt() {
    sed "s/\"operation_id\":\"\([A-Za-z0-9-]*\)\"/\"operation_id\":\"\1-$$-$SESSION_SEQ\"/g" "$1"
}

run_session() {
    SESSION_SEQ=$((SESSION_SEQ + 1))
    label=$1
    worktree=$2
    prompt_file=$3
    transcript=$DIAG_DIR/transcript-$label.jsonl
    rm -f -- "$transcript"
    (cd "$worktree" && CODEX_HOME="$CODEX_HOME_DIR" HOME="$OPERATOR_HOME" \
        PATH="$SESSION_PATH" \
        /usr/bin/env -u CODEX_SANDBOX -u CODEX_SANDBOX_NETWORK_DISABLED \
        -u CODEX_PID -u CODEX_THREAD_ID -u CODEX_SESSION_ID \
        -u AGENT_IDE_BIN -u AGENT_IDE_HOST_ATTACHMENT \
        -u AGENT_IDE_CODEX_RENDEZVOUS_ROOT \
        /usr/bin/perl -e 'alarm shift; exec @ARGV' "$SESSION_SECONDS" \
        "$CODEX" exec --json -C "$worktree" \
            --skip-git-repo-check -m "$MODEL" "$HOOK_TRUST_FLAG" \
            -o "$DIAG_DIR/last-$label.txt" "$(session_prompt "$prompt_file")" \
        >"$transcript" 2>"$DIAG_DIR/session-$label.err") </dev/null &
    session_pid=$!
    (
        sleep "$FIRST_EVENT_SECONDS"
        if kill -0 "$session_pid" 2>/dev/null && [ ! -s "$transcript" ]; then
            note E_SESSION_NO_START \
                "no transcript event within ${FIRST_EVENT_SECONDS}s in session-$label"
            kill -TERM "$session_pid" 2>/dev/null || :
        fi
    ) &
    watchdog_pid=$!
    status=0
    wait "$session_pid" || status=$?
    kill "$watchdog_pid" 2>/dev/null || :
    wait "$watchdog_pid" 2>/dev/null || :
    if [ "$status" -eq 0 ]; then
        note "session-$label-complete"
    else
        note "session-$label-status" "$status"
        return 1
    fi
    [ -s "$transcript" ] || return 1
    return 0
}

# Repeats one scenario session until its verify function accepts or attempts
# run out. The arguments are the label, worktree, prompt file, the verify
# function name, the reset function name, and the closed failure code.
run_scenario() {
    label=$1
    worktree=$2
    prompt_file=$3
    verify=$4
    reset=$5
    code=$6
    attempt=1
    while [ "$attempt" -le "$MAX_ATTEMPTS" ]; do
        note "scenario-$label-attempt" "$attempt"
        if run_session "$label" "$worktree" "$prompt_file" \
            && "$verify"; then
            note "scenario-$label-passed" "attempt $attempt"
            return 0
        fi
        [ -f "$DIAG_DIR/transcript-$label.jsonl" ] && mv -- \
            "$DIAG_DIR/transcript-$label.jsonl" \
            "$DIAG_DIR/transcript-$label-fail$attempt.jsonl"
        attempt=$((attempt + 1))
        "$reset" "$worktree"
    done
    fail "$code" "scenario $label never passed within $MAX_ATTEMPTS attempts"
}

# Prints how many transcript events record a tool call with the given name.
#
# `codex exec --json` emits thread events whose item objects carry the MCP
# tool call. The match is tolerant of drift around the measured 0.155 shape
# (`"type":"mcp_tool_call"`, `"tool":"ide.start"`, `"server":"agent-ide"`):
# only events that also stringify a known tool-call item type are counted, and
# the name may appear dotted ("tool":"ide.start"), underscored
# ("tool":"ide_start"), or host-prefixed ("mcp__agent-ide__ide_start").
codex_tool_call_count() {
    tool_pattern=$(printf '%s' "$2" | sed 's/_/[._]/g')
    jq -s --arg tool "$tool_pattern" '
        [.[]
         | tostring
         | select(test("mcp_tool_call|tool_call|tool_use|local_shell_call|file_change"))
         | select(test("\"(mcp__agent-ide__|agent-ide\\.)?" + $tool + "\""))]
        | length' "$1" 2>>"$DIAG_LOG"
}

# Records the first raw transcript events in the diagnostic log once, so a
# JSONL shape drift can be retuned from a real session without guessing.
record_codex_event_sample() {
    [ -s "$1" ] || return 0
    # BSD sed has no `--` end-of-options token, so the path is passed directly.
    sed -n '1,2p' "$1" 2>>"$DIAG_LOG" | while IFS= read -r line; do
        note "codex-event-sample-$(basename -- "$1")" "$line"
    done
}

# Requires at least one recorded tool call with the given name in a transcript.
require_codex_tool_use() {
    count=$(codex_tool_call_count "$1" "$2")
    [ "${count:-0}" -ge 1 ] || {
        note "$3" "expected codex tool call $2 in $(basename -- "$1")"
        record_codex_event_sample "$1"
        return 1
    }
}

# Requires one literal to appear in a transcript via the shared shape-agnostic
# function, sampling raw events on a miss.
require_text() {
    require_transcript_text "$1" "$2" "$3" || {
        record_codex_event_sample "$1"
        return 1
    }
}

# Requires one literal to be absent from a transcript.
forbid_text() {
    forbid_transcript_text "$1" "$2" "$3"
}

# Requires one native Codex file edit: a patch apply or file-change item, or a
# successful shell command naming the fixture file, since the L2 prompt allows
# either apply_patch or the shell.
require_codex_native_edit() {
    matched=$(jq -s '[.[] | select((tostring | test("\"file_change\"|apply_patch"))
        or ((.item.type? == "command_execution") and (.item.exit_code? == 0)
            and ((.item.command? // "") | contains("acceptance-fixture/fixture.py"))))]
        | length' "$1" 2>>"$DIAG_LOG")
    [ "${matched:-0}" -ge 1 ] || {
        note "$2" "no native codex file edit in $(basename -- "$1")"
        record_codex_event_sample "$1"
        return 1
    }
}

# Prints the arguments object of the first tool call with the given name.
#
# The measured shape carries call arguments under `item.arguments`; the older
# `input` and raw JSON string forms stay supported for drift, and the matcher
# accepts the same dotted, underscored, and host-prefixed name spellings as
# codex_tool_call_count.
codex_first_tool_input() {
    tool_pattern=$(printf '%s' "$2" | sed 's/_/[._]/g')
    jq -s --arg tool "$tool_pattern" '
        [.[]
         | (.item // .)
         | select(tostring
             | test("\"(mcp__agent-ide__|agent-ide\\.)?" + $tool + "\""))
         | (.arguments // .input // .arguments_json // empty)]
        | map(if type == "string" then (fromjson? // {}) else . end)
        | .[0] // null' "$1" 2>>"$DIAG_LOG"
}

# Requires every completed Codex MCP tool reply to stay compact: at most
# 16 KiB of serialized result per reply, matching the compact MCP projection
# contract.
require_codex_compact_replies() {
    oversized=$(jq -s --argjson bound 16384 '
        [.[]
         | (.item // empty)
         | select(.type? == "mcp_tool_call")
         | (.result // null)
         | select(. != null)
         | (tostring | length)
         | select(. > $bound)]
        | length' "$1" 2>>"$DIAG_LOG")
    [ "${oversized:-1}" -eq 0 ] || { note "$2" "oversized codex reply"; return 1; }
    measured=$(jq -s '[.[]
        | (.item // empty)
        | select(.type? == "mcp_tool_call" and .result? != null)] | length' \
        "$1" 2>>"$DIAG_LOG")
    completed=$(jq -s '[.[]
        | tostring
        | select(test("\"item\\.completed\""))
        | select(test("mcp_tool_call"))] | length' "$1" 2>>"$DIAG_LOG")
    # A shape drift that hides results must not make this check vacuous.
    [ "${measured:-0}" -ge "${completed:-1}" ] \
        || { note "$2" "codex reply shape drift"; return 1; }
}

# Restores the committed fixture state of one worktree between attempts.
reset_fixture() {
    /usr/bin/git -C "$1" checkout -q -- acceptance-fixture \
        || fail E_FIXTURE_RESET "git checkout failed in $1"
}

# Expected fixture states shared by verification and retry resets.
FIXED_PY='def value() -> int:
    return 0
'
NATIVE_PY='# native acceptance marker
def value() -> int:
    return 0
'
printf '%s' "$FIXED_PY" >"$DIAG_DIR/expected-l1.py"
printf '%s' "$NATIVE_PY" >"$DIAG_DIR/expected-l2.py"

# Restores the fixed Python fixture after an edit-loop retry in either worktree.
reset_fixed_python() {
    cat "$DIAG_DIR/expected-l1.py" >"$1/acceptance-fixture/fixture.py" \
        || fail E_FIXTURE_RESET "could not restore post-L1 state"
}

# Restores the post-L2 left fixture state (native marker above the fix).
reset_left_native() {
    cat "$DIAG_DIR/expected-l2.py" >"$1/acceptance-fixture/fixture.py" \
        || fail E_FIXTURE_RESET "could not restore post-L2 state"
}

# Verifies the L1 edit/diagnostic/fix/diff/stop loop over the real Pyright path.
verify_l1() {
    t=$DIAG_DIR/transcript-l1.jsonl
    require_codex_tool_use "$t" ide_start A_L1_START || return 1
    require_codex_tool_use "$t" ide_context A_L1_CONTEXT || return 1
    require_codex_tool_use "$t" ide_edit A_L1_EDIT || return 1
    require_codex_tool_use "$t" ide_stop A_L1_STOP || return 1
    require_text "$t" "LEFT_LOOP_OK" A_L1_FINAL || return 1
    require_text "$t" "not assignable" A_L1_PYRIGHT_SEMANTIC || return 1
    edit_input=$(codex_first_tool_input "$t" ide_edit)
    printf '%s' "$edit_input" | jq -e '.source_ref | type == "string" and length >= 1' >/dev/null \
        || { note A_L1_EDIT_INPUT "missing ide_edit source_ref"; return 1; }
    cmp -s "$LEFT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l1.py" \
        || return 1
}

# Verifies the L1 diff session: a fresh daemon generation composes and
# delivers the accumulated diff without holding the edit loop's leases.
verify_l1b() {
    t=$DIAG_DIR/transcript-l1b.jsonl
    require_codex_tool_use "$t" ide_start A_L1B_START || return 1
    require_codex_tool_use "$t" ide_diff A_L1B_DIFF || return 1
    require_codex_tool_use "$t" ide_stop A_L1B_STOP || return 1
    require_text "$t" "LEFT_DIFF_OK" A_L1B_FINAL || return 1
    require_text "$t" "left-python-bad" A_L1B_DIFF_CONTENT || return 1
}

# Verifies the L2 native fallback and the zero-write stale refusal.
#
# A byte-exact compare against a fixed expectation is too strict: a native tool (for
# example BSD `sed -i '' '1i\...'`) may insert the marker line with different trailing
# whitespace than an idealized rendering while still writing zero IDE bytes. Three
# content checks prove the same fact without pinning the native tool's exact formatting.
verify_l2() {
    t=$DIAG_DIR/transcript-l2.jsonl
    require_codex_native_edit "$t" A_L2_NATIVE_EDIT || return 1
    require_text "$t" "LEFT_FALLBACK_OK" A_L2_FINAL || return 1
    require_text "$t" "stale_source" A_L2_STALE_OUTCOME || return 1
    left_py=$LEFT/acceptance-fixture/fixture.py
    [ "$(sed -n '1p' "$left_py")" = "# native acceptance marker" ] || return 1
    grep -qF "return 0" "$left_py" || return 1
    grep -qF "return 7" "$left_py" && return 1
    cp -- "$left_py" "$DIAG_DIR/left-after-l2.py" || return 1
}

# Verifies the L3 real TypeScript semantic context.
verify_l3() {
    t=$DIAG_DIR/transcript-l3.jsonl
    require_codex_tool_use "$t" ide_context A_L3_CONTEXT || return 1
    require_text "$t" "LEFT_TS_OK" A_L3_FINAL || return 1
    # TypeScript r3 is accepted on real semantic symbol context: typescript-language-server
    # 6.0.0 publishes no diagnostics, so "not assignable" is unattainable (T38B).
    require_text "$t" "mode: semantic" A_L3_SEMANTIC || return 1
}

# Verifies the L5 symbol-tools loop: outline, symbol card, insert, read, delete over the
# Rust fixture crate, and that the inserted method leaves no trace and the crate still compiles.
verify_l5() {
    t=$DIAG_DIR/transcript-l5.jsonl
    require_codex_tool_use "$t" ide_start A_L5_START || return 1
    require_codex_tool_use "$t" ide_outline A_L5_OUTLINE || return 1
    require_codex_tool_use "$t" ide_symbol A_L5_SYMBOL || return 1
    require_codex_tool_use "$t" ide_edit A_L5_EDIT || return 1
    require_codex_tool_use "$t" ide_read A_L5_READ || return 1
    require_codex_tool_use "$t" ide_stop A_L5_STOP || return 1
    require_text "$t" "LEFT_SYMBOLS_OK" A_L5_FINAL || return 1
    require_text "$t" "impl Counter" A_L5_OUTLINE_IMPL || return 1
    require_text "$t" "pub fn get" A_L5_OUTLINE_GET || return 1
    require_text "$t" "symbol: get — method" A_L5_SYMBOL_HEADING || return 1
    require_text "$t" "acceptance-fixture/tests/counter.rs" A_L5_SYMBOL_USAGE || return 1
    require_text "$t" "edit: inserted" A_L5_EDIT_INSERTED || return 1
    require_text "$t" "diagnostics:" A_L5_EDIT_DIAGNOSTICS || return 1
    require_text "$t" "pub fn doubled" A_L5_READ_DOUBLED || return 1
    verify_symbol_tools_left_clean "$LEFT" A_L5_LEFT_CLEAN || return 1
}

# Restores the Rust fixture crate's library file to its committed baseline between L5 retries,
# without disturbing the Python/TypeScript fixture state that earlier scenarios advanced.
reset_left_rust_fixture() {
    /usr/bin/git -C "$1" checkout -q -- acceptance-fixture/src/lib.rs \
        || fail E_FIXTURE_RESET "could not restore acceptance-fixture/src/lib.rs"
}

# Scenario L1: real edit/diagnostic/fix/diff/stop loop over Pyright.
if selected l1; then
    cp -- "$DRIVER_DIR/$PROMPT_FAMILY/l1.txt" "$DIAG_DIR/prompt-l1.txt"
    run_scenario l1 "$LEFT" "$DIAG_DIR/prompt-l1.txt" verify_l1 reset_fixture A_L1_SCENARIO
fi

# Scenario L1B: the composed diff of the finished loop in a fresh session.
if selected l1b; then
    cp -- "$DRIVER_DIR/$PROMPT_FAMILY/l1b.txt" "$DIAG_DIR/prompt-l1b.txt"
    run_scenario l1b "$LEFT" "$DIAG_DIR/prompt-l1b.txt" verify_l1b reset_fixed_python A_L1B_SCENARIO
fi

# Scenario L2: native fallback while inactive, then a stale edit with zero
# writes. Retries restart from the post-L1 fixed state.
if selected l2; then
    reset_fixed_python "$LEFT"
    cp -- "$DRIVER_DIR/$PROMPT_FAMILY/l2.txt" "$DIAG_DIR/prompt-l2.txt"
    run_scenario l2 "$LEFT" "$DIAG_DIR/prompt-l2.txt" verify_l2 reset_fixed_python A_L2_SCENARIO
fi

# Scenario L3: real TypeScript semantic context through the accepted bundle.
if selected l3; then
    reset_left_native "$LEFT"
    cp -- "$DRIVER_DIR/$PROMPT_FAMILY/l3.txt" "$DIAG_DIR/prompt-l3.txt"
    run_scenario l3 "$LEFT" "$DIAG_DIR/prompt-l3.txt" verify_l3 reset_left_native A_L3_SCENARIO
fi

# Scenario L5: symbol-addressed outline/symbol/edit/read loop over the Rust fixture crate.
if selected l5; then
    cp -- "$DRIVER_DIR/$PROMPT_FAMILY/l5.txt" "$DIAG_DIR/prompt-l5.txt"
    run_scenario l5 "$LEFT" "$DIAG_DIR/prompt-l5.txt" verify_l5 reset_left_rust_fixture A_L5_SCENARIO
fi

# Restart-safe telemetry: durable events survive the daemon restart of a fresh
# session and both query and export read them back through the CLI.
if selected l4; then
TELEMETRY_DB=$OPERATOR_HOME/.agent-ide/telemetry/$LEFT_IDENTITY/state.sqlite
[ -f "$TELEMETRY_DB" ] || fail A_TELEMETRY_DB_MISSING "no durable database before restart"
before_rows=$("$BINARY" telemetry export --database "$TELEMETRY_DB" 2>>"$DIAG_LOG" | wc -l | tr -d ' ')
[ "${before_rows:-0}" -ge 1 ] || fail A_TELEMETRY_PRE_RESTART_EMPTY "export before restart"

# Scenario L4: one minimal session whose daemon generation must append events.
cp -- "$DRIVER_DIR/$PROMPT_FAMILY/l4.txt" "$DIAG_DIR/prompt-l4.txt"
l4_attempt=1
while [ "$l4_attempt" -le "$MAX_ATTEMPTS" ]; do
    note "scenario-l4-attempt" "$l4_attempt"
    if run_session l4 "$LEFT" "$DIAG_DIR/prompt-l4.txt" \
        && require_text "$DIAG_DIR/transcript-l4.jsonl" "LEFT_RESTART_OK" A_L4_FINAL; then
        break
    fi
    l4_attempt=$((l4_attempt + 1))
done
[ "$l4_attempt" -le "$MAX_ATTEMPTS" ] || fail A_L4_SCENARIO "restart session never completed"

after_rows=$("$BINARY" telemetry export --database "$TELEMETRY_DB" 2>>"$DIAG_LOG" | wc -l | tr -d ' ')
[ "$after_rows" -gt "$before_rows" ] || fail A_TELEMETRY_RESTART_LOST "no new rows after restart"
"$BINARY" telemetry query --database "$TELEMETRY_DB" >"$DIAG_DIR/telemetry-query.json" 2>>"$DIAG_LOG" \
    || fail A_TELEMETRY_QUERY_EXIT "query exited nonzero"
jq -e '.rows | length >= 1' "$DIAG_DIR/telemetry-query.json" >/dev/null 2>&1 \
    || fail A_TELEMETRY_QUERY_ROWS "query returned no events"
fi

# Verifies the R5 divergent-worktree loop.
verify_r5() {
    t=$DIAG_DIR/transcript-r5.jsonl
    require_codex_tool_use "$t" ide_context A_R5_CONTEXT || return 1
    require_codex_tool_use "$t" ide_edit A_R5_EDIT || return 1
    require_text "$t" "right-python-bad" A_R5_PYRIGHT_MARKER || return 1
    require_text "$t" "right-typescript-bad" A_R5_TYPESCRIPT_MARKER || return 1
    forbid_text "$t" "left-python-bad" A_R5_LEFT_LEAK || return 1
    forbid_text "$t" "left-typescript-bad" A_R5_LEFT_LEAK_TS || return 1
    require_text "$t" "RIGHT_LOOP_OK" A_R5_FINAL || return 1
    # A mismatch keeps the actual file beside the diagnostics so the failing side is known.
    if ! cmp -s "$RIGHT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l1.py"; then
        cp -- "$RIGHT/acceptance-fixture/fixture.py" "$DIAG_DIR/r5-right-actual.py" 2>/dev/null || :
        note A_R5_RIGHT_CONTENT "right fixture.py differs from expected-l1.py (kept as r5-right-actual.py)"
        return 1
    fi
    # Left isolation: compare against the snapshot taken right after the L2 checks,
    # not the idealized expected-l2.py (see verify_l2's native-tool formatting note).
    if ! cmp -s "$LEFT/acceptance-fixture/fixture.py" "$DIAG_DIR/left-after-l2.py"; then
        cp -- "$LEFT/acceptance-fixture/fixture.py" "$DIAG_DIR/r5-left-actual.py" 2>/dev/null || :
        note A_R5_LEFT_CONTENT "left fixture.py changed since L2 (kept as r5-left-actual.py)"
        return 1
    fi
}

# Scenario R5: the complete loop in the divergent right worktree.
if selected r5; then
    cp -- "$DRIVER_DIR/$PROMPT_FAMILY/r5.txt" "$DIAG_DIR/prompt-r5.txt"
    run_scenario r5 "$RIGHT" "$DIAG_DIR/prompt-r5.txt" verify_r5 reset_fixture A_R5_SCENARIO
fi

# Verifies the R5 diff session in the divergent right worktree.
verify_r5b() {
    t=$DIAG_DIR/transcript-r5b.jsonl
    require_codex_tool_use "$t" ide_start A_R5B_START || return 1
    require_codex_tool_use "$t" ide_diff A_R5B_DIFF || return 1
    require_codex_tool_use "$t" ide_stop A_R5B_STOP || return 1
    require_text "$t" "RIGHT_DIFF_OK" A_R5B_FINAL || return 1
    require_text "$t" "right-python-bad" A_R5B_DIFF_CONTENT || return 1
    forbid_text "$t" "left-python-bad" A_R5B_LEFT_LEAK || return 1
}

# Scenario R5B: the composed diff of the right loop in a fresh session.
if selected r5b; then
    cp -- "$DRIVER_DIR/$PROMPT_FAMILY/r5b.txt" "$DIAG_DIR/prompt-r5b.txt"
    run_scenario r5b "$RIGHT" "$DIAG_DIR/prompt-r5b.txt" verify_r5b reset_fixed_python A_R5B_SCENARIO
fi

# Compact projection holds across every captured session.
for transcript in "$DIAG_DIR"/transcript-l*.jsonl "$DIAG_DIR"/transcript-r5*.jsonl; do
    [ -f "$transcript" ] || continue
    require_codex_compact_replies "$transcript" A_COMPACT_REPLIES || fail A_COMPACT_REPLIES "$transcript"
done

# A filtered run is a diagnostic rerun and never emits the closed result
# document; only the complete cell may claim every scenario real_pass.
if [ -z "$ONLY" ]; then
    write_pass_result
else
    note driver-filtered-run "$ONLY"
fi
note driver-complete codex
exit 0
