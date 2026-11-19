"""The native bindings and the Arrow/Parquet export. Skipped unless pyarrow is installed and the
extension is built (`maturin develop --release` in python/, inside the virtual environment)."""

import concurrent.futures as cf
import tempfile
import unittest
from pathlib import Path

from hftsim import Scenario, run_args
from hftsim.runner import native

try:
    import pyarrow.parquet as pq

    from hftsim.arrow import run_tables, write_parquet
except ImportError:
    pq = None

TINY = Scenario(secs=4, warmup_secs=1)
S = 1_000_000_000


@unittest.skipIf(native() is None or pq is None, "needs the native extension and pyarrow")
class TestNative(unittest.TestCase):
    def test_native_runs_reproduce_the_binary(self):
        args = TINY.with_(makers=(7,), informed=3, arbs=1, maker_fee=-20, taker_fee=30).args(5)
        a, b = run_args(args, cache=False), run_args(args, cache=False, use_native=True)
        for r in (a, b):
            r["manifest"].pop("wall_secs")
        self.assertEqual(a, b)
        with self.assertRaises(RuntimeError):
            run_args(["--mm", "11"], cache=False, use_native=True)

    def test_threads_run_native_markets_deterministically(self):
        jobs = [TINY.with_(makers=(5,), informed=2).args(seed) for seed in (1, 2, 3, 1)]
        with cf.ThreadPoolExecutor(max_workers=4) as pool:
            parallel = [r["manifest"]["digest"] for r in pool.map(lambda a: run_args(a, cache=False, use_native=True), jobs)]
        serial = [run_args(a, cache=False, use_native=True)["manifest"]["digest"] for a in jobs]
        self.assertEqual(parallel, serial)
        self.assertEqual(parallel[0], parallel[3])
        self.assertEqual(len(set(parallel[:3])), 3)

    def test_tables_agree_with_the_report(self):
        s = TINY.with_(makers=(5,), informed=2)
        report, t = run_tables(s, 9)
        ev = t["events"].to_pydict()
        n = len(ev["seq"])
        self.assertEqual(ev["seq"], list(range(1, n + 1)), "one row per event, in order")
        self.assertTrue(all(x <= y for x, y in zip(ev["ts_ns"], ev["ts_ns"][1:])))
        kinds = ev["kind"]
        trades = kinds.count("trade")
        self.assertEqual(trades, report["manifest"]["trades"])
        self.assertEqual(kinds.count("fill"), 2 * trades)
        # The market maker's fills, rebuilt from the event table, give the report's position and
        # its window fill count.
        mm = [a for a in report["agents"] if a["kind"] == "market_maker"][0]
        fills = [k for k in range(n) if kinds[k] == "fill" and ev["owner"][k] == mm["id"]]
        position = sum(ev["qty"][k] * (1 if ev["side"][k] == "buy" else -1) for k in fills)
        self.assertEqual(position, mm["position"])
        in_window = [k for k in fills if s.warmup_secs * S < ev["ts_ns"][k] <= s.secs * S]
        self.assertEqual(len(in_window), mm["fills"])
        self.assertTrue(all(ev["liquidity"][k] == "maker" for k in fills), "the market maker only provides liquidity")
        # The last top of book is the report's final mid.
        book = t["book"].to_pydict()
        self.assertAlmostEqual((book["bid"][-1] + book["ask"][-1]) / 200, report["market"]["final_mid"])
        owners = t["owners"].to_pydict()
        self.assertEqual(owners["role"].count("informed"), 2)
        self.assertEqual(owners["owner"][owners["role"].index("market_maker")], mm["id"])

    def test_parquet_round_trip(self):
        s = TINY.with_(makers=(7,), informed=2)
        _, tables = run_tables(s, 4)
        with tempfile.TemporaryDirectory() as d:
            out = write_parquet(s, 4, Path(d) / "run")
            for name, table in tables.items():
                back = pq.read_table(out / f"{name}.parquet")
                self.assertTrue(back.equals(table), name)
            self.assertTrue((out / "report.json").exists())


if __name__ == "__main__":
    unittest.main()
