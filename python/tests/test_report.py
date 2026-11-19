import math
import tempfile
import unittest
from pathlib import Path

from hftsim import report
from hftsim.research import write_index


class TestReport(unittest.TestCase):
    def test_interval_and_table_formatting(self):
        self.assertEqual(report.ci(1.0, 0.5, 1.5), "1.00 [0.50, 1.50]")
        self.assertEqual(report.ci(1.0, math.nan, math.nan, 1), "1.0")
        self.assertEqual(report.table(["a", "b"], [[1, "x"]]), "| a | b |\n|---|---|\n| 1 | x |")

    def test_index_lists_reports_in_order_with_the_disclaimer(self):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d)
            (out / "rq2.md").write_text("# RQ2: two\n", encoding="utf-8")
            (out / "rq1.md").write_text("# RQ1: one\n", encoding="utf-8")
            text = write_index(out).read_text(encoding="utf-8")
            self.assertLess(text.index("RQ1: one"), text.index("RQ2: two"))
            self.assertIn("Simulation evidence only", text)
            self.assertIn("(rq1.manifests.json)", text)


if __name__ == "__main__":
    unittest.main()
