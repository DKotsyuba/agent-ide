#!/bin/sh
# Claude evidence exists for an earlier candidate; Codex live run pending.

# Real-host driver for installed agent-run to Claude and Codex acceptance routes.
#
# The runner starts this executable with AGENT_IDE_ACCEPTANCE_ROUTE=
# agent-run-claude or agent-run-codex, two isolated fixture worktrees, and a fresh result
# path. The driver starts real agents through the installed agent-run 0.14.x
# CLI. Its resident broker spawns the agent process, so environment variables
# set on the start command line (AGENT_IDE_BIN, HOME) do NOT reach the agent:
# the agent's host, agent-ide MCP, and plugin come entirely from the
# operator's ~/.agent-run/config.toml [mcp.agent_ide] entry (installed
# /Users/pluto/.local/bin/agent-ide plus launcher.json). The driver therefore
# fails closed before starting any agent unless the installed binary is
# byte-identical to the release candidate. Scenario verification uses the
# agents' stored answers, real filesystem effects, and the durable telemetry
# database.
#
# The route selects its matching provider and prompt family. Claude keeps Sonnet;
# Codex uses the economical gpt-6-luna default and prompts without a helper.
#
# The outer `perl alarm` bounds each start call independently of agent-run's
# own timeout setting. Setting
# AGENT_IDE_ACCEPTANCE_DRY=1 prints the exact session command lines and exits 0
# without running anything.
#
# Operator environment (all optional, defaults fit the release host):
#   AGENT_IDE_ACCEPTANCE_BINARY         candidate agent-ide executable used for
#                                       identity, telemetry, and evidence
#                                       helpers; default
#                                       <repo>/target/release/agent-ide.
#   AGENT_IDE_ACCEPTANCE_AGENT_RUN      installed agent-run CLI; default
#                                       /Users/pluto/.agent-run/standalone/current/bin/agent-run.
#   AGENT_IDE_ACCEPTANCE_AGENT_RUN_MCP_BINARY binary the agent-run mcp.agent_ide
#                                       table executes; default
#                                       /Users/pluto/.local/bin/agent-ide. It
#                                       must be byte-identical to the release
#                                       candidate; the driver refuses anything
#                                       else with installed_binary_differs.
#   AGENT_IDE_ACCEPTANCE_AGENT_RUN_PROVIDER agent-run schema-2 provider id;
#                                       must match the route (claude or codex).
#   AGENT_IDE_ACCEPTANCE_AGENT_RUN_MODEL    model id; default sonnet for Claude
#                                       and gpt-6-luna for Codex.
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

case "${AGENT_IDE_ACCEPTANCE_ROUTE:-}" in
    agent-run-claude)
        DEFAULT_PROVIDER=claude
        DEFAULT_MODEL=sonnet
        PROMPT_FAMILY=claude-prompts
        ;;
    agent-run-codex)
        DEFAULT_PROVIDER=codex
        DEFAULT_MODEL=gpt-6-luna
        PROMPT_FAMILY=codex-prompts
        ;;
    *) fail E_ROUTE "unsupported agent-run route" ;;
esac
[ -n "${AGENT_IDE_ACCEPTANCE_LEFT_WORKTREE:-}" ] || fail E_LEFT_WORKTREE_MISSING
[ -n "${AGENT_IDE_ACCEPTANCE_RIGHT_WORKTREE:-}" ] || fail E_RIGHT_WORKTREE_MISSING
[ -n "${AGENT_IDE_ACCEPTANCE_RESULT:-}" ] || fail E_RESULT_PATH_MISSING

BINARY=${AGENT_IDE_ACCEPTANCE_BINARY:-$DRIVER_ROOT/target/release/agent-ide}
AGENT_RUN=${AGENT_IDE_ACCEPTANCE_AGENT_RUN:-/Users/pluto/.agent-run/standalone/current/bin/agent-run}
MCP_BINARY=${AGENT_IDE_ACCEPTANCE_AGENT_RUN_MCP_BINARY:-/Users/pluto/.local/bin/agent-ide}
PROVIDER=${AGENT_IDE_ACCEPTANCE_AGENT_RUN_PROVIDER:-$DEFAULT_PROVIDER}
AR_MODEL=${AGENT_IDE_ACCEPTANCE_AGENT_RUN_MODEL:-$DEFAULT_MODEL}
[ -z "${AGENT_IDE_ACCEPTANCE_AGENT_RUN_RUNTIME:-}" ] \
    || fail E_RUNTIME_OPTION_UNSUPPORTED "use AGENT_IDE_ACCEPTANCE_AGENT_RUN_PROVIDER"
[ "$PROVIDER" = "$DEFAULT_PROVIDER" ] || fail E_PROVIDER "provider does not match route"
AR_PROFILE=${AGENT_IDE_ACCEPTANCE_AGENT_RUN_PROFILE:-implement}
LAUNCHER=${AGENT_IDE_ACCEPTANCE_LAUNCHER:-/Users/pluto/.config/agent-ide/launcher.json}
OPERATOR_HOME=${AGENT_IDE_ACCEPTANCE_OPERATOR_HOME:-/Users/pluto}
SESSION_SECONDS=${AGENT_IDE_ACCEPTANCE_SESSION_SECONDS:-1800}

# Dry run: print the exact session command lines and exit without running
# anything. Contract variables only need to be set; no host is touched.
if [ "${AGENT_IDE_ACCEPTANCE_DRY:-0}" = 1 ]; then
    for scenario in l1:LEFT l1b:LEFT l2:LEFT l3:LEFT l4:LEFT r5:RIGHT r5b:RIGHT; do
        case "$scenario:$AGENT_IDE_ACCEPTANCE_ROUTE" in *b:*:agent-run-claude) continue ;; esac
        label=${scenario%%:*}
        eval "worktree=\$AGENT_IDE_ACCEPTANCE_${scenario##*:}_WORKTREE"
        printf '%s\n' "/usr/bin/perl -e 'alarm shift; exec @ARGV' $SESSION_SECONDS \
$AGENT_RUN start --provider $PROVIDER --model $AR_MODEL --profile $AR_PROFILE \
--task \"\$(cat $DIAG_DIR/task-$label.txt)\" --workdir $worktree --write --wait \
> $DIAG_DIR/start-$label.out 2> $DIAG_DIR/start-$label.err
$AGENT_RUN answer \$agent_id > $DIAG_DIR/answer-$label.txt"
    done
    exit 0
fi

[ -x "$BINARY" ] || fail E_BINARY_MISSING "$BINARY"
[ -x "$AGENT_RUN" ] || fail E_AGENT_RUN_MISSING "$AGENT_RUN"
[ -x "$MCP_BINARY" ] || fail E_MCP_BINARY_MISSING "$MCP_BINARY"
[ -r "$LAUNCHER" ] || fail E_LAUNCHER_MISSING "$LAUNCHER"

# The route can only claim the candidate contract when the binary the resident
# broker actually executes is byte-identical to the release candidate, and the
# launcher template declares the accepted host profile. Environment
# set here cannot redirect the agent to the candidate, so a differing installed
# binary fails the cell closed before any agent starts.
cmp -s "$BINARY" "$MCP_BINARY" \
    || fail installed_binary_differs \
        "$MCP_BINARY is not byte-identical to the candidate $BINARY"
if [ "$AGENT_IDE_ACCEPTANCE_ROUTE" = agent-run-claude ]; then
    /usr/bin/grep -q 'claude-r3-2026-09-14' "$LAUNCHER" \
        || fail E_LAUNCHER_NO_CLAUDE_TYPESCRIPT "launcher template lacks the accepted Claude TypeScript record"
fi

LEFT=$(canonical_dir "$AGENT_IDE_ACCEPTANCE_LEFT_WORKTREE") || fail E_LEFT_CANONICAL
RIGHT=$(canonical_dir "$AGENT_IDE_ACCEPTANCE_RIGHT_WORKTREE") || fail E_RIGHT_CANONICAL
if [ "$AGENT_IDE_ACCEPTANCE_ROUTE" = agent-run-codex ]; then
    jq -e --arg left "$LEFT" --arg right "$RIGHT" '
        . as $launcher | all([$left, $right][]; . as $path
            | any($launcher.allowed_roots[]?; . as $root
                | $path == $root or ($path | startswith($root + "/"))))
    ' "$LAUNCHER" >/dev/null 2>>"$DIAG_LOG" \
        || fail E_WORKTREE_OUTSIDE_LAUNCHER_ROOT "fixture worktrees are outside launcher allowed_roots"
fi
LEFT_IDENTITY=$(project_identity "$BINARY" "$LEFT") || fail E_LEFT_IDENTITY
[ "${#LEFT_IDENTITY}" = 64 ] || fail E_LEFT_IDENTITY_LENGTH
RIGHT_IDENTITY=$(project_identity "$BINARY" "$RIGHT") || fail E_RIGHT_IDENTITY

mkdir -p -- "$DIAG_DIR"

# Prepares the project-local sandbox socket allowance for one worktree.
prepare_worktree() {
    socket=$2/claude-helper.sock
    mkdir -p -- "$1/.claude" || fail E_SETTINGS_DIR "$1"
    jq -n --arg socket "$socket" \
        '{sandbox:{network:{allowUnixSockets:[$socket]}}}' \
        >"$1/.claude/settings.local.json" || fail E_SETTINGS_WRITE "$1"
}

# Requires one complete agent-run transcript to pair each Agent IDE call with a bounded text reply.
#
# Agent-run stores ordered messages with string content, unlike the direct-host JSONL transcripts.
# The first argument is its private JSON path; the second is a closed failure code. Empty or
# malformed transcripts, missing results, and replies above 16 KiB fail the whole host cell.
require_agent_run_compact_replies() {
    jq -e --argjson bound 16384 '
        .complete == true and .next_cursor == null and (.messages | type == "array") and
        ([.messages as $messages
          | range(0; $messages | length) as $i
          | select($messages[$i].role == "tool_call"
              and (($messages[$i].name // "") | test("^mcp__agent[-_]ide__ide[._]")))
          | $messages[$i + 1]] as $results
         | ($results | length > 0)
           and all($results[];
               .role == "tool_result"
               and (.content | type == "string" and utf8bytelength <= $bound)))
    ' "$1" >/dev/null 2>>"$DIAG_LOG" \
        || { note "$2" "missing, malformed, or oversized Agent IDE reply"; return 1; }
}

# Runs one wall-clock-bounded agent-run agent and collects its final answer.
#
# The first argument is a short label, the second the worktree, and the third
# the task file. No environment is attached to the start command: the resident
# broker spawns the agent process, so CLI-level variables (AGENT_IDE_BIN, HOME)
# never reach it — the agent runs with the broker's environment and the
# operator's config.toml MCP entry. The outer `perl alarm` remains the
# per-start wall-clock bound. The label, worktree, and answer are appended to
# the private diagnostic log; agent identifiers never enter public evidence.
run_agent() {
    label=$1
    worktree=$2
    task_file=$3
    answer_file=$DIAG_DIR/answer-$label.txt
    started_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    if /usr/bin/perl -e 'alarm shift; exec @ARGV' "$SESSION_SECONDS" \
        "$AGENT_RUN" start --provider "$PROVIDER" --model "$AR_MODEL" \
        --profile "$AR_PROFILE" --task "$(cat "$task_file")" \
        --workdir "$worktree" --write --wait \
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
    "$AGENT_RUN" transcript --full --limit 1000 "$agent_id" \
        >"$DIAG_DIR/transcript-$label.json" 2>>"$DIAG_LOG" \
        || fail "E_${label}_TRANSCRIPT" "transcript exited nonzero"
    if require_agent_run_compact_replies "$DIAG_DIR/transcript-$label.json" "A_${label}_COMPACT_REPLIES"; then
        :
    elif [ "$PROVIDER" = codex ]; then
        # agent-run exports a Codex transcript without tool rows (only the assistant stream), so
        # the Codex cell proves each Agent IDE round trip through the daemon's own journal for this
        # fixture repository instead: the activation and the stop must have completed after the
        # agent started. The bounded-reply size check is not reproducible from the journal; the
        # diagnostic log records that this cell is journal-backed.
        require_ide_journal_activity "$worktree" "$started_at" "$label"
    else
        fail "A_${label}_COMPACT_REPLIES" "agent $label has no bounded Agent IDE replies"
    fi
    printf '%s %s\n' "$label" "$agent_id" >>"$DIAG_DIR/agent-ids.log"
}

# Requires the daemon journal for the fixture repository to record a completed activation and a
# completed stop at or after `since` (an RFC 3339 UTC instant). Used only where the host transcript
# carries no tool rows; the journal names no reply bytes, so this proves the round trips, not
# their size.
require_ide_journal_activity() {
    worktree=$1
    since=$2
    label=$3
    journal=$DIAG_DIR/ide-journal-$label.txt
    "$BINARY" errors --repo "$worktree" --all --since 30 --limit 400 >"$journal" 2>>"$DIAG_LOG" \
        || fail "E_${label}_JOURNAL" "errors reader exited nonzero"
    for method in start stop; do
        awk -v since="$since" -v method="$method" \
            '$1 >= since && $3 == method && $4 == "completed" { found = 1 } END { exit !found }' \
            "$journal" \
            || fail "A_${label}_JOURNAL_${method}" "journal has no completed $method after $since"
    done
    note "agent-$label-journal-backed" "transcript carried no tool rows; daemon journal proves the round trips"
}

# Requires the agent's final answer to contain one literal token.
require_answer_text() {
    /usr/bin/grep -qF -- "$2" "$DIAG_DIR/answer-$1.txt" \
        || fail "$3" "answer $1 lacks $2"
}

# Requires the agent's stored transcript (tool results included) to contain one
# literal token. The scripts force a fixed final reply, so tool outcomes such as
# stale_source or the semantic mode line only ever appear in tool results, as on
# the direct Claude route. Markers checked here never occur in the task prompts.
require_record_text() {
    /usr/bin/grep -qF -- "$2" "$DIAG_DIR/transcript-$1.json" \
        || fail "$3" "transcript $1 lacks $2"
}

# The edit ledger is durable per repository, so a later run must never reuse an
# earlier run's operation_id (it would answer conflicting_duplicate). Each run
# suffixes every operation_id with this driver's PID, as the direct Claude
# driver does per session; verifiers never read the id.
task_prompt() {
    sed "s/\"operation_id\":\"\([A-Za-z0-9-]*\)\"/\"operation_id\":\"\\1-$$\"/g" "$1"
}

if [ "$AGENT_IDE_ACCEPTANCE_ROUTE" = agent-run-claude ]; then
    LEFT_RUNTIME=$("$BINARY" claude-rendezvous "$LEFT" | sed -n 's/^runtime_dir=//p')
    RIGHT_RUNTIME=$("$BINARY" claude-rendezvous "$RIGHT" | sed -n 's/^runtime_dir=//p')
    [ -n "$LEFT_RUNTIME" ] && [ -n "$RIGHT_RUNTIME" ] || fail E_CLAUDE_RUNTIME
    prepare_worktree "$LEFT" "$LEFT_RUNTIME"
    prepare_worktree "$RIGHT" "$RIGHT_RUNTIME"
fi

# Agent L1: real edit/diagnostic/fix/diff/stop loop over Pyright.
task_prompt "$DRIVER_DIR/$PROMPT_FAMILY/l1.txt" >"$DIAG_DIR/task-l1.txt"
run_agent l1 "$LEFT" "$DIAG_DIR/task-l1.txt"
require_answer_text l1 "LEFT_LOOP_OK" A_L1_FINAL
if [ "$AGENT_IDE_ACCEPTANCE_ROUTE" = agent-run-codex ]; then
    require_record_text l1 "not assignable" A_L1_PYRIGHT_SEMANTIC
fi
printf 'def value() -> int:\n    return 0\n' >"$DIAG_DIR/expected-l1.py"
cmp -s "$LEFT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l1.py" \
    || fail A_L1_FILE_CONTENT "left fixture.py is not the helper-edited content"
if [ "$AGENT_IDE_ACCEPTANCE_ROUTE" = agent-run-codex ]; then
    task_prompt "$DRIVER_DIR/$PROMPT_FAMILY/l1b.txt" >"$DIAG_DIR/task-l1b.txt"
    run_agent l1b "$LEFT" "$DIAG_DIR/task-l1b.txt"
    require_answer_text l1b "LEFT_DIFF_OK" A_L1B_FINAL
    require_record_text l1b "left-python-bad" A_L1B_DIFF_CONTENT
fi

# Agent L2: native fallback while inactive, then a stale edit with zero writes.
task_prompt "$DRIVER_DIR/$PROMPT_FAMILY/l2.txt" >"$DIAG_DIR/task-l2.txt"
run_agent l2 "$LEFT" "$DIAG_DIR/task-l2.txt"
require_answer_text l2 "LEFT_FALLBACK_OK" A_L2_FINAL
require_record_text l2 "stale_source" A_L2_STALE_OUTCOME
printf '# native acceptance marker\ndef value() -> int:\n    return 0\n' >"$DIAG_DIR/expected-l2.py"
cmp -s "$LEFT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l2.py" \
    || fail A_L2_ZERO_WRITE "left fixture.py does not prove the stale edit wrote nothing"

# Agent L3: real TypeScript semantic context through the accepted bundle.
task_prompt "$DRIVER_DIR/$PROMPT_FAMILY/l3.txt" >"$DIAG_DIR/task-l3.txt"
run_agent l3 "$LEFT" "$DIAG_DIR/task-l3.txt"
require_answer_text l3 "LEFT_TS_OK" A_L3_FINAL
require_record_text l3 "mode: semantic" A_L3_SEMANTIC

# Restart-safe telemetry across a fresh agent's daemon generation.
if [ "$AGENT_IDE_ACCEPTANCE_ROUTE" = agent-run-claude ]; then
    telemetry_candidate=$(jq -er '.targets[0].candidate | select(type == "string" and startswith("/"))' "$LEFT_RUNTIME/launcher.json") \
        || fail E_TELEMETRY_CANDIDATE
    telemetry_identity=$(project_identity "$BINARY" "$telemetry_candidate") \
        || fail E_TELEMETRY_IDENTITY
    TELEMETRY_DB=$OPERATOR_HOME/.agent-ide/telemetry/$telemetry_identity/state.sqlite
else
    TELEMETRY_DB=$OPERATOR_HOME/.agent-ide/telemetry/$LEFT_IDENTITY/state.sqlite
fi
[ -f "$TELEMETRY_DB" ] || fail A_TELEMETRY_DB_MISSING "no durable database before restart"
before_rows=$("$BINARY" telemetry export --database "$TELEMETRY_DB" 2>>"$DIAG_LOG" | wc -l | tr -d ' ')
[ "${before_rows:-0}" -ge 1 ] || fail A_TELEMETRY_PRE_RESTART_EMPTY "export before restart"
task_prompt "$DRIVER_DIR/$PROMPT_FAMILY/l4.txt" >"$DIAG_DIR/task-l4.txt"
run_agent l4 "$LEFT" "$DIAG_DIR/task-l4.txt"
require_answer_text l4 "LEFT_RESTART_OK" A_L4_FINAL
after_rows=$("$BINARY" telemetry export --database "$TELEMETRY_DB" 2>>"$DIAG_LOG" | wc -l | tr -d ' ')
[ "$after_rows" -gt "$before_rows" ] || fail A_TELEMETRY_RESTART_LOST "no new rows after restart"

# Agent R5: the complete loop in the divergent right worktree.
task_prompt "$DRIVER_DIR/$PROMPT_FAMILY/r5.txt" >"$DIAG_DIR/task-r5.txt"
run_agent r5 "$RIGHT" "$DIAG_DIR/task-r5.txt"
require_answer_text r5 "RIGHT_LOOP_OK" A_R5_FINAL
require_record_text r5 "right-python-bad" A_R5_PYRIGHT_MARKER
require_record_text r5 "right-typescript-bad" A_R5_TYPESCRIPT_MARKER
for marker in left-python-bad left-typescript-bad; do
    if /usr/bin/grep -qF "$marker" "$DIAG_DIR/transcript-r5.json"; then
        fail A_R5_LEFT_LEAK "right transcript leaked left worktree content"
    fi
done
cmp -s "$RIGHT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l1.py" \
    || fail A_R5_FILE_CONTENT "right fixture.py is not the helper-edited content"
cmp -s "$LEFT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l2.py" \
    || fail A_R5_LEFT_ISOLATION "left fixture.py changed during the right agent"
if [ "$AGENT_IDE_ACCEPTANCE_ROUTE" = agent-run-codex ]; then
    task_prompt "$DRIVER_DIR/$PROMPT_FAMILY/r5b.txt" >"$DIAG_DIR/task-r5b.txt"
    run_agent r5b "$RIGHT" "$DIAG_DIR/task-r5b.txt"
    require_answer_text r5b "RIGHT_DIFF_OK" A_R5B_FINAL
    require_record_text r5b "right-python-bad" A_R5B_DIFF_CONTENT
    for marker in left-python-bad left-typescript-bad; do
        if /usr/bin/grep -qF "$marker" "$DIAG_DIR/transcript-r5b.json"; then
            fail A_R5B_LEFT_LEAK "right diff transcript leaked left worktree content"
        fi
    done
fi

write_pass_result
note driver-complete "$AGENT_IDE_ACCEPTANCE_ROUTE"
exit 0
