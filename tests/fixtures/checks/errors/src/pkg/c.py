"""Module C: never opened in an editor; holds the stale cross-module call."""

from .a import add

TOTAL = add(1, 2, 3)  # stale call: a.add takes two args -> reportCallIssue error
