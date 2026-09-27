#!/bin/sh
# Builds this checkout into a sealed release bundle and installs it through
# `agent-ide self-install` — the one code path that writes the standalone prefix, both
# `current` symlinks, and the managed launcher. Never targets the real $HOME/.local from an
# automated task or test run.

set -eu

usage() {
    printf '%s\n' 'usage: scripts/install-local.sh [--prefix DIR] [--no-build]' >&2
}

fail() {
    printf 'install-local.sh: %s\n' "$1" >&2
    exit 1
}

script_dir=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)

prefix="$HOME/.local"
no_build=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        --prefix)
            [ "$#" -ge 2 ] || { usage; exit 2; }
            prefix=$2
            shift 2
            ;;
        --prefix=*)
            prefix=${1#--prefix=}
            shift
            ;;
        --no-build)
            no_build=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            usage
            exit 2
            ;;
    esac
done

case "$prefix" in
    /*) ;;
    *) prefix="$PWD/$prefix" ;;
esac

version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo_root/Cargo.toml" | head -n 1)
[ -n "$version" ] || fail "cannot read version from $repo_root/Cargo.toml"

if [ "$no_build" -eq 0 ]; then
    (cd "$repo_root" && cargo build --locked --release)
fi
release_bin="$repo_root/target/release/agent-ide"
[ -x "$release_bin" ] || fail "release binary not found: $release_bin (build first or drop --no-build)"

# Package this checkout into the sealed bundle and hand it to the installer inside it.
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/agent-ide-install-local.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM
asset=$("$script_dir/package-release.sh" "$release_bin" "v$version" "$tmp_dir")
tar -xzf "$asset" -C "$tmp_dir"
bundle="$tmp_dir/agent-ide-v$version"
[ -x "$bundle/agent-ide" ] || fail "the packaged bundle is missing its binary"

"$bundle/agent-ide" self-install \
    --release "$bundle" \
    --version "$version" \
    --prefix "$prefix" \
    --bin-dir "$prefix/bin" \
    --share-dir "$prefix/share/agent-ide" \
    --replace

current_link="$prefix/share/agent-ide/plugin/current"
printf '\n'
printf 'agent-run (~/.agent-run/config.toml) runtimes: replace the checkout path with\n'
printf '  plugins = [..., "%s"]\n' "$current_link"
printf 'crew (crew.toml): source root\n'
printf '  agent_ide = "%s"\n' "$current_link"
printf 'and hook commands\n'
printf '  %s/hooks/claude-hook.sh\n' "$current_link"
printf 'Claude skills catalogs: symlink target\n'
printf '  %s/skills/agent-ide\n' "$current_link"
printf '\n'
printf 'Codex native hooks (managed): print the exact hooks.json fragment with\n'
printf '  %s codex-hooks print\n' "$prefix/bin/agent-ide"
printf 'agent-run resolves %s once when its service starts: restart that service after this\n' "$current_link"
printf 'install, or its Claude runtimes keep loading the previous plugin version.\n'
