#!/bin/sh
# WORK IN PROGRESS: this driver is unfinished and committed as-is to carry the
# work forward. The direct Claude route is partially debugged and has not yet
# produced a real_pass host-cell result, so nothing may wire it into release
# acceptance yet.

# Real-host driver for the installed agent-run to Claude acceptance route.
#
# The runner starts this executable with AGENT_IDE_ACCEPTANCE_ROUTE=
# agent-run-claude, the two isolated fixture worktrees, and a fresh result
# path. The driver starts real agents through the installed agent-run CLI on
# its glm runtime (Claude Code with a GLM backend), which attaches the
# externally installed agent-ide MCP and plugin from the operator's agent-run
# configuration. Scenario verification uses the agents' stored answers, real
# filesystem effects, and the durable telemetry database; the driver fails
# closed before starting any agent when the installed MCP binary or launcher
# template does not carry the candidate v0.2 surface.
#
# Operator environment (all optional, defaults fit the release host):
#   AGENT_IDE_ACCEPTANCE_BINARY         candidate agent-ide executable used for
#                                       identity, telemetry, and evidence
#                                       helpers; default
#                                       <repo>/target/release/agent-ide.
#   AGENT_IDE_ACCEPTANCE_AGENT_RUN      installed agent-run CLI; default
#                                       /Users/pluto/.local/bin/agent-run.
#   AGENT_IDE_ACCEPTANCE_AGENT_RUN_MCP_BINARY binary the agent-run mcp.agent_ide
#                                       table executes; default
#                                       /Users/pluto/.local/bin/agent-ide. It
#                                       must be the release candidate; the
#                                       driver refuses a binary that lacks the
#                                       v0.2 Claude surface.
#   AGENT_IDE_ACCEPTANCE_AGENT_RUN_RUNTIME  agent-run runtime name; default glm.
#   AGENT_IDE_ACCEPTANCE_AGENT_RUN_MODEL    economical model id; default
#                                       glm-5.3-flash.
#   AGENT_IDE_ACCEPTANCE_AGENT_RUN_PROFILE  agent-run profile; default
#                                       implement.
#   AGENT_IDE_ACCEPTANCE_LAUNCHER       launcher template the installed MCP
#                                       loads; default
#                                       /Users/pluto/.config/agent-ide/launcher.json.
#   AGENT_IDE_ACCEPTANCE_OPERATOR_HOME  operator home; default /Users/pluto.
#   AGENT_IDE_ACCEPTANCE_SESSION_SECONDS per-agent wall-clock bound; default
#                                       1800.

set -eu

DRIVER_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$DRIVER_DIR/driver-common.sh"

[ "${AGENT_IDE_ACCEPTANCE_ROUTE:-}" = agent-run-claude ] || fail E_ROUTE "route is not agent-run-claude"
[ -n "${AGENT_IDE_ACCEPTANCE_LEFT_WORKTREE:-}" ] || fail E_LEFT_WORKTREE_MISSING
[ -n "${AGENT_IDE_ACCEPTANCE_RIGHT_WORKTREE:-}" ] || fail E_RIGHT_WORKTREE_MISSING
[ -n "${AGENT_IDE_ACCEPTANCE_RESULT:-}" ] || fail E_RESULT_PATH_MISSING

BINARY=${AGENT_IDE_ACCEPTANCE_BINARY:-$DRIVER_ROOT/target/release/agent-ide}
AGENT_RUN=${AGENT_IDE_ACCEPTANCE_AGENT_RUN:-/Users/pluto/.local/bin/agent-run}
MCP_BINARY=${AGENT_IDE_ACCEPTANCE_AGENT_RUN_MCP_BINARY:-/Users/pluto/.local/bin/agent-ide}
RUNTIME=${AGENT_IDE_ACCEPTANCE_AGENT_RUN_RUNTIME:-glm}
AR_MODEL=${AGENT_IDE_ACCEPTANCE_AGENT_RUN_MODEL:-glm-5.3-flash}
AR_PROFILE=${AGENT_IDE_ACCEPTANCE_AGENT_RUN_PROFILE:-implement}
LAUNCHER=${AGENT_IDE_ACCEPTANCE_LAUNCHER:-/Users/pluto/.config/agent-ide/launcher.json}
OPERATOR_HOME=${AGENT_IDE_ACCEPTANCE_OPERATOR_HOME:-/Users/pluto}
SESSION_SECONDS=${AGENT_IDE_ACCEPTANCE_SESSION_SECONDS:-1800}

[ -x "$BINARY" ] || fail E_BINARY_MISSING "$BINARY"
[ -x "$AGENT_RUN" ] || fail E_AGENT_RUN_MISSING "$AGENT_RUN"
[ -x "$MCP_BINARY" ] || fail E_MCP_BINARY_MISSING "$MCP_BINARY"
[ -r "$LAUNCHER" ] || fail E_LAUNCHER_MISSING "$LAUNCHER"

# The route can only claim the candidate contract when the binary agent-run
# executes actually carries the v0.2 Claude TypeScript record surface and the
# launcher template declares that accepted record.
/usr/bin/strings "$MCP_BINARY" | /usr/bin/grep -q 'claude-r3-2026-09-14' \
    || fail E_MCP_BINARY_NOT_CANDIDATE "installed MCP binary predates the v0.2 surface"
/usr/bin/grep -q 'claude-r3-2026-09-14' "$LAUNCHER" \
    || fail E_LAUNCHER_NO_CLAUDE_TYPESCRIPT "launcher template lacks the accepted Claude TypeScript record"

LEFT=$(canonical_dir "$AGENT_IDE_ACCEPTANCE_LEFT_WORKTREE") || fail E_LEFT_CANONICAL
RIGHT=$(canonical_dir "$AGENT_IDE_ACCEPTANCE_RIGHT_WORKTREE") || fail E_RIGHT_CANONICAL
LEFT_IDENTITY=$(project_identity "$BINARY" "$LEFT") || fail E_LEFT_IDENTITY
[ "${#LEFT_IDENTITY}" = 64 ] || fail E_LEFT_IDENTITY_LENGTH
LEFT_RUNTIME=/private/tmp/ai-c-$(printf '%s' "$LEFT_IDENTITY" | cut -c1-16)
RIGHT_IDENTITY=$(project_identity "$BINARY" "$RIGHT") || fail E_RIGHT_IDENTITY
RIGHT_RUNTIME=/private/tmp/ai-c-$(printf '%s' "$RIGHT_IDENTITY" | cut -c1-16)

mkdir -p -- "$DIAG_DIR"

# Prepares the project-local sandbox socket allowance for one worktree.
prepare_worktree() {
    socket=$2/claude-helper.sock
    mkdir -p -- "$1/.claude" || fail E_SETTINGS_DIR "$1"
    jq -n --arg socket "$socket" \
        '{sandbox:{network:{allowUnixSockets:[$socket]}}}' \
        >"$1/.claude/settings.local.json" || fail E_SETTINGS_WRITE "$1"
}

# Runs one bounded agent-run agent and prints its final answer text.
#
# The first argument is a short label, the second the worktree, and the third
# the task file. The label, worktree, and answer are appended to the private
# diagnostic log; agent identifiers never enter public evidence.
run_agent() {
    label=$1
    worktree=$2
    task_file=$3
    answer_file=$DIAG_DIR/answer-$label.txt
    if AGENT_IDE_BIN="$MCP_BINARY" HOME="$OPERATOR_HOME" \
        /usr/bin/perl -e 'alarm shift; exec @ARGV' "$SESSION_SECONDS" \
        "$AGENT_RUN" start --runtime "$RUNTIME" --model "$AR_MODEL" \
        --profile "$AR_PROFILE" --task "$(cat "$task_file")" \
        --workdir "$worktree" --write --wait --timeout "$SESSION_SECONDS" \
        >"$DIAG_DIR/start-$label.out" 2>"$DIAG_DIR/start-$label.err"
    then
        note "agent-$label-started"
    else
        status=$?
        fail "E_${label}_START" "status $status"
    fi
    agent_id=$(sed -n 's/.*ID: \([A-Za-z0-9_-]\{1,64\}\).*/\1/p' "$DIAG_DIR/start-$label.out" | tail -1)
    [ -n "$agent_id" ] || agent_id=$(jq -r '.agent_id // empty' "$DIAG_DIR/start-$label.out" 2>/dev/null | tail -1)
    [ -n "$agent_id" ] || fail "E_${label}_AGENT_ID" "no agent id in start output"
    "$AGENT_RUN" answer "$agent_id" >"$answer_file" 2>>"$DIAG_LOG" \
        || fail "E_${label}_ANSWER" "answer exited nonzero"
    [ -s "$answer_file" ] || fail "E_${label}_ANSWER_EMPTY" "empty answer"
    printf '%s %s\n' "$label" "$agent_id" >>"$DIAG_DIR/agent-ids.log"
}

# Requires the agent's final answer to contain one literal token.
require_answer_text() {
    /usr/bin/grep -qF -- "$2" "$DIAG_DIR/answer-$1.txt" \
        || fail "$3" "answer $1 lacks $2"
}

prepare_worktree "$LEFT" "$LEFT_RUNTIME"
prepare_worktree "$RIGHT" "$RIGHT_RUNTIME"

# Agent L1: real edit/diagnostic/fix/diff/stop loop over Pyright.
cp -- "$DRIVER_DIR/claude-prompts/l1.txt" "$DIAG_DIR/task-l1.txt"
run_agent l1 "$LEFT" "$DIAG_DIR/task-l1.txt"
require_answer_text l1 "LEFT_LOOP_OK" A_L1_FINAL
printf 'def value() -> int:\n    return 0\n' >"$DIAG_DIR/expected-l1.py"
cmp -s "$LEFT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l1.py" \
    || fail A_L1_FILE_CONTENT "left fixture.py is not the helper-edited content"

# Agent L2: native fallback while inactive, then a stale edit with zero writes.
cp -- "$DRIVER_DIR/claude-prompts/l2.txt" "$DIAG_DIR/task-l2.txt"
run_agent l2 "$LEFT" "$DIAG_DIR/task-l2.txt"
require_answer_text l2 "LEFT_FALLBACK_OK" A_L2_FINAL
require_answer_text l2 "stale" A_L2_STALE_OUTCOME
printf '# native acceptance marker\ndef value() -> int:\n    return 0\n' >"$DIAG_DIR/expected-l2.py"
cmp -s "$LEFT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l2.py" \
    || fail A_L2_ZERO_WRITE "left fixture.py does not prove the stale edit wrote nothing"

# Agent L3: real TypeScript semantic context through the accepted bundle.
cp -- "$DRIVER_DIR/claude-prompts/l3.txt" "$DIAG_DIR/task-l3.txt"
run_agent l3 "$LEFT" "$DIAG_DIR/task-l3.txt"
require_answer_text l3 "LEFT_TS_OK" A_L3_FINAL
require_answer_text l3 "not assignable" A_L3_SEMANTIC

# Restart-safe telemetry across a fresh agent's daemon generation.
TELEMETRY_DB=$OPERATOR_HOME/.agent-ide/telemetry/$LEFT_IDENTITY/state.sqlite
[ -f "$TELEMETRY_DB" ] || fail A_TELEMETRY_DB_MISSING "no durable database before restart"
before_rows=$("$BINARY" telemetry export --database "$TELEMETRY_DB" 2>>"$DIAG_LOG" | wc -l | tr -d ' ')
[ "${before_rows:-0}" -ge 1 ] || fail A_TELEMETRY_PRE_RESTART_EMPTY "export before restart"
cp -- "$DRIVER_DIR/claude-prompts/l4.txt" "$DIAG_DIR/task-l4.txt"
run_agent l4 "$LEFT" "$DIAG_DIR/task-l4.txt"
require_answer_text l4 "LEFT_RESTART_OK" A_L4_FINAL
after_rows=$("$BINARY" telemetry export --database "$TELEMETRY_DB" 2>>"$DIAG_LOG" | wc -l | tr -d ' ')
[ "$after_rows" -gt "$before_rows" ] || fail A_TELEMETRY_RESTART_LOST "no new rows after restart"

# Agent R5: the complete loop in the divergent right worktree.
cp -- "$DRIVER_DIR/claude-prompts/r5.txt" "$DIAG_DIR/task-r5.txt"
run_agent r5 "$RIGHT" "$DIAG_DIR/task-r5.txt"
require_answer_text r5 "RIGHT_LOOP_OK" A_R5_FINAL
require_answer_text r5 "right-python-bad" A_R5_PYRIGHT_MARKER
require_answer_text r5 "right-typescript-bad" A_R5_TYPESCRIPT_MARKER
if /usr/bin/grep -qF 'left-python-bad' "$DIAG_DIR/answer-r5.txt"; then
    fail A_R5_LEFT_LEAK "right answer leaked left worktree content"
fi
cmp -s "$RIGHT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l1.py" \
    || fail A_R5_FILE_CONTENT "right fixture.py is not the helper-edited content"
cmp -s "$LEFT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l2.py" \
    || fail A_R5_LEFT_ISOLATION "left fixture.py changed during the right agent"

write_pass_result
note driver-complete agent-run-claude
exit 0
