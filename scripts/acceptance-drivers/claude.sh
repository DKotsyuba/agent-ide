#!/bin/sh
# Status: refreshed for the current hosts; first passing run pending; see
# docs/macos-acceptance.md results table.

# Real-host driver for the direct Claude Code acceptance route.
#
# The runner starts this executable with AGENT_IDE_ACCEPTANCE_ROUTE=claude, the
# two isolated fixture worktrees, and a fresh result path. The driver runs real
# `claude -p` sessions with the operator's authenticated home against the
# candidate binary's managed MCP and plugin hooks, verifies every scenario from
# the captured stream-json transcripts plus real filesystem and telemetry
# effects, and only then emits the closed nine-line real_pass document. Any
# failing step records a closed code in the private diagnostic log and fails the
# whole cell honestly.
#
# Turn bound: Claude Code 2.1.274 documents no flag that bounds agentic turns
# (`--max-turns` and similar are gone from `claude --help`), so each session is
# bounded only by the outer `perl alarm` wall clock below.
#
# Each scenario session is attempted up to three times because an economical
# model occasionally drops a scripted helper step; every retry first restores
# the exact fixture precondition, so an attempt always starts from the same
# worktree state and a later attempt can never inherit a partial effect.
#
# Setting AGENT_IDE_ACCEPTANCE_DRY=1 prints the exact session command lines and
# exits 0 without running anything.
#
# Operator environment (all optional, defaults fit the release host):
#   AGENT_IDE_ACCEPTANCE_BINARY       candidate agent-ide executable;
#                                     default <repo>/target/release/agent-ide.
#   AGENT_IDE_ACCEPTANCE_CLAUDE       real Claude Code CLI; default
#                                     /Users/pluto/.local/bin/claude.
#   AGENT_IDE_ACCEPTANCE_MODEL        economical model alias; default haiku.
#   AGENT_IDE_ACCEPTANCE_LAUNCHER     strict launcher template carrying both the
#                                     Pyright and accepted TypeScript Claude
#                                     records (claude-r3-2026-09-14); default
#                                     /Users/pluto/.config/agent-ide/launcher.json.
#   AGENT_IDE_ACCEPTANCE_OPERATOR_HOME operator home with the existing Claude
#                                     login; default /Users/pluto. Credentials
#                                     are never copied, read, or printed.
#   AGENT_IDE_ACCEPTANCE_SESSION_SECONDS wall-clock bound per session; default
#                                     900.

set -eu

DRIVER_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$DRIVER_DIR/driver-common.sh"

# Maximum model-session attempts per scenario before the driver fails.
MAX_ATTEMPTS=3

[ "${AGENT_IDE_ACCEPTANCE_ROUTE:-}" = claude ] || fail E_ROUTE "route is not claude"
[ -n "${AGENT_IDE_ACCEPTANCE_LEFT_WORKTREE:-}" ] || fail E_LEFT_WORKTREE_MISSING
[ -n "${AGENT_IDE_ACCEPTANCE_RIGHT_WORKTREE:-}" ] || fail E_RIGHT_WORKTREE_MISSING
[ -n "${AGENT_IDE_ACCEPTANCE_RESULT:-}" ] || fail E_RESULT_PATH_MISSING

BINARY=${AGENT_IDE_ACCEPTANCE_BINARY:-$DRIVER_ROOT/target/release/agent-ide}
CLAUDE=${AGENT_IDE_ACCEPTANCE_CLAUDE:-/Users/pluto/.local/bin/claude}
MODEL=${AGENT_IDE_ACCEPTANCE_MODEL:-haiku}
LAUNCHER=${AGENT_IDE_ACCEPTANCE_LAUNCHER:-/Users/pluto/.config/agent-ide/launcher.json}
OPERATOR_HOME=${AGENT_IDE_ACCEPTANCE_OPERATOR_HOME:-/Users/pluto}
SESSION_SECONDS=${AGENT_IDE_ACCEPTANCE_SESSION_SECONDS:-900}

# Dry run: print the exact session command lines and exit without running
# anything. Contract variables only need to be set; no host is touched.
if [ "${AGENT_IDE_ACCEPTANCE_DRY:-0}" = 1 ]; then
    for scenario in l1:LEFT l1b:LEFT l2:LEFT l3:LEFT l4:LEFT r5:RIGHT r5b:RIGHT; do
        label=${scenario%%:*}
        eval "worktree=\$AGENT_IDE_ACCEPTANCE_${scenario##*:}_WORKTREE"
        printf '%s\n' "cd $worktree && HOME=$OPERATOR_HOME AGENT_IDE_BIN=$BINARY \
CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=0 PATH=/usr/bin:/bin:/usr/sbin:/sbin \
/usr/bin/env -u ANTHROPIC_AUTH_TOKEN -u ANTHROPIC_BASE_URL -u ANTHROPIC_API_KEY \
-u ANTHROPIC_MODEL -u ANTHROPIC_SMALL_FAST_MODEL -u CLAUDECODE \
-u CLAUDE_CODE_CHILD_SESSION -u CLAUDE_CODE_ENTRYPOINT -u CLAUDE_CODE_EXECPATH \
-u CLAUDE_CODE_MESSAGING_SOCKET -u CLAUDE_CODE_MESSAGING_TOKEN \
-u CLAUDE_CODE_SESSION_ID -u CLAUDE_PID -u CLAUDE_EFFORT \
/usr/bin/perl -e 'alarm shift; exec @ARGV' $SESSION_SECONDS \
$CLAUDE -p \"\$(cat $DIAG_DIR/prompt-$label.txt)\" --model $MODEL \
--output-format stream-json --verbose --dangerously-skip-permissions \
--strict-mcp-config --mcp-config $DIAG_DIR/claude-mcp.json \
--plugin-dir $worktree > $DIAG_DIR/transcript-$label.jsonl \
2> $DIAG_DIR/session-$label.err"
    done
    exit 0
fi

[ -x "$BINARY" ] || fail E_BINARY_MISSING "$BINARY"
[ -x "$CLAUDE" ] || fail E_CLAUDE_MISSING "$CLAUDE"
[ -r "$LAUNCHER" ] || fail E_LAUNCHER_MISSING "$LAUNCHER"
[ -d "$OPERATOR_HOME" ] || fail E_OPERATOR_HOME_MISSING "$OPERATOR_HOME"

LEFT=$(canonical_dir "$AGENT_IDE_ACCEPTANCE_LEFT_WORKTREE") || fail E_LEFT_CANONICAL
RIGHT=$(canonical_dir "$AGENT_IDE_ACCEPTANCE_RIGHT_WORKTREE") || fail E_RIGHT_CANONICAL
LEFT_IDENTITY=$(project_identity "$BINARY" "$LEFT") || fail E_LEFT_IDENTITY
[ "${#LEFT_IDENTITY}" = 64 ] || fail E_LEFT_IDENTITY_LENGTH
LEFT_RUNTIME=/private/tmp/ai-c-$(printf '%s' "$LEFT_IDENTITY" | cut -c1-16)

mkdir -p -- "$DIAG_DIR"

# Writes the MCP server configuration once; every session uses the candidate
# binary with the strict Claude launcher template.
MCP_CONFIG=$DIAG_DIR/claude-mcp.json
jq -n --arg command "$BINARY" --arg launcher "$LAUNCHER" \
    '{mcpServers:{"agent-ide":{type:"stdio",command:$command,args:["mcp","--claude-launcher-template",$launcher]}}}' \
    >"$MCP_CONFIG" || fail E_MCP_CONFIG

# Prepares one worktree for sessions: project-local sandbox settings allowing
# exactly this project's helper socket.
# The first argument is the canonical worktree and the second its runtime root.
prepare_worktree() {
    socket=$2/claude-helper.sock
    mkdir -p -- "$1/.claude" || fail E_SETTINGS_DIR "$1"
    jq -n --arg socket "$socket" \
        '{sandbox:{network:{allowUnixSockets:[$socket]}}}' \
        >"$1/.claude/settings.local.json" || fail E_SETTINGS_WRITE "$1"
}

# Restores the committed fixture state of one worktree between attempts.
# The single argument is the canonical worktree path.
reset_fixture() {
    /usr/bin/git -C "$1" checkout -q -- acceptance-fixture \
        || fail E_FIXTURE_RESET "git checkout failed in $1"
}

# Runs one wall-clock-bounded `claude -p` session inside a worktree.
#
# The first argument is a short session label, the second the canonical
# worktree, and the third the prompt file. The session runs inside that worktree
# and inherits the operator's real home for the existing OAuth login plus
# AGENT_IDE_BIN so the plugin hooks exec the candidate binary. Every inherited
# authentication and nested-session variable of the invoking agent is removed,
# so the route really exercises the operator's Claude login instead of a wrapper
# backend. PATH is reduced to system directories: an ambient `agent-ide` on PATH
# would let a second, stale host hook register a duplicate pre-observation for
# the same tool call, which the daemon correctly rejects as ambiguous, so only
# the candidate plugin hook may deliver binding evidence. The complete
# stream-json transcript stays in the private diagnostic directory.
run_session() {
    label=$1
    worktree=$2
    prompt_file=$3
    transcript=$DIAG_DIR/transcript-$label.jsonl
    rm -f -- "$transcript"
    if (cd "$worktree" && HOME="$OPERATOR_HOME" AGENT_IDE_BIN="$BINARY" \
        CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=0 PATH=/usr/bin:/bin:/usr/sbin:/sbin \
        /usr/bin/env -u ANTHROPIC_AUTH_TOKEN -u ANTHROPIC_BASE_URL \
        -u ANTHROPIC_API_KEY -u ANTHROPIC_MODEL -u ANTHROPIC_SMALL_FAST_MODEL \
        -u CLAUDECODE -u CLAUDE_CODE_CHILD_SESSION -u CLAUDE_CODE_ENTRYPOINT \
        -u CLAUDE_CODE_EXECPATH -u CLAUDE_CODE_MESSAGING_SOCKET \
        -u CLAUDE_CODE_MESSAGING_TOKEN -u CLAUDE_CODE_SESSION_ID -u CLAUDE_PID \
        -u CLAUDE_EFFORT \
        /usr/bin/perl -e 'alarm shift; exec @ARGV' "$SESSION_SECONDS" \
        "$CLAUDE" -p "$(cat "$prompt_file")" \
        --model "$MODEL" --output-format stream-json --verbose \
        --dangerously-skip-permissions \
        --strict-mcp-config --mcp-config "$MCP_CONFIG" --plugin-dir "$worktree" \
        >"$transcript" 2>"$DIAG_DIR/session-$label.err")
    then
        note "session-$label-complete"
    else
        status=$?
        note "session-$label-status" "$status"
        return 1
    fi
    [ -s "$transcript" ] || return 1
    return 0
}

# Repeats one scenario session until its verify function accepts or attempts
# run out. The arguments are the label, worktree, prompt file, verify function
# name, reset function name, and the closed failure code.
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

HELPER_MARKER="claude-worker --runtime-dir '$LEFT_RUNTIME"

prepare_worktree "$LEFT" "$LEFT_RUNTIME"

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

# Restores the post-L1 left fixture state (Python file fixed by the helper).
reset_left_fixed() {
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
    require_tool_use "$t" mcp__agent-ide__ide_start A_L1_START || return 1
    require_tool_use "$t" mcp__agent-ide__ide_context A_L1_CONTEXT || return 1
    require_tool_use "$t" mcp__agent-ide__ide_edit A_L1_EDIT || return 1
    require_tool_use "$t" mcp__agent-ide__ide_stop A_L1_STOP || return 1
    require_helper_immediately_after "$t" mcp__agent-ide__ide_start A_L1_HELPER_ORDER_START || return 1
    require_helper_immediately_after "$t" mcp__agent-ide__ide_context A_L1_HELPER_ORDER_CONTEXT || return 1
    require_helper_immediately_after "$t" mcp__agent-ide__ide_edit A_L1_HELPER_ORDER_EDIT || return 1
    require_transcript_text "$t" "$HELPER_MARKER" A_L1_HELPER_COMMAND || return 1
    require_transcript_text "$t" "LEFT_LOOP_OK" A_L1_FINAL || return 1
    require_transcript_text "$t" "not assignable" A_L1_PYRIGHT_SEMANTIC || return 1
    edit_input=$(first_tool_input "$t" mcp__agent-ide__ide_edit)
    printf '%s' "$edit_input" | jq -e '.source_ref | type == "string" and length >= 1' >/dev/null \
        || return 1
    cmp -s "$LEFT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l1.py" \
        || return 1
}

# Verifies the L1 diff session: a fresh daemon generation composes and delivers
# the accumulated diff without holding the edit loop's admission leases.
verify_l1b() {
    t=$DIAG_DIR/transcript-l1b.jsonl
    require_tool_use "$t" mcp__agent-ide__ide_start A_L1B_START || return 1
    require_tool_use "$t" mcp__agent-ide__ide_diff A_L1B_DIFF || return 1
    require_tool_use "$t" mcp__agent-ide__ide_stop A_L1B_STOP || return 1
    require_helper_immediately_after "$t" mcp__agent-ide__ide_diff A_L1B_HELPER_ORDER_DIFF || return 1
    require_transcript_text "$t" "LEFT_DIFF_OK" A_L1B_FINAL || return 1
    require_transcript_text "$t" "left-python-bad" A_L1B_DIFF_CONTENT || return 1
}

# Verifies the L2 native fallback and the zero-write stale refusal.
verify_l2() {
    t=$DIAG_DIR/transcript-l2.jsonl
    require_tool_use "$t" Edit A_L2_NATIVE_EDIT || return 1
    require_transcript_text "$t" "LEFT_FALLBACK_OK" A_L2_FINAL || return 1
    require_transcript_text "$t" "stale_source" A_L2_STALE_OUTCOME || return 1
    cmp -s "$LEFT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l2.py" \
        || return 1
}

# Verifies the L3 real TypeScript semantic context.
verify_l3() {
    t=$DIAG_DIR/transcript-l3.jsonl
    require_tool_use "$t" mcp__agent-ide__ide_context A_L3_CONTEXT || return 1
    require_helper_immediately_after "$t" mcp__agent-ide__ide_context A_L3_HELPER_ORDER || return 1
    require_transcript_text "$t" "LEFT_TS_OK" A_L3_FINAL || return 1
    require_transcript_text "$t" "not assignable" A_L3_SEMANTIC || return 1
}

# Verifies the R5 divergent-worktree loop.
verify_r5() {
    t=$DIAG_DIR/transcript-r5.jsonl
    require_tool_use "$t" mcp__agent-ide__ide_edit A_R5_EDIT || return 1
    require_helper_immediately_after "$t" mcp__agent-ide__ide_edit A_R5_HELPER_ORDER_EDIT || return 1
    require_transcript_text "$t" "right-python-bad" A_R5_PYRIGHT_MARKER || return 1
    require_transcript_text "$t" "right-typescript-bad" A_R5_TYPESCRIPT_MARKER || return 1
    forbid_transcript_text "$t" "left-python-bad" A_R5_LEFT_LEAK || return 1
    forbid_transcript_text "$t" "left-typescript-bad" A_R5_LEFT_LEAK_TS || return 1
    require_transcript_text "$t" "RIGHT_LOOP_OK" A_R5_FINAL || return 1
    cmp -s "$RIGHT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l1.py" \
        || return 1
    cmp -s "$LEFT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l2.py" \
        || return 1
}

# Scenario L1: real edit/diagnostic/fix/diff/stop loop over Pyright.
cp -- "$DRIVER_DIR/claude-prompts/l1.txt" "$DIAG_DIR/prompt-l1.txt"
run_scenario l1 "$LEFT" "$DIAG_DIR/prompt-l1.txt" verify_l1 reset_fixture A_L1_SCENARIO

# Scenario L1B: the composed diff of the finished loop in a fresh session.
cp -- "$DRIVER_DIR/claude-prompts/l1b.txt" "$DIAG_DIR/prompt-l1b.txt"
run_scenario l1b "$LEFT" "$DIAG_DIR/prompt-l1b.txt" verify_l1b reset_left_fixed A_L1B_SCENARIO

# Scenario L2: native fallback while inactive, then a stale edit with zero
# writes. Retries restart from the post-L1 fixed state.
reset_left_fixed "$LEFT"
cp -- "$DRIVER_DIR/claude-prompts/l2.txt" "$DIAG_DIR/prompt-l2.txt"
run_scenario l2 "$LEFT" "$DIAG_DIR/prompt-l2.txt" verify_l2 reset_left_fixed A_L2_SCENARIO

# Scenario L3: real TypeScript semantic context through the accepted bundle.
reset_left_native "$LEFT"
cp -- "$DRIVER_DIR/claude-prompts/l3.txt" "$DIAG_DIR/prompt-l3.txt"
run_scenario l3 "$LEFT" "$DIAG_DIR/prompt-l3.txt" verify_l3 reset_left_native A_L3_SCENARIO

# Restart-safe telemetry: durable events survive the daemon restart of a fresh
# session and both query and export read them back through the CLI.
TELEMETRY_DB=$OPERATOR_HOME/.agent-ide/telemetry/$LEFT_IDENTITY/state.sqlite
[ -f "$TELEMETRY_DB" ] || fail A_TELEMETRY_DB_MISSING "no durable database before restart"
before_rows=$("$BINARY" telemetry export --database "$TELEMETRY_DB" 2>>"$DIAG_LOG" | wc -l | tr -d ' ')
[ "${before_rows:-0}" -ge 1 ] || fail A_TELEMETRY_PRE_RESTART_EMPTY "export before restart"

# Scenario L4: one minimal session whose daemon generation must append events.
cp -- "$DRIVER_DIR/claude-prompts/l4.txt" "$DIAG_DIR/prompt-l4.txt"
l4_attempt=1
while [ "$l4_attempt" -le "$MAX_ATTEMPTS" ]; do
    note "scenario-l4-attempt" "$l4_attempt"
    if run_session l4 "$LEFT" "$DIAG_DIR/prompt-l4.txt" \
        && require_transcript_text "$DIAG_DIR/transcript-l4.jsonl" "LEFT_RESTART_OK" A_L4_FINAL; then
        break
    fi
    l4_attempt=$((l4_attempt + 1))
done
[ "$l4_attempt" -le "$MAX_ATTEMPTS" ] || fail A_L4_SCENARIO "restart session never completed"

after_rows=$("$BINARY" telemetry export --database "$TELEMETRY_DB" 2>>"$DIAG_LOG" | wc -l | tr -d ' ')
[ "$after_rows" -gt "$before_rows" ] || fail A_TELEMETRY_RESTART_LOST "no new rows after restart"
"$BINARY" telemetry query --database "$TELEMETRY_DB" >"$DIAG_DIR/telemetry-query.json" 2>>"$DIAG_LOG" \
    || fail A_TELEMETRY_QUERY_EXIT "query exited nonzero"
jq -e '.events | length >= 1' "$DIAG_DIR/telemetry-query.json" >/dev/null 2>&1 \
    || fail A_TELEMETRY_QUERY_ROWS "query returned no events"

# Scenario R5: the complete loop in the divergent right worktree.
RIGHT_IDENTITY=$(project_identity "$BINARY" "$RIGHT") || fail E_RIGHT_IDENTITY
RIGHT_RUNTIME=/private/tmp/ai-c-$(printf '%s' "$RIGHT_IDENTITY" | cut -c1-16)
prepare_worktree "$RIGHT" "$RIGHT_RUNTIME"
cp -- "$DRIVER_DIR/claude-prompts/r5.txt" "$DIAG_DIR/prompt-r5.txt"
run_scenario r5 "$RIGHT" "$DIAG_DIR/prompt-r5.txt" verify_r5 reset_fixture A_R5_SCENARIO

# Verifies the R5 diff session in the divergent right worktree.
verify_r5b() {
    t=$DIAG_DIR/transcript-r5b.jsonl
    require_tool_use "$t" mcp__agent-ide__ide_start A_R5B_START || return 1
    require_tool_use "$t" mcp__agent-ide__ide_diff A_R5B_DIFF || return 1
    require_tool_use "$t" mcp__agent-ide__ide_stop A_R5B_STOP || return 1
    require_helper_immediately_after "$t" mcp__agent-ide__ide_diff A_R5B_HELPER_ORDER_DIFF || return 1
    require_transcript_text "$t" "RIGHT_DIFF_OK" A_R5B_FINAL || return 1
    require_transcript_text "$t" "right-python-bad" A_R5B_DIFF_CONTENT || return 1
    forbid_transcript_text "$t" "left-python-bad" A_R5B_LEFT_LEAK || return 1
}

# Scenario R5B: the composed diff of the right loop in a fresh session.
cp -- "$DRIVER_DIR/claude-prompts/r5b.txt" "$DIAG_DIR/prompt-r5b.txt"
run_scenario r5b "$RIGHT" "$DIAG_DIR/prompt-r5b.txt" verify_r5b reset_fixture A_R5B_SCENARIO

# Compact projection holds across every captured session.
for transcript in "$DIAG_DIR"/transcript-l*.jsonl "$DIAG_DIR"/transcript-r5*.jsonl; do
    require_compact_replies "$transcript" A_COMPACT_REPLIES
done

write_pass_result
note driver-complete claude
exit 0
