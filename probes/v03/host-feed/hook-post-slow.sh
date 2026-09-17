#!/bin/sh
# Probe: same as fast but sleeps 2 s; settings declare "timeout": 1 to test kill/fail-open.
cat >/dev/null
sleep 2
printf '%s\n' '{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"<agent-ide>\nerrors 9 · warnings 9 · slow\n</agent-ide>"}}'
