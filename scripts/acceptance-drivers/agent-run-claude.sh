#!/bin/sh
# Status: both agent-run cells passed for release 0.6.2 (revision 2db699f).

# Real-host driver for installed agent-run to Claude and Codex acceptance routes.
#
# The runner starts this executable with AGENT_IDE_ACCEPTANCE_ROUTE=
# agent-run-claude or agent-run-codex, two isolated fixture worktrees, and a fresh result
# path. The driver starts real agents through the installed agent-run 0.19.x
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
# The route selects its matching provider and model. Claude keeps Sonnet; Codex
# uses the economical gpt-6-luna default. Both share the same host-neutral
# prompt family.
#
# The outer `perl alarm` bounds each start call independently of agent-run's
# own timeout setting. Like the direct drivers, each scenario is attempted up
# to three times when its post-checks fail transiently (a transcript without
# tool rows, or an unfixed fixture file); a scenario that passes on a later
# attempt counts as passed, with the attempt number only in the diagnostic
# log. Setting
# AGENT_IDE_ACCEPTANCE_DRY=1 prints the exact session command lines and exits 0
# without running anything.
#
# Operator environment (all optional, defaults fit the release host):
#   AGENT_IDE_ACCEPTANCE_BINARY         candidate agent-ide executable used for
#                                       identity, telemetry, and evidence
#                                       support; default
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
        ;;
    agent-run-codex)
        DEFAULT_PROVIDER=codex
        DEFAULT_MODEL=gpt-6-luna
        ;;
    *) fail E_ROUTE "unsupported agent-run route" ;;
esac
# Host-neutral acceptance prompt family shared with every other driver.
PROMPT_FAMILY=prompts
# Maximum model-session attempts per scenario before the driver fails, as in claude.sh.
MAX_ATTEMPTS=3
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
    for scenario in l1:LEFT l1b:LEFT l2:LEFT l3:LEFT l4:LEFT l5:LEFT r5:RIGHT r5b:RIGHT; do
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
# Since 0.4.1 the installed path is normally the managed launcher shim; the binary it execs
# (`<prefix>/current/agent-ide`) is what the broker's agents run, so that is what must match.
if [ "$(sed -n 2p "$MCP_BINARY" 2>/dev/null)" = "# agent-ide managed launcher v1" ]; then
    MCP_BINARY=$(sed -n "s/^exec '\(.*\)' \"\$@\"$/\1/p" "$MCP_BINARY")
    [ -n "$MCP_BINARY" ] && [ -r "$MCP_BINARY" ] \
        || fail installed_binary_differs "the managed launcher does not name a readable binary"
fi
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

# Requires one complete agent-run transcript to pair each Agent IDE call with a bounded text reply.
#
# Agent-run stores ordered messages with string content, unlike the direct-host JSONL transcripts.
# One call may span a block-start row and continuation rows (agent-run 0.20.2), so each call is
# paired with its result rows by their shared raw_ref. Claude names the tools `mcp__agent_ide__…`,
# Codex (agent-run 0.20.3 exports its tool rows) `agent_ide/ide.…`. The first argument is its private JSON path;
# the second is a closed failure code. Empty or malformed transcripts, missing results, and replies
# above 16 KiB fail the whole host cell.
require_agent_run_compact_replies() {
    jq -e --argjson bound 16384 '
        def text: if type == "string" then . else (map(.text // "") | join("")) end;
        .complete == true and .next_cursor == null and (.messages | type == "array") and
        (.messages as $messages
         | [$messages[]
            | select(.role == "tool_call" and (.starts_block // true)
                and ((.name // "") | test("^(mcp__agent[-_]ide__|agent[-_]ide/)ide[._]")))
            | .raw_ref] as $calls
         | ($calls | length > 0)
           and all($calls[]; . as $ref
               | $ref != null
                 and ([$messages[] | select(.role == "tool_result" and .raw_ref == $ref)
                       | .content | text]
                      | length > 0 and (join("") | utf8bytelength <= $bound))))
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
        # The model answered the completion token without calling a single Agent IDE tool (the
        # transcript carries no tool rows). This is the one transient scenario failure, already
        # noted under A_${label}_COMPACT_REPLIES above, so report it to run_scenario instead of
        # failing the driver at once.
        return 1
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
    "$BINARY" errors --repo "$worktree" --all --since 15 --limit 4000 >"$journal" 2>>"$DIAG_LOG" \
        || fail "E_${label}_JOURNAL" "errors reader exited nonzero"
    # An activation completes inline (`start completed`) or answers `start pending` and is
    # delivered by a later `inspect completed`; either shape proves the round trip.
    for expected in "start pending|completed" "stop completed"; do
        set -- $expected
        awk -v since="$since" -v method="$1" -v outcomes="$2" \
            'BEGIN { n = split(outcomes, ok, "|") }
             $1 >= since && $3 == method { for (i = 1; i <= n; i++) if ($4 == ok[i]) found = 1 }
             END { exit !found }' \
            "$journal" \
            || fail "A_${label}_JOURNAL_$1" "journal has no $expected after $since"
    done
    note "agent-$label-journal-backed" "transcript carried no tool rows; daemon journal proves the round trips"
}

# Requires the agent's final answer to contain one literal token.
require_answer_text() {
    /usr/bin/grep -qF -- "$2" "$DIAG_DIR/answer-$1.txt" \
        || fail "$3" "answer $1 lacks $2"
}

# Requires one fixture file to hold the exact expected bytes. Unlike the closed post-checks,
# a content miss is retryable: the model may have skipped its tool calls, so the miss is
# recorded and the scenario rerun rather than the driver failing at once.
require_retryable_file() {
    label=$1
    actual=$2
    expected=$3
    if cmp -s "$actual" "$expected"; then
        return 0
    fi
    note "A_${label}_FILE_CONTENT" "fixture file is not the expected edited content"
    return 1
}

# Repeats one agent-run scenario until its post-checks accept or attempts run out, the same
# bounded discipline the direct drivers apply. The arguments are the label, the worktree, the
# prompt file basename under the prompt family, the verify function name, and the reset
# function name. A scenario that passes on a later attempt counts as passed; only the
# diagnostic log records the attempt numbers.
run_scenario() {
    label=$1
    worktree=$2
    prompt_file=$3
    verify=$4
    reset=$5
    attempt=1
    while [ "$attempt" -le "$MAX_ATTEMPTS" ]; do
        note "scenario-$label-attempt" "$attempt"
        task_prompt "$DRIVER_DIR/$PROMPT_FAMILY/$prompt_file" "$attempt" \
            >"$DIAG_DIR/task-$label.txt"
        if run_agent "$label" "$worktree" "$DIAG_DIR/task-$label.txt" && "$verify"; then
            note "scenario-$label-passed" "attempt $attempt"
            return 0
        fi
        [ -f "$DIAG_DIR/transcript-$label.json" ] && mv -- \
            "$DIAG_DIR/transcript-$label.json" \
            "$DIAG_DIR/transcript-$label-fail$attempt.json"
        attempt=$((attempt + 1))
        "$reset" "$worktree"
    done
    fail "A_${label}_SCENARIO" "scenario $label never passed within $MAX_ATTEMPTS attempts"
}

# Restores the committed fixture state of one worktree between attempts, exactly as the direct
# drivers do, so a retried scenario never inherits a partial ide.edit.
reset_committed() {
    /usr/bin/git -C "$1" checkout -q -- acceptance-fixture \
        || fail E_FIXTURE_RESET "git checkout failed in $1"
}

# No reset between attempts: the scenario's only retryable failure is a transcript without
# tool rows, which leaves the worktree untouched, and its precondition may be a later
# scenario's post state that a checkout would destroy.
reset_none() {
    :
}

# Requires the agent's stored transcript (tool results included) to contain one
# literal token. The scripts force a fixed final reply, so tool outcomes such as
# stale_source or the semantic mode line only ever appear in tool results, as on
# the direct Claude route. Markers checked here never occur in the task prompts.
require_record_text() {
    if /usr/bin/grep -qF -- "$2" "$DIAG_DIR/transcript-$1.json"; then
        return 0
    fi
    # A journal-backed Codex cell (see require_ide_journal_activity) has a transcript without
    # tool rows, so a tool-result marker cannot be observed there at all; the skip is recorded
    # and the direct Codex cell proves the same marker with the same binary and launcher.
    if [ "$PROVIDER" = codex ] \
        && ! jq -e 'any(.messages[]?; .role == "tool_call")' "$DIAG_DIR/transcript-$1.json" \
            >/dev/null 2>&1
    then
        note "$3" "journal-backed cell: transcript carries no tool rows, marker unobservable: $2"
        return 0
    fi
    fail "$3" "transcript $1 lacks $2"
}

# The edit ledger is durable per repository, so a later run must never reuse an
# earlier run's operation_id (it would answer conflicting_duplicate). Each run
# suffixes every operation_id with this driver's PID and the scenario attempt,
# as the direct Claude driver does per session; verifiers never read the id.
task_prompt() {
    sed "s/\"operation_id\":\"\([A-Za-z0-9-]*\)\"/\"operation_id\":\"\\1-$$-$2\"/g" "$1"
}

if [ "$AGENT_IDE_ACCEPTANCE_ROUTE" = agent-run-claude ]; then
    LEFT_RUNTIME=$("$BINARY" claude-rendezvous "$LEFT" | sed -n 's/^runtime_dir=//p')
    RIGHT_RUNTIME=$("$BINARY" claude-rendezvous "$RIGHT" | sed -n 's/^runtime_dir=//p')
    [ -n "$LEFT_RUNTIME" ] && [ -n "$RIGHT_RUNTIME" ] || fail E_CLAUDE_RUNTIME
fi

# Agent L1: real edit/diagnostic/fix/diff/stop loop over Pyright.
printf 'def value() -> int:\n    return 0\n' >"$DIAG_DIR/expected-l1.py"

# Post-checks for l1: the answer token, the Codex-only Pyright marker, and the exact edited
# file. The file-content miss is the retryable one.
verify_l1() {
    require_answer_text l1 "LEFT_LOOP_OK" A_L1_FINAL
    if [ "$AGENT_IDE_ACCEPTANCE_ROUTE" = agent-run-codex ]; then
        require_record_text l1 "not assignable" A_L1_PYRIGHT_SEMANTIC
    fi
    require_retryable_file l1 "$LEFT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l1.py"
}

# Post-checks for l1b: the answer token and the diff marker. Only the transcript class can
# retry, and the precondition is l1's post state, which a checkout would destroy.
verify_l1b() {
    require_answer_text l1b "LEFT_DIFF_OK" A_L1B_FINAL
    require_record_text l1b "left-python-bad" A_L1B_DIFF_CONTENT
}

run_scenario l1 "$LEFT" l1.txt verify_l1 reset_committed
if [ "$AGENT_IDE_ACCEPTANCE_ROUTE" = agent-run-codex ]; then
    run_scenario l1b "$LEFT" l1b.txt verify_l1b reset_none
fi

# Agent L2: native fallback while inactive, then a stale edit with zero writes.
# Post-checks for l2: the answer token and the stale outcome marker. Only the transcript
# class can retry, and the precondition is l1's post state, so no reset.
verify_l2() {
    require_answer_text l2 "LEFT_FALLBACK_OK" A_L2_FINAL
    require_record_text l2 "stale_source" A_L2_STALE_OUTCOME
}
run_scenario l2 "$LEFT" l2.txt verify_l2 reset_none
# A byte-exact compare against a fixed expectation is too strict: a native tool (for
# example BSD `sed -i '' '1i\...'`) may insert the marker line with different trailing
# whitespace than an idealized rendering while still writing zero IDE bytes. Three
# content checks prove the same fact without pinning the native tool's exact formatting.
left_py=$LEFT/acceptance-fixture/fixture.py
[ "$(sed -n '1p' "$left_py")" = "# native acceptance marker" ] \
    || fail A_L2_ZERO_WRITE "left fixture.py does not start with the native marker"
grep -qF "return 0" "$left_py" \
    || fail A_L2_ZERO_WRITE "left fixture.py lost the fixed return"
if grep -qF "return 7" "$left_py"; then
    fail A_L2_ZERO_WRITE "left fixture.py absorbed the stale edit's return 7"
fi
cp -- "$left_py" "$DIAG_DIR/left-after-l2.py" || fail A_L2_ZERO_WRITE "could not snapshot left fixture"

# Agent L3: real TypeScript semantic context through the accepted bundle.
verify_l3() {
    require_answer_text l3 "LEFT_TS_OK" A_L3_FINAL
    require_record_text l3 "mode: semantic" A_L3_SEMANTIC
}
run_scenario l3 "$LEFT" l3.txt verify_l3 reset_none

# Agent L5: symbol-addressed outline/symbol/edit/read loop over the Rust fixture crate. The
# inserted method must leave no trace and the crate must still compile afterward.
verify_l5() {
    require_answer_text l5 "LEFT_SYMBOLS_OK" A_L5_FINAL
    require_record_text l5 "impl Counter" A_L5_OUTLINE_IMPL
    require_record_text l5 "pub fn get" A_L5_OUTLINE_GET
    require_record_text l5 "symbol: get — method" A_L5_SYMBOL_HEADING
    require_record_text l5 "acceptance-fixture/tests/counter.rs" A_L5_SYMBOL_USAGE
    require_record_text l5 "edit: inserted" A_L5_EDIT_INSERTED
    require_record_text l5 "pub fn doubled" A_L5_READ_DOUBLED
    verify_symbol_tools_left_clean "$LEFT" A_L5_LEFT_CLEAN \
        || fail A_L5_LEFT_CLEAN "left fixture crate not clean or not compiling after l5"
}
run_scenario l5 "$LEFT" l5.txt verify_l5 reset_none

# Restart-safe telemetry across a fresh agent's daemon generation.
if [ "$AGENT_IDE_ACCEPTANCE_ROUTE" = agent-run-claude ]; then
    # The managed daemon removes its runtime launcher copy the moment its last lease closes, which
    # can happen before this line runs (l5's agent has already exited). That copy names the
    # daemon's own worktree as its first target, so the left fixture itself is the same candidate.
    telemetry_candidate=$(jq -er '.targets[0].candidate | select(type == "string" and startswith("/"))' "$LEFT_RUNTIME/launcher.json" 2>/dev/null \
        || printf '%s' "$LEFT") \
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

# Post-checks for l4: the answer token and durable telemetry growth across the restart. A
# retried attempt only adds rows, so the growth check stays valid across attempts.
verify_l4() {
    require_answer_text l4 "LEFT_RESTART_OK" A_L4_FINAL
    after_rows=$("$BINARY" telemetry export --database "$TELEMETRY_DB" 2>>"$DIAG_LOG" | wc -l | tr -d ' ')
    [ "$after_rows" -gt "$before_rows" ] || fail A_TELEMETRY_RESTART_LOST "no new rows after restart"
}
run_scenario l4 "$LEFT" l4.txt verify_l4 reset_none

# Agent R5: the complete loop in the divergent right worktree.
# Post-checks for r5: the answer token, the right-worktree markers, the left-leak guard, and
# the exact edited right file. The file-content miss is the retryable one; the leak guard and
# the left-worktree isolation snapshot stay closed.
verify_r5() {
    require_answer_text r5 "RIGHT_LOOP_OK" A_R5_FINAL
    require_record_text r5 "right-python-bad" A_R5_PYRIGHT_MARKER
    require_record_text r5 "right-typescript-bad" A_R5_TYPESCRIPT_MARKER
    for marker in left-python-bad left-typescript-bad; do
        if /usr/bin/grep -qF "$marker" "$DIAG_DIR/transcript-r5.json"; then
            fail A_R5_LEFT_LEAK "right transcript leaked left worktree content"
        fi
    done
    require_retryable_file r5 "$RIGHT/acceptance-fixture/fixture.py" "$DIAG_DIR/expected-l1.py"
}
run_scenario r5 "$RIGHT" r5.txt verify_r5 reset_committed
cmp -s "$LEFT/acceptance-fixture/fixture.py" "$DIAG_DIR/left-after-l2.py" \
    || fail A_R5_LEFT_ISOLATION "left fixture.py changed during the right agent"
if [ "$AGENT_IDE_ACCEPTANCE_ROUTE" = agent-run-codex ]; then
    # Post-checks for r5b: the answer token, the diff marker, and the leak guard. Only the
    # transcript class can retry, and the precondition is r5's post state, so no reset.
    verify_r5b() {
        require_answer_text r5b "RIGHT_DIFF_OK" A_R5B_FINAL
        require_record_text r5b "right-python-bad" A_R5B_DIFF_CONTENT
        for marker in left-python-bad left-typescript-bad; do
            if /usr/bin/grep -qF "$marker" "$DIAG_DIR/transcript-r5b.json"; then
                fail A_R5B_LEFT_LEAK "right diff transcript leaked left worktree content"
            fi
        done
    }
    run_scenario r5b "$RIGHT" r5b.txt verify_r5b reset_none
fi

write_pass_result
note driver-complete "$AGENT_IDE_ACCEPTANCE_ROUTE"
exit 0
