#!/bin/sh
[ -n "${AGENT_IDE_BIN:-}" ] || exit 0
case "$AGENT_IDE_BIN" in
    /*) ;;
    *) exit 0 ;;
esac
[ -x "$AGENT_IDE_BIN" ] || exit 0
exec "$AGENT_IDE_BIN" claude-hook
