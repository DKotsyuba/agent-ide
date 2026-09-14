#!/bin/sh
# Inspects a downloaded release bundle and executes only its packaged macOS arm64 binary.

set -eu

[ "$#" -eq 1 ] || {
    printf '%s\n' 'usage: scripts/release-smoke.sh ARCHIVE' >&2
    exit 2
}

RELEASE_ARCHIVE=$1
RELEASE_ARCHIVE_DIR=$(CDPATH= cd -- "$(dirname "$RELEASE_ARCHIVE")" && pwd)
RELEASE_ARCHIVE_NAME=$(basename "$RELEASE_ARCHIVE")
case "$RELEASE_ARCHIVE_NAME" in
    agent-ide-v*-aarch64-apple-darwin.tar.gz) ;;
    *) printf '%s\n' 'unexpected release archive name' >&2; exit 2 ;;
esac
RELEASE_TAG=${RELEASE_ARCHIVE_NAME#agent-ide-}
RELEASE_TAG=${RELEASE_TAG%-aarch64-apple-darwin.tar.gz}
RELEASE_VERSION=${RELEASE_TAG#v}
RELEASE_BUNDLE="agent-ide-$RELEASE_TAG"

(
    cd "$RELEASE_ARCHIVE_DIR"
    shasum -a 256 -c SHA256SUMS
)

RELEASE_EXPECTED=$(printf '%s\n' \
    "$RELEASE_BUNDLE/.agents/plugins/marketplace.json" \
    "$RELEASE_BUNDLE/.claude-plugin/marketplace.json" \
    "$RELEASE_BUNDLE/.claude-plugin/plugin.json" \
    "$RELEASE_BUNDLE/.codex-plugin/plugin.json" \
    "$RELEASE_BUNDLE/README.md" \
    "$RELEASE_BUNDLE/agent-ide" \
    "$RELEASE_BUNDLE/docs/release.md" \
    "$RELEASE_BUNDLE/hooks/claude-hook.sh" \
    "$RELEASE_BUNDLE/hooks/hooks.json" \
    "$RELEASE_BUNDLE/install.sh" \
    "$RELEASE_BUNDLE/skills/agent-ide/SKILL.md" \
    "$RELEASE_BUNDLE/skills/agent-ide/agents/openai.yaml" | LC_ALL=C sort)
RELEASE_ACTUAL=$(tar -tzf "$RELEASE_ARCHIVE" | sed '/\/$/d' | LC_ALL=C sort)
[ "$RELEASE_ACTUAL" = "$RELEASE_EXPECTED" ] || {
    printf '%s\n' 'release archive contents do not match the plugin bundle contract' >&2
    exit 1
}

RELEASE_TMP=$(mktemp -d "${TMPDIR:-/tmp}/agent-ide-smoke.XXXXXX")
trap 'rm -rf "$RELEASE_TMP"' EXIT HUP INT TERM
tar -xzf "$RELEASE_ARCHIVE" -C "$RELEASE_TMP"
RELEASE_BIN="$RELEASE_TMP/$RELEASE_BUNDLE/agent-ide"
[ -x "$RELEASE_BIN" ]
file "$RELEASE_BIN" | grep -q 'Mach-O 64-bit executable arm64'
jq -e --arg version "$RELEASE_VERSION" '.version == $version' \
    "$RELEASE_TMP/$RELEASE_BUNDLE/.codex-plugin/plugin.json" \
    "$RELEASE_TMP/$RELEASE_BUNDLE/.claude-plugin/plugin.json" >/dev/null
jq -e --arg version "$RELEASE_VERSION" \
    '.plugins == [{"name":"agent-ide", "source":"./", "description":"Agent IDE coding-companion skill and Claude lifecycle hooks.", "version":$version}]' \
    "$RELEASE_TMP/$RELEASE_BUNDLE/.claude-plugin/marketplace.json" >/dev/null
jq -e '.plugins == [{"name":"agent-ide", "source":{"source":"local", "path":"./"}}]' \
    "$RELEASE_TMP/$RELEASE_BUNDLE/.agents/plugins/marketplace.json" >/dev/null
RELEASE_MEASUREMENT=$("$RELEASE_BIN" evidence executable --identity "agent-ide-$RELEASE_TAG" "$RELEASE_BIN")
printf '%s\n' "$RELEASE_MEASUREMENT" | jq -e \
    --arg identity "agent-ide-$RELEASE_TAG" \
    --arg path "$RELEASE_BIN" \
    '.identity == $identity and .path == $path and (.blake3 | length == 64)' >/dev/null
printf '%s\n' '{}' | \
    AGENT_IDE_BIN="$RELEASE_BIN" CLAUDE_PROJECT_DIR="$RELEASE_TMP" \
    "$RELEASE_TMP/$RELEASE_BUNDLE/hooks/claude-hook.sh"
