"""Small helpers to write Markdown reports."""

from __future__ import annotations

import math
from typing import Sequence


def ci(m: float, lo: float, hi: float, digits: int = 2) -> str:
    """'m [lo, hi]' with a fixed number of decimals."""
    if any(math.isnan(x) for x in (m, lo, hi)):
        return f"{m:.{digits}f}"
    return f"{m:.{digits}f} [{lo:.{digits}f}, {hi:.{digits}f}]"


def table(headers: Sequence[str], rows: Sequence[Sequence[object]]) -> str:
    out = ["| " + " | ".join(headers) + " |", "|" + "---|" * len(headers)]
    out += ["| " + " | ".join(str(c) for c in r) + " |" for r in rows]
    return "\n".join(out)


SIMULATION_ONLY = (
    "> **Simulation evidence only.** These numbers describe the hft-sim synthetic market "
    "(docs/STYLIZED_FACTS.md §5 and docs/MARKET_MAKING.md §7 list its artefacts, notably RISKS "
    "K21 and K23). They are not evidence about any real market."
)
