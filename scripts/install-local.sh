#!/bin/sh
# Builds and installs a source checkout into a user prefix (default $HOME/.local) so agent-run
# runtimes, crew hooks, and Claude skills catalogs can point at a stable installed path instead of
# this checkout. Never targets the real $HOME/.local from an automated task or test run.

set -eu

usage() {
    printf '%s\n' 'usage: scripts/install-local.sh [--prefix DIR] [--dry-run] [--no-build]' >&2
}

script_dir=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)

prefix="$HOME/.local"
dry_run=0
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
        --dry-run)
            dry_run=1
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

# Reports and, unless --dry-run is set, executes one filesystem-mutating command; every call
# prints its argv on stdout before running so a dry run shows the exact plan.
act() {
    printf '+ %s\n' "$*"
    if [ "$dry_run" -eq 0 ]; then
        "$@"
    fi
}

version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo_root/Cargo.toml" | head -n 1)
[ -n "$version" ] || {
    printf 'cannot read version from %s\n' "$repo_root/Cargo.toml" >&2
    exit 1
}

bin_dir="$prefix/bin"
installed_bin="$bin_dir/agent-ide"
plugin_root="$prefix/share/agent-ide/plugin"
version_dir="$plugin_root/$version"
current_link="$plugin_root/current"
release_bin="$repo_root/target/release/agent-ide"

# Determines the backup-file suffix for a previously installed binary: its own `--version`
# output, sanitized to safe filename characters, when it prints one, else a UTC timestamp. Reads
# only; never writes. Prints nothing when no previous binary is installed.
detect_old_version_label() {
    [ -e "$installed_bin" ] || return 0
    version_output=""
    if [ -x "$installed_bin" ]; then
        version_output=$("$installed_bin" --version 2>/dev/null) || version_output=""
    fi
    if [ -n "$version_output" ]; then
        printf '%s' "$version_output" | head -n 1 | tr -c 'A-Za-z0-9._-' '-'
    else
        date -u +ts-%Y%m%dT%H%M%SZ
    fi
}

old_version_label=$(detect_old_version_label)

if [ "$no_build" -eq 0 ]; then
    act sh -c 'cd "$1" && exec cargo build --locked --release' -- "$repo_root"
fi

if [ "$dry_run" -eq 0 ]; then
    [ -x "$release_bin" ] || {
        printf 'release binary not found: %s (build first or drop --no-build)\n' "$release_bin" >&2
        exit 1
    }
fi

act mkdir -p "$bin_dir"

if [ -e "$installed_bin" ]; then
    act cp "$installed_bin" "$bin_dir/agent-ide.bak-$old_version_label"
fi

tmp_bin="$bin_dir/agent-ide.tmp-$$"
act cp "$release_bin" "$tmp_bin"
act chmod 755 "$tmp_bin"
act mv -f "$tmp_bin" "$installed_bin"

staged_version_dir="$plugin_root/$version.tmp-$$"
act mkdir -p "$plugin_root"
act rm -rf "$staged_version_dir"
act mkdir -p "$staged_version_dir"
for part in .claude-plugin .codex-plugin hooks skills; do
    act cp -R "$repo_root/$part" "$staged_version_dir/$part"
done

hook_path="$staged_version_dir/hooks/claude-hook.sh"
printf '+ generate %s (exec %s claude-hook)\n' "$hook_path" "$installed_bin"
if [ "$dry_run" -eq 0 ]; then
    printf '#!/bin/sh\nexec "%s" claude-hook\n' "$installed_bin" >"$hook_path"
    chmod 755 "$hook_path"
fi

act rm -rf "$version_dir"
act mv -f "$staged_version_dir" "$version_dir"

tmp_link="$plugin_root/current.tmp-$$"
act rm -f "$tmp_link"
act ln -s "$version" "$tmp_link"
act mv -f "$tmp_link" "$current_link"

if [ "$dry_run" -eq 0 ]; then
    launcher_config="$HOME/.config/agent-ide/launcher.json"
    if [ -f "$launcher_config" ]; then
        if launcher_output=$("$installed_bin" launcher check "$launcher_config" 2>&1); then
            printf 'launcher check: ok (%s)\n' "$launcher_config"
        else
            printf 'launcher check: warning - %s\n' "$launcher_output" >&2
        fi
    else
        printf 'launcher check: skipped, no %s\n' "$launcher_config"
    fi

    [ -f "$version_dir/hooks/hooks.json" ] || {
        printf 'installed bundle is missing hooks/hooks.json\n' >&2
        exit 1
    }
    [ -f "$version_dir/skills/agent-ide/SKILL.md" ] || {
        printf 'installed bundle is missing skills/agent-ide/SKILL.md\n' >&2
        exit 1
    }
    [ -x "$version_dir/hooks/claude-hook.sh" ] || {
        printf 'generated hooks/claude-hook.sh is not executable\n' >&2
        exit 1
    }
fi

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
printf 'agent-ide install: %s -> %s\n' "${old_version_label:-none}" "$version"
printf '  binary:  %s\n' "$installed_bin"
printf '  plugin:  %s\n' "$version_dir"
printf '  current: %s\n' "$current_link"
