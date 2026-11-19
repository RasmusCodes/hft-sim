import math
import unittest

from hftsim import stats


class TestStats(unittest.TestCase):
    def test_t_quantiles_match_tables(self):
        # Standard two-sided 95% and 99% critical values.
        for q, df, want in [(0.975, 1, 12.7062047), (0.975, 2, 4.3026527), (0.975, 7, 2.3646243), (0.975, 30, 2.0422725), (0.995, 10, 3.1692727)]:
            self.assertAlmostEqual(stats.t_ppf(q, df), want, places=6, msg=f"t({q}, {df})")
        self.assertAlmostEqual(stats.t_cdf(0.0, 5), 0.5, places=15)
        self.assertAlmostEqual(stats.t_ppf(0.025, 9), -stats.t_ppf(0.975, 9), places=10)

    def test_mean_interval_by_hand(self):
        m, lo, hi = stats.mean_ci([1, 2, 3, 4, 5])
        half = 2.7764451 * math.sqrt(2.5) / math.sqrt(5)
        self.assertAlmostEqual(m, 3.0)
        self.assertAlmostEqual(lo, 3.0 - half, places=6)
        self.assertAlmostEqual(hi, 3.0 + half, places=6)
        self.assertTrue(math.isnan(stats.mean_ci([1.0])[1]))

    def test_paired_differences(self):
        m, lo, hi = stats.paired_ci([1, 2, 3], [0, 1, 2])
        self.assertEqual((m, lo, hi), (1.0, 1.0, 1.0))
        with self.assertRaises(ValueError):
            stats.paired_ci([1], [1, 2])

    def test_wilson_interval(self):
        p, lo, hi = stats.wilson_ci(0, 10)
        self.assertEqual((p, lo), (0.0, 0.0))
        self.assertAlmostEqual(hi, 3.8414588 / 13.8414588, places=6)
        p, lo, hi = stats.wilson_ci(50, 100)
        self.assertAlmostEqual(lo + hi, 1.0, places=12)

    def test_bootstrap_is_seeded(self):
        xs = [0.3, 1.2, -0.4, 2.2, 0.9, 1.1, -1.0, 0.0]
        a, b = stats.bootstrap_ci(xs, seed=3), stats.bootstrap_ci(xs, seed=3)
        self.assertEqual(a, b)
        self.assertTrue(a[1] <= a[0] <= a[2])


if __name__ == "__main__":
    unittest.main()
