"""Fixture: real-shaped Python for the minifier's property tests."""

from dataclasses import dataclass

# Characters that must survive verbatim: a statement separator and a comment
# marker, both inside a string.
TRICKY = "a;b # not a comment"

RAW = """line one
line two; still inside # the triple-quoted string
"""


@dataclass
class Record:
    """A parsed row."""

    name: str
    count: int = 0

    def describe(self, kind):
        if kind == 0:
            return f"{self.name}: point"
        elif kind == 1:
            return f"{self.name}: line"
        else:
            return self.name.upper()


def tally(records):
    out = {}
    for r in records:
        if r.name not in out:
            out[r.name] = 0
# An outdented comment inside a nested block must not re-nest what follows.
        out[r.name] += r.count
    return out


def wide_call(a, b, c):
    return tally(
        [
            Record(a),
            Record(b),
            Record(c),
        ]
    )


def banner(empty):
    if empty or not TRICKY:
        return RAW
    return TRICKY
