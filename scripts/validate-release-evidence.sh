#!/bin/sh
# Requires one complete accepted macOS arm64 product-and-host matrix for a release candidate.

set -eu

[ "$#" -le 2 ] || {
    printf '%s\n' 'usage: scripts/validate-release-evidence.sh [RELEASE_REVISION [EVIDENCE_DIR]]' >&2
    exit 2
}

RELEASE_ROOT=$(/usr/bin/git rev-parse --show-toplevel)
RELEASE_REVISION=$(/usr/bin/git -C "$RELEASE_ROOT" rev-parse "${1:-HEAD}^{commit}")
RELEASE_EVIDENCE_DIR=${2:-$RELEASE_ROOT/docs/evidence}
RELEASE_CANDIDATE=

for RELEASE_REQUIREMENT in \
    'macos-v0.2-product.json|product|product_pass' \
    'macos-v0.2-direct-codex.json|codex|real_pass' \
    'macos-v0.2-direct-claude.json|claude|real_pass' \
    'macos-v0.2-agent-run-claude.json|agent_run_claude|real_pass'
do
    RELEASE_FILE=${RELEASE_REQUIREMENT%%|*}
    RELEASE_REST=${RELEASE_REQUIREMENT#*|}
    RELEASE_ROUTE=${RELEASE_REST%%|*}
    RELEASE_STATUS=${RELEASE_REST#*|}
    RELEASE_PATH="$RELEASE_EVIDENCE_DIR/$RELEASE_FILE"
    [ -f "$RELEASE_PATH" ]
    [ "$(wc -c <"$RELEASE_PATH" | tr -d ' ')" -le 16384 ]
    jq -e --arg route "$RELEASE_ROUTE" --arg status "$RELEASE_STATUS" '
        keys == ["host", "platform", "privacy", "revision", "route", "scenarios", "schema", "status", "toolchains"] and
        .schema == "agent-ide.macos-acceptance.v1" and
        (.revision | test("^[0-9a-f]{40}$")) and
        .route == $route and .status == $status and
        (.platform | keys == ["architecture", "os", "version"]) and
        .platform.os == "macos" and .platform.architecture == "arm64" and
        (.platform.version | test("^[A-Za-z0-9._+-]{1,64}$")) and
        .platform.version != "not_tested" and
        (.host | keys == ["version"]) and
        (.host.version | test("^[A-Za-z0-9._+-]{1,64}$")) and
        .host.version != "not_tested" and
        .toolchains == {
            "go": "1.25.0",
            "gopls": "0.23.0",
            "node": "24.4.0",
            "pyright": "1.1.413",
            "rust": "1.98.1",
            "rust_analyzer": "1.98.1",
            "typescript": "5.9.3",
            "typescript_language_server": "6.0.0"
        } and
        (.scenarios | keys == ["compact_content", "divergent_worktrees", "edit_diagnostic_loop", "native_fallback", "python_provider", "stale_edit_zero_write", "telemetry_restart_query_export", "typescript_r3"]) and
        ([.scenarios[]] | all(. == $status)) and
        (.privacy | keys == ["commands", "credentials", "diagnostic_messages", "paths", "private_ids", "prompts", "source"]) and
        ([.privacy[]] | all(. == false))
    ' "$RELEASE_PATH" >/dev/null
    RELEASE_EVIDENCE_REVISION=$(jq -er '.revision' "$RELEASE_PATH")
    if [ -z "$RELEASE_CANDIDATE" ]; then
        RELEASE_CANDIDATE=$RELEASE_EVIDENCE_REVISION
    else
        [ "$RELEASE_EVIDENCE_REVISION" = "$RELEASE_CANDIDATE" ]
    fi
done

/usr/bin/git -C "$RELEASE_ROOT" cat-file -e "$RELEASE_CANDIDATE^{commit}"
/usr/bin/git -C "$RELEASE_ROOT" merge-base --is-ancestor "$RELEASE_CANDIDATE" "$RELEASE_REVISION"
