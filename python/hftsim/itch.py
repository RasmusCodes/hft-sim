"""Nasdaq TotalView-ITCH 5.0 sample days (emi.nasdaq.com/ITCH/): decompress one and run
`hftsim itch-check` on it (docs/HISTORICAL_DATA.md §7). Standard library only.

    python -I -m hftsim.itch check ../data/itch/20191230-bx/20191230.BX_ITCH_50.gz --replay 8

The report lands in a `derived/` folder next to the file.
"""

from __future__ import annotations

import argparse
import gzip
import json
import shutil
import subprocess
import sys
from pathlib import Path


def decompress(gz: Path) -> Path:
    """Decompress `gz` next to itself unless already done. Reading a gzip file to its end checks
    its CRC-32 and length, so a corrupt download fails here."""
    raw = gz.with_suffix("")
    if raw.exists() and raw.stat().st_mtime >= gz.stat().st_mtime:
        return raw
    part = raw.with_name(raw.name + ".part")
    with gzip.open(gz, "rb") as src, open(part, "wb") as dst:
        shutil.copyfileobj(src, dst, 1 << 22)
    part.replace(raw)
    return raw


def day_of(name: str) -> str:
    """The date in a sample file's name as YYYYMMDD: BX and PSX files start with YYYYMMDD,
    main-market files with MMDDYYYY."""
    d = name[:8]
    return d if d[:2] in ("19", "20") else d[4:] + d[:4]


def check(path: Path, replay: int = 8, levels: int = 10, sample: int = 200, symbols: list[str] | None = None, every: int = 1) -> dict:
    """Run `hftsim itch-check` on an ITCH file (decompressing a `.gz` first). With `every` above
    1 the all-orders replay check uses every n-th order (busy days). Progress goes to stderr."""
    from .runner import ensure_built

    raw = decompress(path) if path.suffix == ".gz" else path
    venue = "Nasdaq BX" if "BX_ITCH" in raw.name else "Nasdaq PSX" if "PSX_ITCH" in raw.name else "Nasdaq"
    args = [str(ensure_built()), "itch-check", str(raw), "--replay", str(replay), "--levels", str(levels), "--sample", str(sample), "--every", str(every),
            "--source", f"{venue} TotalView-ITCH 5.0 sample day from emi.nasdaq.com/ITCH/", "--date", day_of(raw.name)]
    if symbols:
        args += ["--symbols", ",".join(symbols)]
    if path != raw:
        args += ["--also", str(path)]
    proc = subprocess.run(args, stdout=subprocess.PIPE, text=True)
    if proc.returncode != 0:
        raise RuntimeError(f"hftsim itch-check failed (exit {proc.returncode}); its messages are above")
    report = json.loads(proc.stdout)
    out = raw.parent / "derived"
    out.mkdir(exist_ok=True)
    (out / "itch_check.json").write_text(json.dumps(report, indent=1) + "\n", encoding="utf-8")
    return report


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="python -m hftsim.itch", description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    d = sub.add_parser("decompress", help="decompress a .gz day next to itself")
    d.add_argument("file", type=Path)
    c = sub.add_parser("check", help="book, checks and replay check on a day")
    c.add_argument("file", type=Path)
    c.add_argument("--replay", type=int, default=8, help="most-traded stocks to replay")
    c.add_argument("--levels", type=int, default=10)
    c.add_argument("--sample", type=int, default=200, help="filled orders per stock for the counterfactual")
    c.add_argument("--symbols", default="", help="comma-separated symbols instead of the most traded")
    c.add_argument("--every", type=int, default=1, help="replay every n-th order in the all-orders check (busy days)")
    a = p.parse_args(argv)
    if a.cmd == "decompress":
        print(decompress(a.file))
    else:
        report = check(a.file, a.replay, a.levels, a.sample, [s for s in a.symbols.split(",") if s], a.every)
        print(json.dumps({k: report[k] for k in ("check", "cross_check")}, indent=1))
        print(json.dumps({k: report["replay"][k] for k in ("all_orders", "filled_sample", "filled_sample_counterfactual")}, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
