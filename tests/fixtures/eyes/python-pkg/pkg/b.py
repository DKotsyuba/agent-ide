"""Calls `a.combine`; never opened or edited by the acceptance scenario itself."""

from pkg.a import combine


def use_it() -> int:
    return combine(1, 2)
