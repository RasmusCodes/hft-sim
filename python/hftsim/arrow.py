"""Arrow tables and Parquet files of one run, through the native bindings.

Needs pyarrow and the compiled extension: `maturin develop --release` in `python/`, inside a
virtual environment. The run is the `hftsim` binary's run for the same flags (the same code, so
the same digest).

    from hftsim import Scenario
    from hftsim.arrow import run_tables, write_parquet

    s = Scenario(secs=60, warmup_secs=10, makers=(7,), informed=10)
    report, tables = run_tables(s, seed=1)       # report: the JSON summary; tables: pyarrow
    tables["events"].num_rows
    write_parquet(s, 1, "runs/example")          # events/book/owners .parquet + report.json

Tables:
* `events`: the engine's event log, one row per event, wide (columns that do not apply to a row's
  kind are null): seq, ts_ns, kind, order_id, owner, side, price (cents), qty, leaves, liquidity,
  trade_id, aggressor, buy_order_id, sell_order_id, client_tag, tif, reason, request,
  priority_kept, phase. `side` is filled in for every event about one order.
* `book`: the top of book after every public-event time: ts_ns, bid, bid_qty, ask, ask_qty.
* `owners`: every participant: owner, role, label.
"""

from __future__ import annotations

import json
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from . import _native
from .runner import Scenario


def _array(c: dict) -> pa.Array:
    n = c["length"]
    validity = None if c["validity"] is None else pa.py_buffer(c["validity"])
    values = pa.py_buffer(c["values"])
    if c["type"] == "int64":
        return pa.Array.from_buffers(pa.int64(), n, [validity, values])
    if c["type"] == "bool":
        return pa.Array.from_buffers(pa.bool_(), n, [validity, values])
    if c["type"] == "dictionary":
        codes = pa.Array.from_buffers(pa.int8(), n, [validity, values])
        return pa.DictionaryArray.from_arrays(codes, pa.array(c["names"], pa.string()))
    raise ValueError(f"unknown column type {c['type']!r}")


def _table(columns: list[dict]) -> pa.Table:
    return pa.table({c["name"]: _array(c) for c in columns})


def _owners(rows: list[tuple[int, str, str]]) -> pa.Table:
    return pa.table({
        "owner": pa.array([r[0] for r in rows], pa.int64()),
        "role": pa.array([r[1] for r in rows], pa.string()).dictionary_encode(),
        "label": pa.array([r[2] for r in rows], pa.string()),
    })


def run_native_args(args: list[str]) -> dict:
    """Run `hftsim run` flags in-process and return the JSON report."""
    text, _ = _native.run(list(args))
    return json.loads(text)


def run_tables_args(args: list[str]) -> tuple[dict, dict[str, pa.Table]]:
    """Run `hftsim run` flags in-process; return the report and the events/book/owners tables."""
    text, t = _native.run(list(args), tables=True)
    return json.loads(text), {"events": _table(t["events"]), "book": _table(t["book"]), "owners": _owners(t["owners"])}


def run_tables(scenario: Scenario, seed: int) -> tuple[dict, dict[str, pa.Table]]:
    """One scenario and seed: the report and its tables."""
    return run_tables_args(scenario.args(seed))


def write_parquet_args(args: list[str], directory: str | Path, compression: str = "zstd") -> Path:
    """Run flags and write events.parquet, book.parquet, owners.parquet and report.json."""
    report, tables = run_tables_args(args)
    out = Path(directory)
    out.mkdir(parents=True, exist_ok=True)
    for name, table in tables.items():
        pq.write_table(table, out / f"{name}.parquet", compression=compression)
    (out / "report.json").write_text(json.dumps(report, indent=1) + "\n", encoding="utf-8")
    return out


def write_parquet(scenario: Scenario, seed: int, directory: str | Path, compression: str = "zstd") -> Path:
    """One scenario and seed, written as Parquet (see `write_parquet_args`)."""
    return write_parquet_args(scenario.args(seed), directory, compression)
