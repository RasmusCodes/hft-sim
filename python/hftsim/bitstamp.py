"""Bitstamp public order data: record it, convert it, and check the rebuilt book against the
exchange's own order-level snapshots (phase 5, docs/HISTORICAL_DATA.md §6).

Bitstamp's WebSocket API v2 (wss://ws.bitstamp.net, read in its documentation) publishes, without
an account:
* `live_orders_<market>`: every order's creation, change and deletion (order id, side, price,
  remaining amount, amount traded, a timestamp in microseconds, which in our recording is always a
  whole millisecond; event ids to detect gaps);
* `detail_order_book_<market>`: the best 100 orders on each side, with ids, after every change
  (true microsecond timestamps);
* `live_trades_<market>`: trades with both orders' ids.

    python -I -m hftsim.bitstamp record --minutes 30 --cap-mb 200 --out ../data/bitstamp/run1
    python -I -m hftsim.bitstamp convert ../data/bitstamp/run1
    python -I -m hftsim.bitstamp check ../data/bitstamp/run1     # rebuild vs snapshots
    python -I -m hftsim.bitstamp power ../data/bitstamp/run1     # would the check catch errors?
    python -I -m hftsim.bitstamp crossings ../data/bitstamp/run1 # what leaves the book crossed?
    python -I -m hftsim.bitstamp replay ../data/bitstamp/run1    # replay fill model vs real fills

Standard library only: the WebSocket client below implements the client side of RFC 6455 over
`ssl`. Recorded lines keep each message exactly as received, prefixed by the receive time.
"""

from __future__ import annotations

import argparse
import base64
import datetime
import hashlib
import json
import os
import socket
import ssl
import struct
import sys
import time
from pathlib import Path

HOST = "ws.bitstamp.net"
GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


class WebSocket:
    """Minimal RFC 6455 client: text frames out (masked), text frames in, pings answered."""

    def __init__(self, host: str = HOST, port: int = 443, path: str = "/", timeout: float = 30.0):
        raw = socket.create_connection((host, port), timeout=timeout)
        self.sock = ssl.create_default_context().wrap_socket(raw, server_hostname=host)
        key = base64.b64encode(os.urandom(16)).decode()
        request = (
            f"GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nUser-Agent: hftsim-research/0.1\r\n\r\n"
        )
        self.sock.sendall(request.encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("connection closed during the handshake")
            buf += chunk
        head, self.buf = buf.split(b"\r\n\r\n", 1)
        lines = head.decode("latin-1").split("\r\n")
        if " 101 " not in lines[0] + " ":
            raise ConnectionError(f"handshake refused: {lines[0]}")
        headers = {k.strip().lower(): v.strip() for k, v in (l.split(":", 1) for l in lines[1:] if ":" in l)}
        expected = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest()).decode()
        if headers.get("sec-websocket-accept") != expected:
            raise ConnectionError("bad Sec-WebSocket-Accept")

    def _read(self, n: int) -> bytes:
        while len(self.buf) < n:
            chunk = self.sock.recv(1 << 16)
            if not chunk:
                raise ConnectionError("connection closed")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def _send(self, opcode: int, payload: bytes) -> None:
        mask = os.urandom(4)
        n = len(payload)
        if n < 126:
            head = struct.pack("!BB", 0x80 | opcode, 0x80 | n)
        elif n < 1 << 16:
            head = struct.pack("!BBH", 0x80 | opcode, 0x80 | 126, n)
        else:
            head = struct.pack("!BBQ", 0x80 | opcode, 0x80 | 127, n)
        self.sock.sendall(head + mask + bytes(b ^ mask[k % 4] for k, b in enumerate(payload)))

    def send_text(self, text: str) -> None:
        self._send(0x1, text.encode())

    def recv_text(self) -> str:
        """Next complete text message (fragments joined; pings answered; close raises)."""
        parts: list[bytes] = []
        while True:
            b0, b1 = self._read(2)
            fin, opcode, n = b0 & 0x80, b0 & 0x0F, b1 & 0x7F
            if n == 126:
                (n,) = struct.unpack("!H", self._read(2))
            elif n == 127:
                (n,) = struct.unpack("!Q", self._read(8))
            mask = self._read(4) if b1 & 0x80 else None
            payload = self._read(n)
            if mask:
                payload = bytes(b ^ mask[k % 4] for k, b in enumerate(payload))
            if opcode == 0x9:
                self._send(0xA, payload)
                continue
            if opcode == 0xA:
                continue
            if opcode == 0x8:
                raise ConnectionError(f"server closed the connection: {payload[2:].decode(errors='replace')}")
            parts.append(payload)
            if fin:
                return b"".join(parts).decode("utf-8")

    def close(self) -> None:
        try:
            self._send(0x8, struct.pack("!H", 1000))
        except OSError:
            pass
        self.sock.close()


def record(out: Path, minutes: float = 30.0, cap_mb: float = 200.0, market: str = "btcusd", snapshot_every_s: float = 1.0, log=print) -> dict:
    """Record the three public channels of `market` into a new, empty directory `out`."""
    out.mkdir(parents=True, exist_ok=False)
    names = ("orders", "trades", "snapshots", "control")
    files = {k: open(out / f"{k}.jsonl", "w", encoding="utf-8", newline="\n") for k in names}
    counts = dict.fromkeys(names, 0)
    channels = [f"live_orders_{market}", f"live_trades_{market}", f"detail_order_book_{market}"]
    started = datetime.datetime.now(datetime.timezone.utc)
    deadline, cap = time.monotonic() + 60 * minutes, int(cap_mb * 1e6)
    written, reconnects, last_bucket, stop = 0, 0, None, "time limit"
    next_report = time.monotonic() + 60
    try:
        while time.monotonic() < deadline:
            try:
                ws = WebSocket()
                for ch in channels:
                    ws.send_text(json.dumps({"event": "bts:subscribe", "data": {"channel": ch}}))
                while time.monotonic() < deadline:
                    text = ws.recv_text()
                    recv_ns = time.time_ns()
                    msg = json.loads(text)
                    channel, event = msg.get("channel", ""), msg.get("event", "")
                    if event == "bts:request_reconnect":
                        raise ConnectionError("server requested a reconnect")
                    if channel.startswith("live_orders_") and event.startswith("order_"):
                        kind = "orders"
                    elif channel.startswith("live_trades_") and event == "trade":
                        kind = "trades"
                    elif channel.startswith("detail_order_book_") and event == "data":
                        bucket = int(int(msg["data"]["microtimestamp"]) / (1e6 * snapshot_every_s))
                        if bucket == last_bucket:
                            continue
                        last_bucket, kind = bucket, "snapshots"
                    else:
                        kind = "control"
                    line = f"{recv_ns}\t{text}\n"
                    if written + len(line.encode()) > cap:
                        stop = "size cap"  # stop before the message that would pass the cap
                        deadline = 0
                        break
                    files[kind].write(line)
                    counts[kind] += 1
                    written += len(line.encode())
                    if time.monotonic() >= next_report:
                        next_report += 60
                        log(f"{datetime.datetime.now():%H:%M:%S} {counts} {written / 1e6:.1f} MB")
                ws.close()
            except (ConnectionError, OSError, ssl.SSLError, ValueError) as e:
                reconnects += 1
                files["control"].write(f"{time.time_ns()}\t{json.dumps({'event': 'hftsim:disconnect', 'reason': str(e)})}\n")
                log(f"disconnected ({e}); reconnecting")
                if reconnects > 20:
                    stop = "too many reconnects"
                    break
                time.sleep(1)
    finally:
        for f in files.values():
            f.close()
    manifest = {
        "source": "Bitstamp WebSocket API v2 (wss://ws.bitstamp.net), public channels",
        "market": market,
        "channels": channels,
        "started_utc": started.isoformat(),
        "ended_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "stopped_by": stop,
        "snapshot_every_s": snapshot_every_s,
        "messages": counts,
        "bytes": written,
        "reconnects": reconnects,
        "format": "each line: receive time (ns since the Unix epoch), a tab, the message exactly as received",
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=1) + "\n", encoding="utf-8")
    return manifest


def _scaled(text: str, places: int) -> int:
    """A decimal string as an exact integer in units of 10^-places (refuses finer digits)."""
    from decimal import Decimal

    value = Decimal(text) * (Decimal(10) ** places)
    if value != value.to_integral_value():
        raise ValueError(f"{text} has more than {places} decimals")
    return int(value)


PRICE_PLACES, AMOUNT_PLACES = 2, 8  # USD cents and satoshis for btcusd
CONVERT_VERSION = 2  # bump when convert() output changes; check() then converts again


def _lines(path: Path):
    with open(path, encoding="utf-8") as f:
        for line in f:
            if not line.endswith("\n"):
                break  # a final line cut off mid-write (the file is still being recorded)
            recv, _, text = line.rstrip("\n").partition("\t")
            if text:
                yield int(recv), json.loads(text)


def convert(run: Path, price_places: int = PRICE_PLACES, amount_places: int = AMOUNT_PLACES) -> dict:
    """Turn a recording into `derived/events.csv` and `derived/snapshots.csv` for `hftsim l3-check`
    (formats in crates/data/src/l3.rs) plus `derived/convert.json`. Stop orders are left out: they
    do not rest in the book until triggered."""
    out = run / "derived"
    out.mkdir(exist_ok=True)
    events, receive_ns, kinds, subtypes = [], [], {}, {}
    gaps = stops = 0
    last_event_id: dict[str, str] = {}  # per order_source: Bitstamp recovers gaps per source
    for recv, msg in _lines(run / "orders.jsonl"):
        source = msg.get("order_source", "orderbook")
        pre = msg.get("pre_event_id")
        if source in last_event_id and pre != last_event_id[source]:
            gaps += 1
        last_event_id[source] = msg.get("event_id")
        if source == "stop_order":
            stops += 1
            continue
        d = msg["data"]
        kind = msg["event"].removeprefix("order_")
        side = "buy" if int(d["order_type"]) == 0 else "sell"
        amount = 0 if kind == "deleted" else _scaled(d["amount_str"], amount_places)
        events.append((int(d["microtimestamp"]), kind, int(d["id"]), side, _scaled(d["price_str"], price_places), amount))
        receive_ns.append(recv)
        kinds[kind] = kinds.get(kind, 0) + 1
        subtypes[str(d.get("order_subtype"))] = subtypes.get(str(d.get("order_subtype")), 0) + 1
    order = sorted(range(len(events)), key=lambda k: events[k][0])
    out_of_order = sum(1 for a, b in zip(order, order[1:]) if b < a)
    # Do order ids rise with time? (Then an order first seen in a snapshot with an id above the
    # first created one was placed during the recording, and its creation event is missing.)
    created_ids = [events[k][2] for k in order if events[k][1] == "created"]
    rising = sum(1 for a, b in zip(created_ids, created_ids[1:]) if b > a)
    # Receive time minus exchange time, per event (local clock offset plus transit), in ms.
    lags = sorted((r - e[0] * 1000) / 1e6 for r, e in zip(receive_ns, events))
    # The event stamps' unit: the largest of 1, 10, 100, 1000 µs dividing every stamp (Bitstamp's
    # order events: whole milliseconds, while its snapshots carry true microseconds).
    stamp = 1
    for unit in (10, 100, 1000):
        if events and all(e[0] % unit == 0 for e in events):
            stamp = unit
    with open(out / "events.csv", "w", encoding="utf-8", newline="\n") as f:
        f.write("ts_us,kind,order_id,side,price,amount\n")
        for k in order:
            f.write(",".join(map(str, events[k])) + "\n")
    snaps = 0
    with open(out / "snapshots.csv", "w", encoding="utf-8", newline="\n") as f:
        f.write("ts_us,snapshot,side,rank,order_id,price,amount\n")
        for recv, msg in _lines(run / "snapshots.jsonl"):
            d = msg["data"]
            snaps += 1
            for side, rows in (("buy", d["bids"]), ("sell", d["asks"])):
                for rank, (price, amount, oid) in enumerate(rows, 1):
                    f.write(f"{int(d['microtimestamp'])},{snaps},{side},{rank},{int(oid)},{_scaled(price, price_places)},{_scaled(amount, amount_places)}\n")
    summary = {
        "version": CONVERT_VERSION,
        "events": len(events),
        "events_by_kind": kinds,
        "order_subtypes": subtypes,
        "stop_order_events_left_out": stops,
        "event_id_gaps": gaps,
        "events_reordered_by_time": out_of_order,
        "event_stamp_us": stamp,
        "created_ids_rising": {"pairs": max(len(created_ids) - 1, 0), "rising": rising,
                               "below_first_created_id": sum(1 for i in created_ids if i < created_ids[0]) if created_ids else 0},
        "receive_minus_exchange_ms": {q: round(lags[round((len(lags) - 1) * f)], 3) for q, f in (("min", 0), ("p1", .01), ("median", .5), ("p99", .99), ("max", 1))} if lags else None,
        "snapshots": snaps,
        "first_event_us": events[order[0]][0] if events else None,
        "last_event_us": events[order[-1]][0] if events else None,
        "units": {"price": f"10^-{price_places}", "amount": f"10^-{amount_places}"},
    }
    (out / "convert.json").write_text(json.dumps(summary, indent=1) + "\n", encoding="utf-8")
    return summary


def check(run: Path, depth: int = 100, tolerance_ms: float = 500.0) -> dict:
    """Convert a recording if needed and run `hftsim l3-check` on it; the report also lands in
    `derived/check.json`. Snapshots that do not match at their own time are searched for within
    ±`tolerance_ms` (the two channels' timing offset)."""
    from .runner import binary, build

    derived = run / "derived"
    raw = [run / n for n in ("orders.jsonl", "snapshots.jsonl") if (run / n).exists()]
    made = [derived / n for n in ("events.csv", "snapshots.csv", "convert.json")]
    if (not all(p.exists() for p in made) or (raw and max(p.stat().st_mtime for p in raw) > min(p.stat().st_mtime for p in made))
            or json.loads(made[2].read_text(encoding="utf-8")).get("version") != CONVERT_VERSION):
        convert(run)  # missing, older than the recording (converted while it was still running), or by an older converter
    manifest = json.loads((run / "manifest.json").read_text(encoding="utf-8")) if (run / "manifest.json").exists() else {}
    extra = [
        "--source", "Bitstamp WebSocket API v2, public channels live_orders and detail_order_book (recorded)",
        "--symbol", manifest.get("market", "btcusd").upper(), "--date", manifest.get("started_utc", "unknown"),
    ]
    for name in ("orders.jsonl", "snapshots.jsonl", "trades.jsonl", "manifest.json"):
        if (run / name).exists():
            extra += ["--also", str(run / name)]
    converted = json.loads((derived / "convert.json").read_text(encoding="utf-8"))
    report = _l3_check(derived / "events.csv", derived / "snapshots.csv", depth, tolerance_ms, converted.get("event_stamp_us", 1), extra)
    report["convert"] = converted
    (derived / "check.json").write_text(json.dumps(report, indent=1) + "\n", encoding="utf-8")
    return report


# order_subtype values, from Bitstamp's WebSocket API v2 documentation (which gives only "MOC" for 5).
SUBTYPES = {0: "limit", 1: "instant", 2: "market", 3: "daily", 4: "IOC", 5: "MOC", 6: "FOK", 7: "cash sell", 8: "GTD"}


def crossings(run: Path) -> dict:
    """Which order events leave the rebuilt book crossed or locked (best bid at or above best ask),
    and what became of those orders: traded (a trade message names them) or deleted untraded. The
    raw order messages are replayed on a book seeded with the first snapshot's orders that the
    stream never created; episodes count from the first snapshot on, like `hftsim l3-check`'s
    `crossed_book`. Writes `derived/crossings.json`."""
    import heapq
    from collections import Counter
    from decimal import Decimal

    orders = [m for _, m in _lines(run / "orders.jsonl") if m.get("order_source", "orderbook") != "stop_order"]
    created = {int(m["data"]["id"]) for m in orders if m["event"] == "order_created"}
    first = next((m["data"] for _, m in _lines(run / "snapshots.jsonl")), None)
    traded = Counter()
    trades = 0
    for _, m in _lines(run / "trades.jsonl"):
        if m.get("event") == "trade":
            trades += 1
            traded.update((int(m["data"]["buy_order_id"]), int(m["data"]["sell_order_id"])))
    book: dict[int, tuple[int, int]] = {}  # id -> (side 0 buy / 1 sell, price in cents)
    heaps: tuple[list, list] = ([], [])

    def put(oid: int, side: int, price: int) -> None:
        book[oid] = (side, price)
        heapq.heappush(heaps[side], (-price if side == 0 else price, oid))

    def best(side: int):
        h = heaps[side]
        while h and book.get(h[0][1]) != (side, -h[0][0] if side == 0 else h[0][0]):
            heapq.heappop(h)  # stale entry
        return (-h[0][0] if side == 0 else h[0][0], h[0][1]) if h else None

    start = int(first["microtimestamp"]) if first else None
    for side, rows in (((0, first["bids"]), (1, first["asks"])) if first else ()):
        for price, _amount, oid in rows:
            if int(oid) not in created:
                put(int(oid), side, _scaled(price, PRICE_PLACES))
    # Later events of each order (kind, time, amount traded) to see what became of it.
    life: dict[int, list[tuple[str, int, Decimal]]] = {}
    for m in orders:
        d = m["data"]
        life.setdefault(int(d["id"]), []).append((m["event"], int(d["microtimestamp"]), Decimal(str(d.get("amount_traded") or 0))))
    episodes = []
    was = False
    for m in orders:
        d = m["data"]
        oid, side, ts = int(d["id"]), int(d["order_type"]), int(d["microtimestamp"])
        if m["event"] == "order_deleted":
            book.pop(oid, None)
        else:
            put(oid, side, _scaled(d["price_str"], PRICE_PLACES))
        b, a = best(0), best(1)
        now = b is not None and a is not None and b[0] >= a[0]
        if now and not was and start is not None and ts >= start:
            episodes.append((m["event"].removeprefix("order_"), oid, int(d.get("order_subtype", -1)), ts, b[0] == a[0], oid in (b[1], a[1])))
        was = now
    by_subtype: dict[str, Counter] = {}
    for _kind, oid, sub, ts, _locked, _top in episodes:
        later = [e for e in life[oid] if e[1] >= ts]
        outcome = "traded" if traded[oid] else ("deleted untraded, same ms" if any(e[0] == "order_deleted" and e[1] == ts for e in later)
                                                and not any(e[2] > 0 for e in later) else "other")
        by_subtype.setdefault(f"{sub} {SUBTYPES.get(sub, '?')}", Counter())[outcome] += 1
    summary = {
        "episodes": len(episodes),
        "by_event": dict(Counter(e[0] for e in episodes)),
        "locked": sum(e[4] for e in episodes),
        "event_order_is_the_crossing_top": sum(e[5] for e in episodes),
        "by_subtype": {k: dict(v) for k, v in sorted(by_subtype.items())},
        "created_by_subtype": {f"{k} {SUBTYPES.get(k, '?')}": n for k, n in sorted(Counter(int(m["data"].get("order_subtype", -1)) for m in orders if m["event"] == "order_created").items())},
        "trades_recorded": trades,
        "note": "book seeded from the first snapshot only; orders revealed later by deeper snapshots are not added",
    }
    (run / "derived").mkdir(exist_ok=True)
    (run / "derived" / "crossings.json").write_text(json.dumps(summary, indent=1) + "\n", encoding="utf-8")
    return summary


def to_lobster(run: Path) -> dict:
    """Write the recording as a LOBSTER message file, `derived/lobster_message.csv`, for the replay
    fill model (`hftsim replay-truth`). As in LOBSTER, an order appears only for what of it rests:
    * a resting order's fill in an event (Bitstamp's `amount_traded` is that event's fill, not a
      running total) is an execution (type 4) at its price. A further cut in its remaining amount
      is a partial cancellation (2), and a deletion that leaves something is a deletion (3);
    * takers, named by each trade's `type` (0 buy, 1 sell), never rest. Their trades appear as the
      makers' executions, and their `changed` events, which carry execution prices, are skipped.
      A remainder still live after the taker's own stamp is submitted at the end of that stamp;
    * orders the snapshots show before the stream mentions them, and which the stream never
      created, were placed before the recording. They are submitted 1 µs before the first event.
    Times are seconds after midnight UTC of the first event's day, prices dollars × 10,000
    (LOBSTER's scale), sizes satoshis. `derived/lobster.json` holds the counts and the checks: each
    trade's volume must appear as exactly one maker execution.

    Each trade becomes its own execution row, and two files describe the takers LOBSTER leaves
    out, for `hftsim replay-truth --takers --fills`:
    * `lobster_fills.csv`: the taker of each execution row, row for row;
    * `lobster_takers.csv`: each taker's arrival, limit, size, fills and unfilled remainder. The
      remainder counts as `available` to the taker-remainder rule only if, once the taker was
      done, no resting order was left within its limit (else it could have traded and did not, so
      the rule's premise fails), and never for a fill-or-kill order, which is killed whole while
      liquidity rests."""
    import heapq
    from collections import Counter, deque

    derived = run / "derived"
    derived.mkdir(exist_ok=True)
    orders = [m for _, m in _lines(run / "orders.jsonl") if m.get("order_source", "orderbook") != "stop_order"]
    trades = [m["data"] for _, m in _lines(run / "trades.jsonl") if m.get("event") == "trade"]
    takers: set[int] = set()
    maker_trades: Counter = Counter()  # maker id -> traded units, from the trades channel
    taker_filled: Counter = Counter()  # taker id -> traded units
    unmatched: dict[int, deque] = {}  # maker id -> its trades (taker, units) not yet written
    for t in trades:
        buy, sell = int(t["buy_order_id"]), int(t["sell_order_id"])
        taker, maker = (buy, sell) if int(t["type"]) == 0 else (sell, buy)
        units = _scaled(t["amount_str"], AMOUNT_PLACES)
        takers.add(taker)
        maker_trades[maker] += units
        taker_filled[taker] += units
        unmatched.setdefault(maker, deque()).append((taker, units))
    created = {int(m["data"]["id"]) for m in orders if m["event"] == "order_created"}
    first_event = {}  # id -> time of the stream's first message about it
    for m in orders:
        first_event.setdefault(int(m["data"]["id"]), int(m["data"]["microtimestamp"]))
    seeds: dict[int, dict] = {}
    for _, m in _lines(run / "snapshots.jsonl"):
        d = m["data"]
        ts = int(d["microtimestamp"])
        for side, rows in ((1, d["bids"]), (-1, d["asks"])):
            for price, amount, oid in rows:
                oid = int(oid)
                if oid not in created and oid not in seeds and ts < first_event.get(oid, ts + 1):
                    seeds[oid] = {"side": side, "price": _scaled(price, 4), "left": _scaled(amount, AMOUNT_PLACES), "taker": False, "resting": True}
    t0 = int(orders[0]["data"]["microtimestamp"])
    midnight = t0 // 86_400_000_000 * 86_400_000_000
    rows: list[tuple[int, int, int, int, int, int]] = []
    fills: list[tuple[int, int, int, int, int]] = []  # (time, maker, taker, units, price) per execution row
    taker_rows: list[tuple] = []
    live: dict[int, list] = {}  # the resting book the rows describe: id -> [side, price, left]
    heaps: dict[int, list] = {1: [], -1: []}
    state: dict[int, dict] = dict(seeds)
    arriving: list[int] = []  # takers inside their own arrival stamp
    n = Counter()
    maker_fills: Counter = Counter()

    def emit(row: tuple) -> None:
        rows.append(row)
        _, typ, oid, size, price, side = row
        if typ == 1:
            live[oid] = [side, price, size]
            heapq.heappush(heaps[side], (-price if side == 1 else price, oid))
        elif oid in live:
            live[oid][2] = 0 if typ == 3 else live[oid][2] - size
            if live[oid][2] <= 0:
                del live[oid]

    def best(side: int):
        h = heaps[side]
        while h and (h[0][1] not in live or live[h[0][1]][1] != (-h[0][0] if side == 1 else h[0][0])):
            heapq.heappop(h)  # stale entry
        return (-h[0][0] if side == 1 else h[0][0]) if h else None

    def execute(ts: int, oid: int, units: int, price: int, side: int) -> None:
        # One execution row per trade of this maker, so each row has its taker.
        queue = unmatched.get(oid, deque())
        while units > 0:
            taker, size = queue.popleft() if queue and queue[0][1] <= units else (0, units)
            n["fills_without_trade"] += int(taker == 0)
            emit((ts, 4, oid, size, price, side))
            fills.append((ts, oid, taker, size, price))
            maker_fills[oid] += size
            units -= size

    def finish(oid: int, fate: str, remainder: int) -> None:
        # A taker is done: did it leave volume a resting order within its limit would have got?
        st = state[oid]
        side, limit = st["side"], st["price"]
        opposite = best(-side)
        within = opposite is not None and (opposite <= limit if side == 1 else opposite >= limit)
        premise = "no remainder" if remainder <= 0 else "fill-or-kill" if st["sub"] == 6 else "liquidity left" if within else "ok"
        available = remainder if premise == "ok" else 0
        n["premise: " + premise] += 1
        n["remainder_volume"] += max(remainder, 0)
        n["available_volume"] += available
        n["taker_remainder_mismatches"] += int(remainder != st["size"] - taker_filled[oid])
        taker_rows.append((st["ms"], oid, side, limit, st["size"], st["sub"], taker_filled[oid], remainder, fate, available))

    def settle(now: int) -> None:
        # Takers whose stamp has passed and that are still live rest their remainder.
        for oid in [o for o in arriving if state[o]["ms"] < now]:
            arriving.remove(oid)
            st = state[oid]
            finish(oid, "rested" if st["pending"] > 0 else "filled", st["pending"])
            if st["pending"] > 0:
                emit((st["ms"], 1, oid, st["pending"], st["price"], st["side"]))
                st.update(left=st["pending"], resting=True)
                n["taker_remainders_submitted"] += 1

    for oid, s in sorted(seeds.items(), key=lambda kv: (kv[1]["side"], kv[1]["price"] * -kv[1]["side"], kv[0])):
        emit((t0 - 1, 1, oid, s["left"], s["price"], s["side"]))
    for m in orders:
        d = m["data"]
        kind, oid, ts = m["event"].removeprefix("order_"), int(d["id"]), int(d["microtimestamp"])
        settle(ts)
        side = 1 if int(d["order_type"]) == 0 else -1
        price, amount = _scaled(d["price_str"], 4), _scaled(d["amount_str"], AMOUNT_PLACES)
        traded = _scaled(str(d.get("amount_traded") or 0), AMOUNT_PLACES)
        if kind == "created":
            if oid in takers:
                state[oid] = {"side": side, "price": price, "left": 0, "taker": True, "resting": False, "ms": ts, "pending": amount,
                              "size": amount, "sub": int(d.get("order_subtype", -1))}
                arriving.append(oid)
                n["takers"] += 1
            else:
                state[oid] = {"side": side, "price": price, "left": amount, "taker": False, "resting": True}
                emit((ts, 1, oid, amount, price, side))
            continue
        st = state.get(oid)
        if st is None:
            # Placed before the recording and never shown by a snapshot first: its earlier size is
            # unknown, so only its fill and its deletion are written (LOBSTER's unknown orders).
            n["unknown_order_events"] += 1
            if traded > 0:
                execute(ts, oid, traded, price, side)
            if kind == "deleted" and amount > 0:
                emit((ts, 3, oid, amount, price, side))
            continue
        if st["taker"] and not st["resting"]:
            # Still arriving: its own fills as taker, at execution prices.
            if kind == "changed":
                st["pending"] = amount
            else:
                arriving.remove(oid)
                finish(oid, "cancelled" if amount > 0 else "filled", amount)
            continue
        if kind == "changed" and price != st["price"]:
            # A resting order moved: a new order for priority (LOBSTER has no amendment).
            n["moved"] += 1
            if traded > 0:
                execute(ts, oid, traded, st["price"], st["side"])
            if st["left"] - traded > 0:
                emit((ts, 3, oid, st["left"] - traded, st["price"], st["side"]))
            emit((ts, 1, oid, amount, price, side))
            st.update(price=price, left=amount)
            continue
        if traded > 0:
            execute(ts, oid, traded, st["price"], st["side"])
        rest = st["left"] - traded
        if kind == "changed":
            if rest > amount:
                emit((ts, 2, oid, rest - amount, st["price"], st["side"]))
                n["cuts"] += 1
            elif rest < amount:
                n["amount_increases"] += 1  # not representable in LOBSTER; counted
            st["left"] = amount
        else:
            n["deletion_amount_mismatches"] += int(rest != amount)
            if rest > 0:
                emit((ts, 3, oid, rest, st["price"], st["side"]))
            st["left"] = 0
    settle(10**30)
    by_type = Counter(r[1] for r in rows)
    clock = lambda ts: f"{(ts - midnight) // 1_000_000}.{(ts - midnight) % 1_000_000:06d}"  # noqa: E731
    with open(derived / "lobster_message.csv", "w", encoding="utf-8", newline="\n") as f:
        for ts, typ, oid, size, price, side in rows:
            f.write(f"{clock(ts)},{typ},{oid},{size},{price},{side}\n")
    with open(derived / "lobster_fills.csv", "w", encoding="utf-8", newline="\n") as f:
        f.write("time,maker_id,taker_id,size,price\n")
        for ts, maker, taker, size, price in fills:
            f.write(f"{clock(ts)},{maker},{taker},{size},{price}\n")
    with open(derived / "lobster_takers.csv", "w", encoding="utf-8", newline="\n") as f:
        f.write("time,id,side,limit,size,subtype,filled,remainder,fate,available\n")
        for ts, *rest in sorted(taker_rows, key=lambda r: (r[0], r[1])):
            f.write(",".join([clock(ts), *map(str, rest)]) + "\n")
    first_created = next(int(m["data"]["microtimestamp"]) for m in orders if m["event"] == "order_created") - midnight
    fates = Counter(r[8] for r in taker_rows)
    summary = {
        "rows": len(rows),
        "rows_by_type": {str(k): by_type[k] for k in sorted(by_type)},
        "opening_book_orders": len(seeds),
        "takers": n["takers"],
        "taker_remainders_submitted": n["taker_remainders_submitted"],
        "cuts": n["cuts"],
        "moved": n["moved"],
        "amount_increases": n["amount_increases"],
        "deletion_amount_mismatches": n["deletion_amount_mismatches"],
        "unknown_order_events": n["unknown_order_events"],
        "trades": len(trades),
        "maker_volume_from_orders": sum(maker_fills.values()),
        "maker_volume_from_trades": sum(maker_trades.values()),
        "makers_not_matching_their_trades": sum(1 for o in set(maker_fills) | set(maker_trades) if maker_fills[o] != maker_trades[o]),
        "executions_without_a_trade": n["fills_without_trade"],
        "taker_fates": dict(fates),
        "taker_remainders": {
            "volume": n["remainder_volume"],
            "premise": {k.removeprefix("premise: "): v for k, v in sorted(n.items()) if k.startswith("premise: ")},
            "available_volume": n["available_volume"],
            "mismatches_with_trades": n["taker_remainder_mismatches"],
        },
        "midnight_utc_us": midnight,
        "from_seconds": f"{first_created // 1_000_000}.{first_created % 1_000_000:06d}",
        "units": {"time": "seconds after midnight UTC", "price": "USD x 10^4", "size": "10^-8 BTC"},
    }
    (derived / "lobster.json").write_text(json.dumps(summary, indent=1) + "\n", encoding="utf-8")
    return summary


def replay(run: Path) -> dict:
    """Check the replay fill model against the recording's real orders (`hftsim replay-truth`,
    leave-one-out): each resting order is replayed as a copy through the stream without its own
    messages, and the copy's modelled fills are compared with its real ones. Writes the LOBSTER
    file first (`to_lobster`); the report lands in `derived/replay.json`."""
    lob = to_lobster(run)
    manifest = json.loads((run / "manifest.json").read_text(encoding="utf-8")) if (run / "manifest.json").exists() else {}
    from .runner import binary, build
    import subprocess

    if not binary().exists():
        build()
    derived = run / "derived"
    args = [str(binary()), "replay-truth", str(derived / "lobster_message.csv"), "--from-seconds", lob["from_seconds"],
            "--takers", str(derived / "lobster_takers.csv"), "--fills", str(derived / "lobster_fills.csv"),
            "--source", "Bitstamp WebSocket API v2 (recorded), as LOBSTER messages by python -m hftsim.bitstamp",
            "--symbol", manifest.get("market", "btcusd").upper(), "--date", manifest.get("started_utc", "unknown")]
    for name in ("orders.jsonl", "trades.jsonl", "snapshots.jsonl"):
        if (run / name).exists():
            args += ["--also", str(run / name)]
    proc = subprocess.run(args, capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(f"hftsim replay-truth failed: {proc.stderr.strip()}")
    report = json.loads(proc.stdout)
    report["lobster"] = lob
    (run / "derived" / "replay.json").write_text(json.dumps(report, indent=1) + "\n", encoding="utf-8")
    return report


def _l3_check(events: Path, snapshots: Path, depth: int, tolerance_ms: float, stamp_us: int = 1, extra: list[str] | None = None) -> dict:
    import subprocess

    from .runner import binary, build

    if not binary().exists():
        build()
    args = [str(binary()), "l3-check", str(events), str(snapshots), "--depth", str(depth), "--tolerance-us", str(int(tolerance_ms * 1000)),
            "--event-stamp-us", str(stamp_us), *(extra or [])]
    proc = subprocess.run(args, capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(f"hftsim l3-check failed: {proc.stderr.strip()}")
    return json.loads(proc.stdout)


def power(run: Path, depth: int = 100, tolerance_ms: float = 500.0) -> dict:
    """Check the check on this recording: break the converted events in known ways, one at a time,
    and rerun `hftsim l3-check` against the unchanged snapshots. Every break must be caught: an
    unexplained snapshot, or (for a missing creation, which a snapshot then reveals as if placed
    before the recording) a revealed order whose id is above the stream's first created id. The
    target is the best-ranked order in the middle snapshot that the stream created, never changed
    and deleted more than 2 s after that snapshot. Writes `derived/power.json`."""
    import tempfile

    stamp_us = check(run, depth, tolerance_ms)["convert"].get("event_stamp_us", 1)  # converts if needed
    derived = run / "derived"
    with open(derived / "events.csv", encoding="utf-8") as f:
        header = f.readline()
        lines = f.read().splitlines()
    snaps: dict[int, tuple[int, list[tuple[int, int]]]] = {}
    with open(derived / "snapshots.csv", encoding="utf-8") as f:
        f.readline()
        for row in f:
            ts, n, _side, rank, oid = row.split(",")[:5]
            snaps.setdefault(int(n), (int(ts), []))[1].append((int(rank), int(oid)))
    middle = sorted(snaps)[len(snaps) // 2]
    mid_ts, listed = snaps[middle]
    life: dict[int, dict[str, int]] = {}
    for j, row in enumerate(lines):
        _ts, kind, oid = row.split(",", 3)[:3]
        entry = life.setdefault(int(oid), {})
        entry[kind] = j if kind != "changed" else entry.get(kind, 0) + 1
    t = lambda j: int(lines[j].split(",", 1)[0])  # noqa: E731

    def suitable(oid: int) -> bool:
        e = life.get(oid, {})
        return "created" in e and "deleted" in e and "changed" not in e and t(e["created"]) < mid_ts and t(e["deleted"]) > mid_ts + 2_000_000

    target = next((oid for _rank, oid in sorted(listed, key=lambda x: x[0]) if suitable(oid)), None)
    if target is None:
        raise RuntimeError(f"no suitable order in snapshot {middle}")
    c, d = life[target]["created"], life[target]["deleted"]

    def edit(j: int, column: int, change) -> list[str]:
        out = list(lines)
        cols = out[j].split(",")
        cols[column] = str(change(int(cols[column])))
        out[j] = ",".join(cols)
        return out

    def late_creation() -> list[str]:
        out = edit(c, 0, lambda _: mid_ts + 1_000_000)
        return sorted(out, key=lambda row: int(row.split(",", 1)[0]))  # stable: ties keep their order

    def shifted(us: int) -> list[str]:
        return [f"{int(r.split(',', 1)[0]) + us},{r.split(',', 1)[1]}" for r in lines]

    cases = {
        "unchanged": ("the converted events as they are", lambda: lines),
        "dropped_deletion": (f"order {target}'s deletion removed", lambda: lines[:d] + lines[d + 1:]),
        "amount_plus_one_unit": (f"order {target} created with 1 unit (1 satoshi) more", lambda: edit(c, 5, lambda a: a + 1)),
        "price_plus_one_tick": (f"order {target} created 1 price unit (1 cent) higher", lambda: edit(c, 4, lambda p: p + 1)),
        "creation_one_second_late": (f"order {target} created 1 s after snapshot {middle} instead of before it", late_creation),
        "creation_missing": (f"order {target}'s creation removed (its deletion stays)", lambda: lines[:c] + lines[c + 1:]),
        "clock_plus_600ms": ("every event 600 ms later (beyond the ±%g ms search)" % tolerance_ms, lambda: shifted(600_000)),
        "clock_minus_600ms": ("every event 600 ms earlier", lambda: shifted(-600_000)),
    }
    results = {}
    with tempfile.TemporaryDirectory() as tmp:
        for name, (what, make) in cases.items():
            path = Path(tmp) / f"{name}.csv"
            with open(path, "w", encoding="utf-8", newline="\n") as f:
                f.write(header)
                f.write("\n".join(make()) + "\n")
            v = _l3_check(path, derived / "snapshots.csv", depth, tolerance_ms, stamp_us)["validation"]
            flagged = v["unexplained"] > 0 or v["revealed_above_first_created_id"] > 0
            results[name] = {
                "change": what,
                "unexplained": v["unexplained"],
                "first_unexplained": v["first_unexplained"],
                "unexplained_differences": v["unexplained_differences"],
                "revealed_above_first_created_id": v["revealed_above_first_created_id"],
                "caught": flagged if name != "unchanged" else None,
                "clean": not flagged if name == "unchanged" else None,
            }
    report = {
        "target_order": target,
        "middle_snapshot": middle,
        "tolerance_ms": tolerance_ms,
        "cases": results,
        "all_caught": all(r["caught"] for n, r in results.items() if n != "unchanged") and results["unchanged"]["clean"],
    }
    (derived / "power.json").write_text(json.dumps(report, indent=1) + "\n", encoding="utf-8")
    return report


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="python -m hftsim.bitstamp", description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("record", help="record the public channels into a new directory")
    r.add_argument("--out", type=Path, required=True)
    r.add_argument("--minutes", type=float, default=30.0)
    r.add_argument("--cap-mb", type=float, default=200.0)
    r.add_argument("--market", default="btcusd")
    r.add_argument("--snapshot-every-s", type=float, default=1.0)
    c = sub.add_parser("convert", help="write derived/events.csv and derived/snapshots.csv")
    c.add_argument("run", type=Path)
    k = sub.add_parser("check", help="convert if needed, rebuild the book and compare it with every snapshot")
    k.add_argument("run", type=Path)
    k.add_argument("--depth", type=int, default=100)
    k.add_argument("--tolerance-ms", type=float, default=500.0, help="offset search window (0 = off)")
    x = sub.add_parser("crossings", help="which events leave the rebuilt book crossed, and what became of those orders")
    x.add_argument("run", type=Path)
    y = sub.add_parser("replay", help="check the replay fill model against the recording's real fills (leave-one-out)")
    y.add_argument("run", type=Path)
    w = sub.add_parser("power", help="break the events in known ways and confirm the check catches each")
    w.add_argument("run", type=Path)
    w.add_argument("--depth", type=int, default=100)
    w.add_argument("--tolerance-ms", type=float, default=500.0)
    a = p.parse_args(argv)
    if a.cmd == "record":
        m = record(a.out, a.minutes, a.cap_mb, a.market, a.snapshot_every_s, log=lambda s: print(s, flush=True))
        print(json.dumps(m, indent=1), flush=True)
    elif a.cmd == "convert":
        print(json.dumps(convert(a.run), indent=1))
    elif a.cmd == "check":
        print(json.dumps(check(a.run, a.depth, a.tolerance_ms), indent=1))
    elif a.cmd == "crossings":
        print(json.dumps(crossings(a.run), indent=1))
    elif a.cmd == "replay":
        print(json.dumps(replay(a.run), indent=1))
    else:
        report = power(a.run, a.depth, a.tolerance_ms)
        print(json.dumps(report, indent=1))
        return 0 if report["all_caught"] else 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
