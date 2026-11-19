"""The ITCH wrapper: decompression with its CRC check, and `hftsim itch-check` end to end on a
hand-built day (no network)."""

import gzip
import struct
import tempfile
import unittest
from pathlib import Path

from hftsim.itch import check, decompress


def header(kind: bytes, locate: int, ts: int) -> bytes:
    return kind + struct.pack(">HH", locate, 0) + ts.to_bytes(6, "big")


def directory(locate: int, symbol: str) -> bytes:
    # stock, market category, financial status, round lot, round lots only, classification,
    # sub-type, authenticity, short-sale threshold, IPO flag, LULD tier, ETP flag, leverage, inverse
    body = symbol.ljust(8).encode() + b"QN" + struct.pack(">I", 100) + b"NCZ PN 1N" + struct.pack(">I", 0) + b"N"
    return header(b"R", locate, 1) + body


def add(locate: int, ts: int, oref: int, side: bytes, shares: int, symbol: str, price: int) -> bytes:
    return header(b"A", locate, ts) + struct.pack(">Q", oref) + side + struct.pack(">I", shares) + symbol.ljust(8).encode() + struct.pack(">I", price)


def execute(locate: int, ts: int, oref: int, shares: int, match: int) -> bytes:
    return header(b"E", locate, ts) + struct.pack(">QIQ", oref, shares, match)


def delete(locate: int, ts: int, oref: int) -> bytes:
    return header(b"D", locate, ts) + struct.pack(">Q", oref)


class TestItch(unittest.TestCase):
    def test_a_day_checks_and_replays(self):
        # Bids A (1) then B (2) at $10.00, an ask C (3) at $10.01. A seller takes all of A and 30
        # of B in one go; B's rest and C are deleted later.
        msgs = [
            header(b"S", 0, 5) + b"Q",
            directory(1, "TEST"),
            add(1, 10, 1, b"B", 100, "TEST", 100_000),
            add(1, 20, 2, b"B", 100, "TEST", 100_000),
            add(1, 30, 3, b"S", 50, "TEST", 100_100),
            execute(1, 40, 1, 100, 1),
            execute(1, 40, 2, 30, 2),
            delete(1, 50, 2),
            delete(1, 60, 3),
        ]
        self.assertEqual(len(msgs[1]), 39, "the stock directory layout")
        with tempfile.TemporaryDirectory() as d:
            gz = Path(d) / "20191230.TEST_ITCH_50.gz"
            with gzip.open(gz, "wb") as f:
                for m in msgs:
                    f.write(struct.pack(">H", len(m)) + m)
            raw = decompress(gz)
            self.assertEqual(raw.name, "20191230.TEST_ITCH_50")
            self.assertEqual(decompress(gz), raw, "a second call reuses the file")
            report = check(gz, replay=1, sample=10)
            c = report["check"]
            self.assertEqual((c["messages"], c["unknown_refs"], c["oversize"], c["length_mismatches"], c["live_at_end"]), (9, 0, 0, {}, 0))
            self.assertEqual((c["top_stocks"][0]["symbol"], c["top_stocks"][0]["executions"]), ("TEST", 2))
            self.assertEqual((report["cross_check"]["stocks"], report["cross_check"]["mismatches"]), (["TEST"], 0))
            # A's copy sees B, behind it, trade in the same nanosecond; B's own fill is invisible
            # to its copy (the seller stopped at B); C never traded.
            r = report["replay"]["all_orders"]
            self.assertEqual(r["filled_by"], {"both": 1, "exchange_only": 1, "model_only": 0, "neither": 1})
            self.assertEqual(r["first_fill"]["same_stamp"], 1)
            self.assertTrue((Path(d) / "derived" / "itch_check.json").exists())

    def test_a_corrupt_download_fails_its_crc(self):
        with tempfile.TemporaryDirectory() as d:
            gz = Path(d) / "bad.gz"
            data = bytearray(gzip.compress(b"x" * 1000))
            data[-8] ^= 0xFF  # the CRC-32 in the trailer
            gz.write_bytes(bytes(data))
            with self.assertRaises(OSError):
                decompress(gz)


if __name__ == "__main__":
    unittest.main()
