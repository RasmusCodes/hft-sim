"""Integration tests of the Python layer against the real `hftsim` binary (short runs)."""

import unittest

from hftsim import Scenario, decomposition, market_makers, run, run_args, run_many, verify

TINY = Scenario(secs=4, warmup_secs=1)


class TestRunner(unittest.TestCase):
    def test_scenario_defaults_equal_the_binarys(self):
        # All flags passed explicitly from Python's defaults must give exactly the run the binary
        # makes from its own defaults.
        explicit = run(TINY, 9, cache=False)
        implicit = run_args(["--seed", "9", "--secs", "4", "--warmup-secs", "1"], cache=False)
        self.assertEqual(explicit["manifest"]["digest"], implicit["manifest"]["digest"])

    def test_repeated_market_maker_flags_and_results(self):
        s = TINY.with_(makers=(3, 10), informed=2, maker_fee=-20, taker_fee=30)
        args = s.args(5)
        self.assertEqual([args[k + 1] for k, a in enumerate(args) if a == "--mm"], ["3", "10"])
        r = run(s, 5, cache=False)
        mms = market_makers(r)
        self.assertEqual([m["label"].split(":")[0] for m in mms], ["step 3", "step 10"])
        d = decomposition(mms[0], 1000)
        self.assertAlmostEqual(d["spread_capture"] + d["adverse_selection"] + d["inventory"] + d["carried"] - d["fees"], d["total"], places=9)
        self.assertAlmostEqual(d["total"], mms[0]["pnl_dollars"], places=9)
        self.assertGreater(mms[0]["volume"], 0)
        self.assertLessEqual(mms[0]["cancels_too_late"], mms[0]["cancel_attempts"])

    def test_manifest_reproduces_and_grids_keep_order(self):
        results = run_many([(TINY, 1), (TINY, 2), (TINY.with_(makers=(5,)), 1)], workers=3, progress=False)
        self.assertEqual([r["manifest"]["seed"] for r in results], [1, 2, 1])
        self.assertNotEqual(results[0]["manifest"]["digest"], results[1]["manifest"]["digest"])
        self.assertTrue(verify(results[2]))


if __name__ == "__main__":
    unittest.main()
