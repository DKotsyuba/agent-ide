"""Module C: never opened in an editor; holds the stale cross-module call.

The CLI run must still report its error (whole-project analysis); the LSP route
would surface it only when this file is opened (see ../lsp_warm.mjs).
"""

from .a import add

TOTAL = add(1, 2, 3)  # stale call: a.add takes two args -> reportCallIssue error
