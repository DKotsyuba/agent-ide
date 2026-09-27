#!/bin/sh
# Builds the deterministic macOS arm64 release bundle from one already-verified executable.

set -eu

[ "$#" -eq 3 ] || {
    printf '%s\n' 'usage: scripts/package-release.sh BINARY TAG OUTPUT_DIR' >&2
    exit 2
}

RELEASE_BINARY=$1
RELEASE_TAG=$2
RELEASE_OUTPUT=$3
RELEASE_ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)

[ -f "$RELEASE_BINARY" ] && [ -x "$RELEASE_BINARY" ] || {
    printf '%s\n' 'release binary must be an executable regular file' >&2
    exit 1
}
case "$RELEASE_TAG" in
    v[0-9]*.[0-9]*.[0-9]*) ;;
    *) printf '%s\n' 'release tag must have vMAJOR.MINOR.PATCH form' >&2; exit 2 ;;
esac
RELEASE_VERSION=${RELEASE_TAG#v}
[ "$(jq -r .version "$RELEASE_ROOT/.codex-plugin/plugin.json")" = "$RELEASE_VERSION" ]
[ "$(jq -r .version "$RELEASE_ROOT/.claude-plugin/plugin.json")" = "$RELEASE_VERSION" ]
[ "$(jq -r '.plugins[0].version' "$RELEASE_ROOT/.claude-plugin/marketplace.json")" = "$RELEASE_VERSION" ]

RELEASE_BUNDLE="agent-ide-$RELEASE_TAG"
RELEASE_ASSET="agent-ide-$RELEASE_TAG-aarch64-apple-darwin.tar.gz"
RELEASE_TMP=$(mktemp -d "${TMPDIR:-/tmp}/agent-ide-package.XXXXXX")
trap 'rm -rf "$RELEASE_TMP"' EXIT HUP INT TERM

mkdir -p \
    "$RELEASE_TMP/$RELEASE_BUNDLE/.agents/plugins" \
    "$RELEASE_TMP/$RELEASE_BUNDLE/.claude-plugin" \
    "$RELEASE_TMP/$RELEASE_BUNDLE/.codex-plugin" \
    "$RELEASE_TMP/$RELEASE_BUNDLE/docs" \
    "$RELEASE_TMP/$RELEASE_BUNDLE/hooks" \
    "$RELEASE_TMP/$RELEASE_BUNDLE/skills/agent-ide/agents"
cp "$RELEASE_BINARY" "$RELEASE_TMP/$RELEASE_BUNDLE/agent-ide"
cp "$RELEASE_ROOT/install.sh" "$RELEASE_TMP/$RELEASE_BUNDLE/install.sh"
cp "$RELEASE_ROOT/README.md" "$RELEASE_TMP/$RELEASE_BUNDLE/README.md"
cp "$RELEASE_ROOT/docs/release.md" "$RELEASE_TMP/$RELEASE_BUNDLE/docs/release.md"
cp "$RELEASE_ROOT/.claude-plugin/plugin.json" "$RELEASE_TMP/$RELEASE_BUNDLE/.claude-plugin/plugin.json"
cp "$RELEASE_ROOT/.claude-plugin/marketplace.json" "$RELEASE_TMP/$RELEASE_BUNDLE/.claude-plugin/marketplace.json"
cp "$RELEASE_ROOT/.codex-plugin/plugin.json" "$RELEASE_TMP/$RELEASE_BUNDLE/.codex-plugin/plugin.json"
cp "$RELEASE_ROOT/hooks/hooks.json" "$RELEASE_TMP/$RELEASE_BUNDLE/hooks/hooks.json"
cp "$RELEASE_ROOT/hooks/claude-hook.sh" "$RELEASE_TMP/$RELEASE_BUNDLE/hooks/claude-hook.sh"
cp "$RELEASE_ROOT/skills/agent-ide/SKILL.md" "$RELEASE_TMP/$RELEASE_BUNDLE/skills/agent-ide/SKILL.md"
cp "$RELEASE_ROOT/skills/agent-ide/agents/openai.yaml" "$RELEASE_TMP/$RELEASE_BUNDLE/skills/agent-ide/agents/openai.yaml"
printf '%s\n' \
    '{' \
    '  "name": "agent-ide",' \
    '  "interface": {"displayName": "Agent IDE"},' \
    '  "plugins": [' \
    '    {' \
    '      "name": "agent-ide",' \
    '      "source": {"source": "local", "path": "./"}' \
    '    }' \
    '  ]' \
    '}' >"$RELEASE_TMP/$RELEASE_BUNDLE/.agents/plugins/marketplace.json"
chmod 755 \
    "$RELEASE_TMP/$RELEASE_BUNDLE/agent-ide" \
    "$RELEASE_TMP/$RELEASE_BUNDLE/install.sh" \
    "$RELEASE_TMP/$RELEASE_BUNDLE/hooks/claude-hook.sh"

# Seals the bundle so `agent-ide self-install` can verify it end to end: metadata names the
# version, SHA256SUMS covers every regular file except itself, and COMPLETE — written last, with
# the exact bytes whose digest is already recorded — asserts the finished verifiable whole.
release_bundle="$RELEASE_TMP/$RELEASE_BUNDLE"
jq -n --arg version "$RELEASE_VERSION" '{version: $version, format: 1}' \
    >"$release_bundle/metadata.json"
(
    cd "$release_bundle"
    complete_hash=$(printf 'complete\n' | shasum -a 256 | awk '{print $1}')
    {
        find . -type f ! -name SHA256SUMS ! -name COMPLETE -print | sed 's|^\./||'
        printf '%s\n' COMPLETE
    } | LC_ALL=C sort | while IFS= read -r relative; do
        if [ "$relative" = COMPLETE ]; then
            printf '%s  %s\n' "$complete_hash" "$relative"
        else
            shasum -a 256 "$relative"
        fi
    done >SHA256SUMS
    printf 'complete\n' >COMPLETE
)
find "$RELEASE_TMP/$RELEASE_BUNDLE" -exec touch -t 197001010000 {} +

mkdir -p "$RELEASE_OUTPUT"
COPYFILE_DISABLE=1 tar -cf - -C "$RELEASE_TMP" "$RELEASE_BUNDLE" \
    | gzip -n >"$RELEASE_OUTPUT/$RELEASE_ASSET"
(
    cd "$RELEASE_OUTPUT"
    shasum -a 256 "$RELEASE_ASSET" >SHA256SUMS
)
printf '%s\n' "$RELEASE_OUTPUT/$RELEASE_ASSET"
