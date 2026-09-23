#!/bin/sh
# Runs the shared agent-run host driver for the Codex acceptance route.
set -eu
DRIVER_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec "$DRIVER_DIR/agent-run-claude.sh"
