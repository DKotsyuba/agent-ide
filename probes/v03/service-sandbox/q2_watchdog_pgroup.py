#!/usr/bin/env python3
"""Mitigation A: watchdog starts the supervisor in its own new session/process group
(start_new_session=True == setsid(2)); on cleanup it killpg()s that whole group instead
of just the supervisor pid, which also reaps any grandchildren the supervisor spawned
(as long as they never call setsid/setpgid themselves, which is the common case for
plain `sh -c` / fork-exec provider children).
"""
import os
import signal
import subprocess
import sys
import time

def pgrep(pattern):
    out = subprocess.run(["pgrep", "-fl", pattern], capture_output=True, text=True).stdout
    return [line for line in out.splitlines() if line.strip()]

supervisor = subprocess.Popen(
    ["sh", "-c", "sh -c 'sleep 300 & sleep 300' & wait"],
    start_new_session=True,
)
pgid = os.getpgid(supervisor.pid)
print(f"supervisor pid={supervisor.pid} pgid={pgid}")
time.sleep(1)

print("=== before kill ===")
for line in pgrep("sleep 300"):
    print(line)

print("=== killpg(pgid, SIGKILL) ===")
os.killpg(pgid, signal.SIGKILL)
supervisor.wait()
time.sleep(1)

print("=== after killpg ===")
survivors = pgrep("sleep 300")
for line in survivors:
    print(line)
print("SURVIVORS:", len(survivors))
sys.exit(1 if survivors else 0)
