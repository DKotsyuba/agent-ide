#!/bin/sh
command -v agent-ide >/dev/null 2>&1 || exit 0
exec agent-ide claude-hook
