#!/usr/bin/env python3
"""Tiny Unix-domain socket echo listener for the Q3 sandboxed-Claude reachability probe.
Listens at argv[1], accepts one connection, echoes back "ECHO:<received>", then exits.
"""
import socket
import sys
import os

path = sys.argv[1]
if os.path.exists(path):
    os.unlink(path)
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(path)
srv.listen(1)
print(f"listening on {path}", flush=True)
srv.settimeout(30)
try:
    conn, _ = srv.accept()
    data = conn.recv(4096)
    print(f"received: {data!r}", flush=True)
    conn.sendall(b"ECHO:" + data)
    conn.close()
except socket.timeout:
    print("timeout waiting for a connection", flush=True)
finally:
    srv.close()
    os.unlink(path)
