"""Parsing human-written durations such as "1h30m" or "45s"."""

import re

_PART = re.compile(r"(\d+)([hms])")
_UNITS = "hms"
_SECONDS = {"h": 60, "m": 60, "s": 1}


def parse_duration(text):
    """Returns the number of seconds in `text`, such as "1h30m" (5400) or "90s" (90).

    A duration is one or more parts, each a whole number and a unit: hours (h),
    minutes (m) or seconds (s). Each unit appears at most once, in that order, with
    nothing between the parts. Case and surrounding spaces don't matter. Raises
    ValueError for anything else.
    """
    text = text.strip().lower()
    if not text:
        raise ValueError("empty duration")
    total = 0
    pos = 0
    last = -1
    for match in _PART.finditer(text):
        number, unit = match.groups()
        order = _UNITS.index(unit)
        if match.start() != pos or order <= last:
            raise ValueError(f"invalid duration: {text!r}")
        total += int(number) * _SECONDS[unit]
        pos = match.end()
        last = order
    if pos != len(text):
        raise ValueError(f"invalid duration: {text!r}")
    return total
