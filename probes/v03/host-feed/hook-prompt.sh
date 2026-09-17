#!/bin/sh
# Probe: UserPromptSubmit hook emitting a context block (no delay).
cat >/dev/null
printf '%s\n' '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"<agent-ide>\nerrors 1 · prompt-submit probe\n</agent-ide>"}}'
