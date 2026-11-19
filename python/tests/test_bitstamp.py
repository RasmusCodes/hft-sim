"""The Bitstamp recorder's WebSocket framing, the converter and the order-level check, on synthetic
data (no network)."""

import json
import socket
import struct
import tempfile
import unittest
from pathlib import Path

from hftsim.bitstamp import WebSocket, _scaled, check, convert, crossings, power, replay, to_lobster


def server_frame(opcode: int, payload: bytes, fin: bool = True) -> bytes:
    n = len(payload)
    b0 = (0x80 if fin else 0) | opcode
    if n < 126:
        return struct.pack("!BB", b0, n) + payload
    if n < 1 << 16:
        return struct.pack("!BBH", b0, 126, n) + payload
    return struct.pack("!BBQ", b0, 127, n) + payload


class TestFraming(unittest.TestCase):
    def setUp(self):
        self.client, self.server = socket.socketpair()
        self.ws = object.__new__(WebSocket)
        self.ws.sock, self.ws.buf = self.client, b""

    def tearDown(self):
        self.client.close()
        self.server.close()

    def test_text_long_and_fragmented_messages_and_pings(self):
        long = "x" * 300
        self.server.sendall(server_frame(0x1, b"hello") + server_frame(0x1, long.encode()) + server_frame(0x9, b"hb") + server_frame(0x1, b"frag", fin=False) + server_frame(0x0, b"ment"))
        self.assertEqual(self.ws.recv_text(), "hello")
        self.assertEqual(self.ws.recv_text(), long)
        self.assertEqual(self.ws.recv_text(), "fragment", "the ping in between was answered, not returned")
        # The client answered the ping with a masked pong carrying the same payload.
        head = self.server.recv(2)
        self.assertEqual((head[0], head[1] & 0x80, head[1] & 0x7F), (0x8A, 0x80, 2))
        mask = self.server.recv(4)
        self.assertEqual(bytes(b ^ mask[k % 4] for k, b in enumerate(self.server.recv(2))), b"hb")

    def test_client_frames_are_masked_and_close_raises(self):
        self.ws.send_text("hi")
        data = self.server.recv(64)
        self.assertEqual((data[0], data[1]), (0x81, 0x80 | 2))
        mask = data[2:6]
        self.assertEqual(bytes(b ^ mask[k % 4] for k, b in enumerate(data[6:8])), b"hi")
        self.server.sendall(server_frame(0x8, struct.pack("!H", 1000) + b"bye"))
        with self.assertRaises(ConnectionError):
            self.ws.recv_text()


def line(recv: int, msg: dict) -> str:
    return f"{recv}\t{json.dumps(msg)}\n"


def order(event: str, oid: int, side: int, price: str, amount: str, ts: int, eid: str, pre: str, source: str = "orderbook") -> dict:
    return {"event": f"order_{event}", "channel": "live_orders_btcusd", "event_id": eid, "pre_event_id": pre, "order_source": source,
            "data": {"id": oid, "order_type": side, "order_subtype": 0, "price_str": price, "amount_str": amount, "microtimestamp": str(ts)}}


class TestConvertAndCheck(unittest.TestCase):
    def test_units_are_exact(self):
        self.assertEqual((_scaled("64529.73", 2), _scaled("0.07748366", 8), _scaled("1", 8)), (6452973, 7748366, 100000000))
        with self.assertRaises(ValueError):
            _scaled("0.123456789", 8)

    def test_a_small_recording_converts_and_checks(self):
        with tempfile.TemporaryDirectory() as d:
            run = Path(d) / "run"
            run.mkdir()
            # Unknown bid 7 rests before the recording. Bid 10 and ask 11 are created; 10 is partly
            # filled; a stop order (left out) appears; 7 is deleted; an event id is skipped (gap).
            orders = [
                order("created", 10, 0, "101.00", "0.4", 1, "e1", "e0"),
                order("created", 11, 1, "103.00", "0.6", 2, "e2", "e1"),
                order("created", 99, 1, "90.00", "1.0", 2, "s1", "e2", source="stop_order"),
                order("changed", 10, 0, "101.00", "0.1", 4, "e3", "e2"),
                order("deleted", 7, 0, "100.00", "0.5", 5, "e5", "e4"),
            ]
            (run / "orders.jsonl").write_text("".join(line(0, o) for o in orders), encoding="utf-8")
            snap = lambda ts, bids, asks: {"event": "data", "channel": "detail_order_book_btcusd", "data": {"microtimestamp": str(ts), "bids": bids, "asks": asks}}
            snaps = [snap(3, [["101.00", "0.40000000", "10"], ["100.00", "0.50000000", "7"]], [["103.00", "0.60000000", "11"]]), snap(5, [["101.00", "0.10000000", "10"]], [["103.00", "0.60000000", "11"]])]
            (run / "snapshots.jsonl").write_text("".join(line(0, s) for s in snaps), encoding="utf-8")
            (run / "manifest.json").write_text(json.dumps({"market": "btcusd", "started_utc": "2026-11-18T00:00:00+00:00"}), encoding="utf-8")
            summary = convert(run)
            self.assertEqual((summary["events"], summary["stop_order_events_left_out"], summary["event_id_gaps"], summary["snapshots"]), (4, 1, 1, 2))
            rows = (run / "derived" / "events.csv").read_text().splitlines()
            self.assertEqual(rows[1], "1,created,10,buy,10100,40000000")
            self.assertEqual(rows[-1], "5,deleted,7,buy,10000,0")
            v = check(run)["validation"]
            self.assertEqual((v["snapshots"], v["exact"], v["explained"], v["unexplained"], v["revealed_orders"]), (2, 1, 1, 0, 1))
            self.assertTrue((run / "derived" / "check.json").exists())

    def test_the_power_check_catches_each_break(self):
        with tempfile.TemporaryDirectory() as d:
            run = Path(d) / "run"
            run.mkdir()
            # Ask 5 rests all along; bid 10 lives from 0.5 s to 7.5 s; snapshots every second.
            s = 1_000_000
            orders = [
                order("created", 5, 1, "103.00", "0.6", 400_000, "e1", "e0"),
                order("created", 10, 0, "101.00", "0.4", 500_000, "e2", "e1"),
                order("deleted", 10, 0, "101.00", "0.4", 7_500_000, "e3", "e2"),
            ]
            (run / "orders.jsonl").write_text("".join(line(0, o) for o in orders), encoding="utf-8")
            snap = lambda ts, bids: {"event": "data", "channel": "detail_order_book_btcusd", "data": {"microtimestamp": str(ts), "bids": bids, "asks": [["103.00", "0.60000000", "5"]]}}
            bid = [["101.00", "0.40000000", "10"]]
            (run / "snapshots.jsonl").write_text("".join(line(0, snap(k * s, bid if k < 8 else [])) for k in range(1, 9)), encoding="utf-8")
            report = power(run)
            self.assertEqual((report["target_order"], report["middle_snapshot"]), (10, 5))
            cases = report["cases"]
            self.assertTrue(cases["unchanged"]["clean"])
            for name in ("dropped_deletion", "amount_plus_one_unit", "price_plus_one_tick", "creation_one_second_late", "creation_missing"):
                self.assertTrue(cases[name]["caught"], name)
            self.assertEqual((cases["creation_missing"]["unexplained"], cases["creation_missing"]["revealed_above_first_created_id"]), (0, 1),
                             "a missing creation looks like a pre-recording order; only its id gives it away")
            self.assertEqual(cases["dropped_deletion"]["unexplained_differences"]["extra"], 1)
            self.assertTrue((run / "derived" / "power.json").exists())

    def test_crossings_are_classified_by_subtype_and_fate(self):
        with tempfile.TemporaryDirectory() as d:
            run = Path(d) / "run"
            run.mkdir()

            def sub(o: dict, subtype: int) -> dict:
                o["data"]["order_subtype"] = subtype
                return o

            orders = [
                order("created", 10, 0, "101.00", "0.4", 1_000, "e1", "e0"),
                # A MOC sell at 100 would cross: published, then deleted in the same ms, untraded.
                sub(order("created", 20, 1, "100.00", "0.1", 5_000, "e2", "e1"), 5),
                sub(order("deleted", 20, 1, "100.00", "0.1", 5_000, "e3", "e2"), 5),
                # An IOC sell at 100 trades with bid 10.
                sub(order("created", 21, 1, "100.00", "0.1", 8_000, "e4", "e3"), 4),
                order("changed", 10, 0, "101.00", "0.3", 8_000, "e5", "e4"),
                sub(order("deleted", 21, 1, "100.00", "0.1", 8_000, "e6", "e5"), 4),
            ]
            (run / "orders.jsonl").write_text("".join(line(0, o) for o in orders), encoding="utf-8")
            snap = {"event": "data", "channel": "detail_order_book_btcusd", "data": {"microtimestamp": "2000", "bids": [["101.00", "0.40000000", "10"]], "asks": []}}
            (run / "snapshots.jsonl").write_text(line(0, snap), encoding="utf-8")
            trade = {"event": "trade", "channel": "live_trades_btcusd", "data": {"buy_order_id": 10, "sell_order_id": 21, "microtimestamp": "8000"}}
            (run / "trades.jsonl").write_text(line(0, trade), encoding="utf-8")
            s = crossings(run)
            self.assertEqual((s["episodes"], s["trades_recorded"], s["event_order_is_the_crossing_top"], s["by_event"]), (2, 1, 2, {"created": 2}))
            self.assertEqual(s["by_subtype"], {"4 IOC": {"traded": 1}, "5 MOC": {"deleted untraded, same ms": 1}})

    def test_the_recording_becomes_lobster_messages_and_replays(self):
        with tempfile.TemporaryDirectory() as d:
            run = Path(d) / "run"
            run.mkdir()

            def o(event: str, oid: int, side: int, price: str, amount: str, ts: int, traded: str = "0") -> dict:
                msg = order(event, oid, side, price, amount, ts, "e", "e")
                msg["data"]["amount_traded"] = traded
                return msg

            orders = [
                o("created", 10, 0, "101.00", "0.4", 2000),               # M: bid, partly filled, then cancelled
                o("created", 50, 1, "101.20", "0.2", 3000),               # A: ask, filled by T2
                o("created", 20, 1, "100.50", "0.1", 5000),               # T1: IOC sell, takes 0.1 from M
                o("changed", 10, 0, "101.00", "0.3", 5000, "0.1"),
                o("deleted", 20, 1, "101.00", "0", 5000, "0.1"),
                o("created", 30, 1, "100.00", "0.2", 6000),               # R: MOC that would cross, rejected
                o("deleted", 30, 1, "100.00", "0.2", 6000),
                o("created", 40, 0, "101.50", "0.5", 7000),               # T2: buys A's 0.2, rests 0.3
                o("deleted", 50, 1, "101.20", "0", 7000, "0.2"),
                o("changed", 40, 0, "101.20", "0.3", 7000, "0.2"),
                o("deleted", 10, 0, "101.00", "0.3", 9000),
                o("deleted", 40, 0, "101.50", "0.3", 9500),
            ]
            (run / "orders.jsonl").write_text("".join(line(0, x) for x in orders), encoding="utf-8")
            snap = {"event": "data", "channel": "detail_order_book_btcusd", "data": {"microtimestamp": "1000", "bids": [["100.00", "0.50000000", "7"]], "asks": []}}
            (run / "snapshots.jsonl").write_text(line(0, snap), encoding="utf-8")
            trade = lambda buy, sell, ty, amount, price, ts: {"event": "trade", "channel": "live_trades_btcusd", "data": {"buy_order_id": buy, "sell_order_id": sell, "type": ty, "amount_str": amount, "price_str": price, "microtimestamp": str(ts)}}
            (run / "trades.jsonl").write_text(line(0, trade(10, 20, 1, "0.1", "101.00", 5000)) + line(0, trade(40, 50, 0, "0.2", "101.20", 7000)), encoding="utf-8")
            s = to_lobster(run)
            rows = (run / "derived" / "lobster_message.csv").read_text().splitlines()
            self.assertEqual(rows, [
                "0.001999,1,7,50000000,1000000,1",     # opening book, just before the first event
                "0.002000,1,10,40000000,1010000,1",
                "0.003000,1,50,20000000,1012000,-1",
                "0.005000,4,10,10000000,1010000,1",    # T1's trade as M's execution; T1 itself never rests
                "0.006000,1,30,20000000,1000000,-1",   # the rejected MOC appears and goes in its stamp
                "0.006000,3,30,20000000,1000000,-1",
                "0.007000,4,50,20000000,1012000,-1",   # A filled in full: no deletion
                "0.007000,1,40,30000000,1015000,1",    # T2's remainder rests at its limit, end of its stamp
                "0.009000,3,10,30000000,1010000,1",
                "0.009500,3,40,30000000,1015000,1",
            ])
            self.assertEqual((s["takers"], s["taker_remainders_submitted"], s["opening_book_orders"], s["from_seconds"]), (2, 1, 1, "0.002000"))
            self.assertEqual((s["maker_volume_from_orders"], s["maker_volume_from_trades"], s["makers_not_matching_their_trades"]), (30000000, 30000000, 0))
            # One fill row per execution row, naming its taker; T1 filled, T2 rested 0.3 with no
            # ask left within its 101.50 limit, so that 0.3 is available to the rule.
            self.assertEqual((run / "derived" / "lobster_fills.csv").read_text().splitlines()[1:], ["0.005000,10,20,10000000,1010000", "0.007000,50,40,20000000,1012000"])
            self.assertEqual((run / "derived" / "lobster_takers.csv").read_text().splitlines()[1:], [
                "0.005000,20,-1,1005000,10000000,0,10000000,0,filled,0",
                "0.007000,40,1,1015000,50000000,0,20000000,30000000,rested,30000000",
            ])
            self.assertEqual(s["taker_remainders"], {"volume": 30000000, "premise": {"no remainder": 1, "ok": 1}, "available_volume": 30000000, "mismatches_with_trades": 0})
            report = replay(run)
            r, var = report["replay"], report["variants"]
            # M, A and T2's remainder are compared (R went within its stamp; 7 is the opening book).
            # The takers' volume went to M and A alone, so their copies see nothing: the blind spot.
            self.assertEqual((r["orders"], r["left_out"]["gone_within_entry_stamp"], r["left_out"]["before_from"]), (3, 1, 1))
            self.assertEqual(r["filled_by"], {"both": 0, "exchange_only": 2, "model_only": 0, "neither": 1})
            # The rule: T2's real 0.3 left over reaches A's copy (an ask within its limit) at once.
            self.assertEqual((var["taker_rule"]["filled_by"]["both"], var["taker_rule"]["first_fill"]["same_stamp"]), (1, 1))
            # History without M and A: no orders behind them or deeper within the takers' limits,
            # so their volume stays with the takers; only the rule passes it on.
            self.assertEqual(var["counterfactual"]["rematched"], {"behind": 0, "deeper": 0, "leftover": 30000000, "unattributed": 0})
            self.assertEqual(var["counterfactual"]["filled_by"]["exchange_only"], 2)
            full = var["counterfactual_taker_rule"]
            self.assertEqual((full["filled_by"], full["first_fill"]["same_stamp"], full["overfilled"]), ({"both": 2, "exchange_only": 0, "model_only": 0, "neither": 1}, 2, 0))


if __name__ == "__main__":
    unittest.main()
