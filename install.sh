#!/bin/sh
# Downloads one sealed agent-ide release, verifies it, and installs it through the bundle's own
# `agent-ide self-install`.
#
# usage: install.sh [--version X.Y.Z] [--home DIR] [--prefix DIR] [--bin-dir DIR]
#                   [--share-dir DIR] [--downloader curl|wget] [-h]
#
# The GitHub repository is private today: either export GITHUB_TOKEN (sent as
# `Authorization: Bearer`) or keep an authenticated `gh` on PATH, which this script falls back
# to for downloads when no token is set.

set -eu
umask 077

repository="${AGENT_IDE_REPOSITORY:-DKotsyuba/agent-ide}"
version=""
home=""
prefix=""
bin_dir=""
share_dir=""
downloader=""

usage_text() {
    printf '%s\n' \
        'usage: install.sh [--version X.Y.Z] [--home DIR] [--prefix DIR] [--bin-dir DIR]' \
        '                  [--share-dir DIR] [--downloader curl|wget] [-h]' \
        '' \
        'Downloads the sealed agent-ide release bundle, verifies its SHA256SUMS, and runs the' \
        "bundle's own \`agent-ide self-install\`. macOS arm64 only." \
        '' \
        '  --version X.Y.Z   install this version (a leading `v` is allowed); default: latest release' \
        '  --home DIR        state home (default: ~/.agent-ide; AGENT_IDE_HOME relocates the user home)' \
        '  --prefix DIR      standalone prefix (default: <home>/standalone)' \
        '  --bin-dir DIR     launcher directory (default: ~/.local/bin)' \
        '  --share-dir DIR   plugin root parent (default: ~/.local/share/agent-ide)' \
        '  --downloader TOOL force `curl` or `wget` (default: auto-detect)' \
        '  -h                print this help' \
        '' \
        'The repository is private today: export GITHUB_TOKEN (sent as `Authorization: Bearer`)' \
        'or keep an authenticated `gh` on PATH, which is used as the download fallback.'
}

fail() {
    printf 'install.sh: %s\n' "$1" >&2
    exit 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || fail '--version needs a value'
            version=$2
            shift 2
            ;;
        --version=*)
            version=${1#--version=}
            shift
            ;;
        --home)
            [ "$#" -ge 2 ] || fail '--home needs a value'
            home=$2
            shift 2
            ;;
        --prefix)
            [ "$#" -ge 2 ] || fail '--prefix needs a value'
            prefix=$2
            shift 2
            ;;
        --bin-dir)
            [ "$#" -ge 2 ] || fail '--bin-dir needs a value'
            bin_dir=$2
            shift 2
            ;;
        --share-dir)
            [ "$#" -ge 2 ] || fail '--share-dir needs a value'
            share_dir=$2
            shift 2
            ;;
        --share-dir=*)
            share_dir=${1#--share-dir=}
            shift
            ;;
        --downloader)
            [ "$#" -ge 2 ] || fail '--downloader needs a value'
            downloader=$2
            shift 2
            ;;
        -h | --help)
            usage_text
            exit 0
            ;;
        *)
            fail "unknown argument: $1"
            ;;
    esac
done

# macOS arm64 is the only claimed platform.
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
    fail 'agent-ide supports macOS arm64 only'

# Resolves the downloader: forced --downloader wins, then curl, then wget.
if [ -n "$downloader" ]; then
    case "$downloader" in
        curl | wget) ;;
        *) fail "--downloader must be curl or wget, got: $downloader" ;;
    esac
    command -v "$downloader" >/dev/null 2>&1 || fail "$downloader is not on PATH"
else
    if command -v curl >/dev/null 2>&1; then
        downloader=curl
    elif command -v wget >/dev/null 2>&1; then
        downloader=wget
    else
        fail 'neither curl nor wget is on PATH'
    fi
fi

# fetch URL FILE — https-only curl or no-config wget, with the bearer token when set.
fetch() {
    if [ -n "${GITHUB_TOKEN:-}" ]; then
        case "$downloader" in
            curl) curl -q --proto '=https' -fsSL -H "Authorization: Bearer $GITHUB_TOKEN" --output "$2" "$1" ;;
            wget) wget --no-config -q --header="Authorization: Bearer $GITHUB_TOKEN" -O "$2" "$1" ;;
        esac
    else
        case "$downloader" in
            curl) curl -q --proto '=https' -fsSL --output "$2" "$1" ;;
            wget) wget --no-config -q -O "$2" "$1" ;;
        esac
    fi
}

case "$version" in
    v*) version=${version#v} ;;
esac
if [ -z "$version" ]; then
    # No --version: resolve the latest published release through the GitHub API.
    tmp_latest=$(mktemp "${TMPDIR:-/tmp}/agent-ide-latest.XXXXXX")
    fetch "https://api.github.com/repos/$repository/releases/latest" "$tmp_latest" ||
        fail 'cannot query the latest release (set GITHUB_TOKEN or authenticate gh)'
    version=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$tmp_latest" | head -n 1)
    rm -f "$tmp_latest"
    [ -n "$version" ] || fail 'the latest-release response carried no tag_name'
fi
case "$version" in
    v*) version=${version#v} ;;
esac
printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' ||
    fail "--version must be X.Y.Z, got: $version"

tag="v$version"
asset="agent-ide-$tag-aarch64-apple-darwin.tar.gz"

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/agent-ide-install.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

if [ -z "${GITHUB_TOKEN:-}" ] && command -v gh >/dev/null 2>&1; then
    # Token-less private-repository downloads fall back to an authenticated gh CLI.
    gh release download "$tag" --repo "$repository" \
        --pattern "$asset" --pattern SHA256SUMS --dir "$tmp_dir" --clobber ||
        fail "gh could not download release $tag"
else
    fetch "https://github.com/$repository/releases/download/$tag/$asset" "$tmp_dir/$asset" ||
        fail "cannot download $asset"
    fetch "https://github.com/$repository/releases/download/$tag/SHA256SUMS" "$tmp_dir/SHA256SUMS" ||
        fail 'cannot download SHA256SUMS'
fi

# The tarball hash must be listed in, and match, the release-level SHA256SUMS.
# Entries may carry a leading directory (`./name`); match on the file name only.
expected=$(awk -v asset="$asset" '{ name = $2; sub(/^.*\//, "", name) } name == asset { print $1 }' "$tmp_dir/SHA256SUMS")
[ -n "$expected" ] || fail "SHA256SUMS does not list $asset"
actual=$(shasum -a 256 "$tmp_dir/$asset" | awk '{print $1}')
[ "$actual" = "$expected" ] || fail "the downloaded $asset does not match SHA256SUMS"

# Lists the archive and rejects anything that is not a plain relative file or directory.
tar -tvzf "$tmp_dir/$asset" >"$tmp_dir/listing" || fail 'cannot list the downloaded archive'
while IFS= read -r line; do
    case $line in
        -* | d*) ;;
        *) fail "the archive contains a non-regular entry: $line" ;;
    esac
    path=${line##* }
    case $path in
        /* | *..*) fail "the archive contains an unsafe path: $path" ;;
    esac
done <"$tmp_dir/listing"

tar -xzf "$tmp_dir/$asset" --no-same-owner --no-same-permissions -C "$tmp_dir" ||
    fail 'cannot extract the downloaded archive'
bundle="$tmp_dir/agent-ide-$tag"
[ -d "$bundle" ] && [ -f "$bundle/COMPLETE" ] && [ -x "$bundle/agent-ide" ] ||
    fail "the archive is missing the $tag bundle"

# Installs through the bundle itself; every verification runs once more inside self-install.
set -- "$bundle/agent-ide" self-install --release "$bundle" --version "$version"
if [ -n "$home" ]; then
    set -- "$@" --home "$home"
fi
if [ -n "$prefix" ]; then
    set -- "$@" --prefix "$prefix"
fi
if [ -n "$bin_dir" ]; then
    set -- "$@" --bin-dir "$bin_dir"
fi
if [ -n "$share_dir" ]; then
    set -- "$@" --share-dir "$share_dir"
fi
# The launcher path self-install reports is the one it wrote — the resolved user home may
# differ from $HOME, and --home/--bin-dir relocate it — so read it from the JSON summary.
summary=$("$@")
printf '%s\n' "$summary"
launcher=$(printf '%s' "$summary" | sed -n 's/.*"launcher"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
printf 'agent-ide %s installed; launcher: %s\n' "$version" "${launcher:-${bin_dir:-$HOME/.local/bin}/agent-ide}"
