#!/bin/sh
# Probe: PostToolUse hook emitting an Agent IDE-shaped context block (200 ms delay).
cat >/dev/null
sleep 0.2
printf '%s\n' '{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"<agent-ide>\nerrors 3 · warnings 5 · checked 0s ago\n</agent-ide>"}}'
