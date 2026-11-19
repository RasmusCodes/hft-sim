"""Run one market in-process and write it as Parquet (needs pyarrow and the native extension).

    python -m hftsim.export DIRECTORY [hftsim run flags ...]
    python -m hftsim.export runs/demo --seed 1 --secs 60 --mm 7 --informed 10

Writes DIRECTORY/events.parquet, book.parquet, owners.parquet and report.json (the same report as
`hftsim run` prints for these flags; `hftsim.arrow` describes the tables).
"""

from __future__ import annotations

import sys
import time

import pyarrow.parquet as pq

from .arrow import write_parquet_args


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    if not argv or argv[0].startswith("-"):
        print(__doc__)
        return 2
    started = time.perf_counter()
    out = write_parquet_args(argv[1:], argv[0])
    for name in ("events", "book", "owners"):
        path = out / f"{name}.parquet"
        print(f"{path}: {pq.read_metadata(path).num_rows:,} rows, {path.stat().st_size / 1e6:.1f} MB")
    print(f"{out / 'report.json'} ({time.perf_counter() - started:.1f} s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
