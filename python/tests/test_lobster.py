"""The historical-data commands through Python (short runs against the real binary)."""

import tempfile
import unittest
from pathlib import Path

from hftsim import Scenario
from hftsim.lobster import check, export, replay_check

TINY = Scenario(secs=6, warmup_secs=1, makers=(7,), informed=2)


class TestLobster(unittest.TestCase):
    def test_export_then_check_matches_every_row(self):
        with tempfile.TemporaryDirectory() as d:
            out = export(TINY, 3, Path(d) / "day", levels=5)
            self.assertTrue(Path(out["message"]).name.startswith("HFTSIM_2000-01-01_34200000_34206000_message_5"))
            report = check(out["message"], out["orderbook"])
            v = report["validation"]
            self.assertEqual((v["levels"], v["rows"], v["matched"], v["unexplained_rows"]), (5, out["rows"], out["rows"], 0))
            self.assertEqual(v["messages_by_type"][3], out["manifest"]["trades"], "one visible execution per trade")
            self.assertEqual(report["provenance"]["symbol"], "HFTSIM")

    def test_replay_check_is_never_optimistic(self):
        r = replay_check(TINY.with_(secs=20), 5, probes=4, interval_ms=200, life_ms=2000)
        self.assertGreater(r["probes"], 50)
        self.assertEqual((r["model_only"], r["model_earlier"], r["truth_only_visible"]), (0, 0, 0))


if __name__ == "__main__":
    unittest.main()
