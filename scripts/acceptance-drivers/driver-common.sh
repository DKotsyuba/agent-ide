#!/bin/sh
# Status: refreshed for the current hosts; first passing run pending; see
# docs/macos-acceptance.md results table.

# Shared helpers for the committed real-host acceptance drivers.
#
# A driver is executed by scripts/macos-acceptance.sh with no arguments and only
# the AGENT_IDE_ACCEPTANCE_ROUTE, AGENT_IDE_ACCEPTANCE_LEFT_WORKTREE,
# AGENT_IDE_ACCEPTANCE_RIGHT_WORKTREE, and AGENT_IDE_ACCEPTANCE_RESULT
# environment settings. Every helper keeps the public contract closed: the only
# public success output is the exact nine-line host-cell result document, and
# every failure is recorded as a closed code in a private diagnostic log that
# never enters evidence.

# Absolute repository root that contains this driver directory.
DRIVER_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)

# Private append-only diagnostic log outside the repository and evidence.
#
# Defaults to a per-host stable path so an operator can correlate a failed run;
# AGENT_IDE_ACCEPTANCE_DIAG_LOG overrides it. Transcripts are kept beside it.
DIAG_LOG=${AGENT_IDE_ACCEPTANCE_DIAG_LOG:-${TMPDIR:-/tmp}/agent-ide-acceptance-driver.log}
DIAG_DIR=$(dirname -- "$DIAG_LOG")

# Appends one closed failure or progress code with a UTC timestamp.
#
# The first argument is the closed code token and the second is optional private
# context. Nothing here is printed to stdout, so the runner never turns driver
# diagnostics into public evidence.
note() {
    mkdir -p -- "$DIAG_DIR" || :
    printf '%s %s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" "${2:-}" >>"$DIAG_LOG" || :
}

# Fails the driver honestly: records the closed code and exits nonzero.
#
# The runner converts any nonzero exit, or a missing or incomplete result
# document, into `failed` evidence for every scenario of the cell.
fail() {
    note "$1" "${2:-}"
    printf '%s\n' "driver failed: $1" >&2
    exit 1
}

# Prints the physical absolute form of one existing directory.
canonical_dir() {
    (CDPATH= cd -- "$1" && pwd -P) || return 1
}

# Prints the 64-hex character project identity of one canonical directory path.
#
# The first argument is the tested candidate `agent-ide` executable and the
# second the canonical worktree path. The product derives this identity as
# BLAKE3 of the raw canonical path bytes, so the driver measures a temporary
# file holding exactly those bytes with the same `evidence executable` helper
# the operator uses. Prints nothing and returns nonzero when measurement fails.
project_identity() {
    identity_tmp=$DIAG_DIR/.path-bytes.$$
    printf '%s' "$2" >"$identity_tmp" || return 1
    chmod u+x "$identity_tmp" || return 1
    "$1" evidence executable --identity driver-path "$identity_tmp" 2>>"$DIAG_LOG" \
        | sed -n 's/.*"blake3":"\([0-9a-f]\{64\}\)".*/\1/p'
    rm -f -- "$identity_tmp"
}

# Writes the exact closed nine-line host-cell result document on full success.
#
# The only permitted status is real_pass for every scenario; a partial cell must
# fail instead of writing a reduced document.
write_pass_result() {
    printf '%s\n' \
        'schema=agent-ide.host-cell.v1' \
        'edit_diagnostic_loop=real_pass' \
        'stale_edit_zero_write=real_pass' \
        'native_fallback=real_pass' \
        'telemetry_restart_query_export=real_pass' \
        'compact_content=real_pass' \
        'python_provider=real_pass' \
        'typescript_r3=real_pass' \
        'divergent_worktrees=real_pass' \
        >"$AGENT_IDE_ACCEPTANCE_RESULT"
}

# Requires at least one use of one tool name in a stream-json transcript.
#
# The first argument is the transcript file, the second a tool name such as
# mcp__agent-ide__ide_start, and the third a closed assertion code recorded in
# the diagnostic log when the tool use is absent. Returns nonzero instead of
# exiting so a retrying scenario can treat the miss as one failed attempt.
require_tool_use() {
    count=$(jq -s '[.[] | select(.type == "assistant")
        | .message.content[]? | select(.type == "tool_use")
        | select(.name == $name)] | length' --arg name "$2" "$1" 2>>"$DIAG_LOG")
    [ "$count" -ge 1 ] || { note "$3" "expected tool use $2 in $(basename -- "$1")"; return 1; }
}

# Requires one literal to appear in a captured stream-json transcript.
#
# The scan stringifies every JSON event line, which is enough for closed markers
# that the product renders into tool result text.
require_transcript_text() {
    matched=$(jq -sr --arg needle "$2" '
        [.[] | tostring | select(contains($needle))] | length' "$1" 2>>"$DIAG_LOG")
    [ "$matched" -ge 1 ] || { note "$3" "expected text $2 in $(basename -- "$1")"; return 1; }
}

# Requires one literal to be absent from a captured stream-json transcript.
forbid_transcript_text() {
    matched=$(jq -sr --arg needle "$2" '
        [.[] | tostring | select(contains($needle))] | length' "$1" 2>>"$DIAG_LOG")
    [ "$matched" -eq 0 ] || { note "$3" "forbidden text $2 in $(basename -- "$1")"; return 1; }
}

# Prints the input object of the first tool use with the given name, or null.
first_tool_input() {
    jq -s '[.[] | select(.type == "assistant")
        | .message.content[]? | select(.type == "tool_use")
        | select(.name == $name) | .input][0]' --arg name "$2" "$1" 2>>"$DIAG_LOG"
}

# Requires every Agent IDE tool result in one transcript to stay compact.
#
# Compact means one bounded decision-facing text block of at most 16 KiB per
# reply, matching the product's compact MCP projection contract. The second
# argument is the closed assertion code for the diagnostic log.
require_compact_replies() {
    oversized=$(jq -s --argjson bound 16384 '[.[] | select(.type == "user")
        | .message.content[]? | select(.type == "tool_result")
        | (.content // []) | if type == "array" then . else [] end
        | .[] | select(.type == "text") | (.text | length) | select(. > $bound)
        ] | length' "$1" 2>>"$DIAG_LOG")
    [ "${oversized:-1}" -eq 0 ] || { note "$2" "oversized reply text"; return 1; }
}

# Requires one Bash helper launch to be the immediately next tool use.
#
# The Claude foreground helper must be claimed with no native tool post between
# the minting reply and the launch, so the transcript must show the Bash call as
# the next tool use event. The first argument is the transcript, the second the
# minting tool name whose reply mints the helper, and the third a closed code.
require_helper_immediately_after() {
    ordered=$(jq -s -r '[.[] | select(.type == "assistant")
        | .message.content[]? | select(.type == "tool_use") | .name] | join(",")' \
        "$1" 2>>"$DIAG_LOG")
    case ",$ordered," in
        *",$2,Bash,"*) ;;
        *) note "$3" "Bash helper not immediately after $2 in $(basename -- "$1")"; return 1 ;;
    esac
}
