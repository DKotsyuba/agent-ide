#!/bin/sh
# Observes one exact release tag/commit/run and verifies its assets (REL-02). The observer is
# `cargo xtask release wait` (xtask/src/release.rs); it never installs anything.
#
# usage: scripts/wait-release.sh --repo DKotsyuba/agent-ide --tag vX.Y.Z --commit FULL_SHA \
#            [--run-id N] [--timeout SECONDS] [--result-file /absolute/new/path.json]

set -eu

RELEASE_ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
exec cargo run --quiet --locked --manifest-path "$RELEASE_ROOT/Cargo.toml" --package xtask -- \
    release wait "$@"
