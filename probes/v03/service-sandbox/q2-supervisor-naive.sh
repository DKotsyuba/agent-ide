#!/bin/bash
# Naive supervisor: spawns a provider-like child that itself spawns a grandchild.
# No process-group / kqueue mitigation. Used to show descendants survive SIGKILL of supervisor.
sh -c 'sh -c "sleep 300 & sleep 300"' &
CHILD=$!
echo "SUPERVISOR_PID=$$ CHILD_PID=$CHILD"
wait
