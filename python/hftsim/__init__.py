"""hftsim: research layer over the hft-sim market simulator.

Configure a market as a `Scenario`, run it with `run` or a grid with `run_many` (one process per
run, common random numbers through shared seeds), check reproducibility with `verify`, and
summarise with `stats` and `report`. `hftsim.research` holds the eight research-question
experiments. All of this needs only the standard library and the `hftsim` binary.

Optional: the compiled extension `hftsim._native` (`maturin develop --release` in python/) runs
the same markets in-process (`use_native=True`), and with pyarrow `hftsim.arrow` returns a run's
event log, top of book and participants as tables or Parquet files (`python -m hftsim.export`).
"""

from .runner import Scenario, binary, build, decomposition, ensure_built, market_makers, native, run, run_args, run_many, verify

__all__ = ["Scenario", "binary", "build", "decomposition", "ensure_built", "market_makers", "native", "run", "run_args", "run_many", "verify"]
