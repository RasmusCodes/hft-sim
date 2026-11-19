"""Historical-data commands of the `hftsim` binary, from Python (standard library only).

    from hftsim import Scenario
    from hftsim.lobster import check, export, replay_check

    out = export(Scenario(secs=60, warmup_secs=10, makers=(7,)), seed=1, directory="runs/day", levels=10)
    report = check(out["message"], out["orderbook"])          # rebuild + compare with snapshots
    r = replay_check(Scenario(secs=120, warmup_secs=10, makers=(7,), informed=10), seed=2026)

`check` works on real LOBSTER files too; docs/HISTORICAL_DATA.md describes the format, the
validation report and the replay check.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from .runner import Scenario, binary, build


def _call(args: list[str]) -> dict:
    if not binary().exists():
        build()
    proc = subprocess.run([str(binary()), *args], capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(f"hftsim failed ({proc.returncode}): {proc.stderr.strip()}")
    return json.loads(proc.stdout)


def export(scenario: Scenario, seed: int, directory: str | Path, levels: int = 10) -> dict:
    """Run one market and write it as LOBSTER files in `directory`. Returns the summary (provenance,
    manifest, rows, rows by type) with the two file paths added as "message" and "orderbook"."""
    out = _call(["lobster-export", str(directory), "--levels", str(levels), *scenario.args(seed)])
    for f in out["provenance"]["files"]:
        out["message" if "_message_" in f["path"] else "orderbook"] = f["path"]
    return out


def check(message_file: str | Path, orderbook_file: str | Path, levels: int | None = None) -> dict:
    """Rebuild a LOBSTER stock-day and compare it row by row with its order-book file. Returns
    {"provenance": ..., "validation": ...}."""
    extra = ["--levels", str(levels)] if levels else []
    return _call(["lobster-check", str(message_file), str(orderbook_file), *extra])


def replay_check(scenario: Scenario, seed: int, probes: int = 6, interval_ms: float = 300, life_ms: int = 3000, lots: int = 1, latency: str = "colocated") -> dict:
    """Check the replay fill model against the exchange's own fills of probe orders."""
    flags = ["--probes", str(probes), "--probe-interval-ms", repr(float(interval_ms)), "--probe-life-ms", str(life_ms), "--probe-lots", str(lots), "--probe-latency", latency]
    return _call(["replay-check", *flags, *scenario.args(seed)])
