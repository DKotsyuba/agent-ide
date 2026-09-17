"""Module B: carries the warning-level issue and the missing third-party import."""

import os  # intentionally unused: reportUnusedImport forced to "warning"

import definitely_not_installed_thirdparty  # reportMissingImports -> error

from .a import add


def describe(value: int) -> str:
    """Describe a sum produced by ``a.add``."""
    return f"sum={add(value, value)}"
