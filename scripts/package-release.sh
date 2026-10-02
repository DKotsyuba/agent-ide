#!/bin/sh
# Builds the deterministic macOS arm64 release bundle from one already-verified executable.
#
# Thin wrapper kept for documentation and the acceptance drivers: the packager is
# `cargo xtask package BINARY TAG OUTPUT_DIR` (xtask/src/release.rs), which writes the tarball and
# its SHA256SUMS into OUTPUT_DIR and prints the tarball path last.

set -eu

[ "$#" -eq 3 ] || {
    printf '%s\n' 'usage: scripts/package-release.sh BINARY TAG OUTPUT_DIR' >&2
    exit 2
}

RELEASE_ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
exec cargo run --quiet --locked --manifest-path "$RELEASE_ROOT/Cargo.toml" --package xtask -- \
    package "$1" "$2" "$3"
