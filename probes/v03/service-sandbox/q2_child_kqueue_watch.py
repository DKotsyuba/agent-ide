#!/usr/bin/env python3
"""Mitigation B: kqueue EVFILT_PROC/NOTE_EXIT parent-death watch, run inside the *child*
wrapper itself (macOS has no PR_SET_PDEATHSIG like Linux; this is the documented
equivalent). The wrapper spawns the real grandchild, then blocks on a kqueue watching its
own parent pid for NOTE_EXIT; when the parent (supervisor) dies -- even via SIGKILL, which
gives the supervisor no chance to clean up -- this wrapper notices independently and kills
its grandchild.

Usage: q2_child_kqueue_watch.py <grandchild-cmd...>
Prints its own pid and the parent pid it is watching, then waits.
"""
import os
import select
import signal
import subprocess
import sys

parent_pid = os.getppid()
grandchild = subprocess.Popen(sys.argv[1:])
print(f"wrapper pid={os.getpid()} watching parent pid={parent_pid} grandchild pid={grandchild.pid}", flush=True)

kq = select.kqueue()
ev = select.kevent(parent_pid, filter=select.KQ_FILTER_PROC,
                    flags=select.KQ_EV_ADD, fflags=select.KQ_NOTE_EXIT)
kq.control([ev], 0)

events = kq.control(None, 1, None)  # blocks until parent exits
print(f"wrapper observed parent exit: {events}", flush=True)
grandchild.send_signal(signal.SIGKILL)
grandchild.wait()
print("wrapper killed grandchild, exiting", flush=True)
