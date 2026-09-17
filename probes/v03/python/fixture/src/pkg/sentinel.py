"""Side-effect tripwire: executing this module would create the sentinel file.

Pyright must only parse and analyze project code, never run it, so
``fixture/sentinel_written.txt`` must stay absent after any probe run.
"""

from pathlib import Path

(Path(__file__).resolve().parent.parent.parent / "sentinel_written.txt").write_text(
    "executed"
)
