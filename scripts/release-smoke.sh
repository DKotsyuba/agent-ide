#!/bin/sh
# Inspects a downloaded release bundle and executes only its packaged macOS arm64 binary.
#
# Thin wrapper: the checks live in `cargo xtask package verify ARCHIVE` (xtask/src/release.rs) —
# the directory SHA256SUMS, the release manifest binding when one sits next to the archive, the
# exact file set, the seal, the marketplaces, the executable measurement, the Claude hook and a
# disposable self-install.

set -eu

[ "$#" -eq 1 ] || {
    printf '%s\n' 'usage: scripts/release-smoke.sh ARCHIVE' >&2
    exit 2
}

RELEASE_ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
exec cargo run --quiet --locked --manifest-path "$RELEASE_ROOT/Cargo.toml" --package xtask -- \
    package verify "$1"
