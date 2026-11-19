# Historical data (phase 5, part 1)

`crates/data` turns vendor files into a normalised, checked stream. It rebuilds the order book
and checks it against the vendor's own snapshots, records where every dataset came from, and
replays hypothetical orders under an explicit queue model. The `hftsim` binary exposes it.

**Status.**
* **Real data: Bitstamp BTC/USD (§6).** 19 minutes of real order-level data were recorded from
  Bitstamp's public feed. The book rebuilt from the order events reproduces **all 1,136** of the
  exchange's own order-level snapshots (best 100 orders per side: ids, prices, amounts, priority
  order). None is unexplained. Deliberately broken copies of the data are caught, down to one
  satoshi on one order. This meets the phase-5 exit criterion ("reconstruction matches vendor
  snapshots") for one venue and one window.
* **LOBSTER: built, never run on a real file.** The adapter, reconstruction, provenance and
  pure-replay fill model are tested only on simulated sessions written in LOBSTER's format.
  LOBSTER no longer offers anonymous samples (§7).
* **Replay fill model against real fills (§6).** Each of 172,862 resting Bitstamp orders was
  replayed through the stream without itself, under four variants. The model never filled an
  order the exchange left unfilled, nor one earlier or for more.
  * The conservative rules alone find a third of real fills on time.
  * With a taker-remainder rule (Bitstamp publishes takers' orders), and history rebuilt without
    each order, it finds 292 of 298 on time and 99.6% of the volume.
  * The check also exposed, and fixed, an optimistic trade-through rule.
* **Not built:** ITCH 5.0, Databento and crypto L2 adapters, and hybrid replay.

> Results on historical data are a separate stage from simulation results. Every result built on
> this crate carries its provenance and states the data level it rests on (ARCHITECTURE §10).

## 1. LOBSTER files, as implemented

LOBSTER (lobsterdata.com) sells reconstructed NASDAQ order books as two CSV files per stock-day.
Source tags as in [RESEARCH_SURVEY.md](RESEARCH_SURVEY.md):
* **[V]**: read in the primary text;
* **[S]**: secondary sources only;
* **[M]**: from memory.

LOBSTER's own documentation page could not be loaded, so nothing here is
[V].

| Detail | As implemented | Source |
|---|---|---|
| File names | `TICKER_DATE_STARTms_ENDms_message_N.csv` and `..._orderbook_N.csv`, for example `AAPL_2012-06-21_34200000_57600000_message_10.csv` | **[S]** a university course page on LOBSTER data |
| Message fields | time, type, order id, size, price, direction | **[S]** documentation of a LOBSTER reconstructor library |
| Time | seconds after midnight, decimal; parsed exactly to nanoseconds, never through floating point | **[S]** same |
| Types | 1 submission, 2 partial cancellation, 3 deletion, 4 execution of a visible order, 5 execution of a hidden order, 6 cross trade, 7 trading halt | **[S]** same |
| Price | dollars × 10,000 | **[S]** same, and a LOBSTER loader script |
| Direction | 1 buy order, −1 sell order: the side of the order the message is about, so an executed sell order is a buyer-initiated trade | side of the book **[S]**; the ±1 coding **[M]** |
| Order-book rows | one per message, the book after it; per level: ask price, ask size, bid price, bid size | **[S]** loader script; one row per message **[M]** |
| Empty levels | size 0 with a dummy price (±9,999,999,999). The reader treats any level with size 0 as empty, so it does not depend on the dummy value | **[M]** |
| Halt rows (type 7) | price −1 halt, 0 quote-only, 1 resume; counted, not otherwise interpreted | **[M]** |

The reader rejects any field it cannot interpret and names the line, instead of guessing.
`hftsim lobster-check` on a real file will confirm or correct every [M] row above.

## 2. Rebuilding the book and checking it against the vendor

A LOBSTER message file starts at its start time (9:30 for a full day). The book at that moment
already holds orders placed earlier, and they appear only through later cancellations and
executions of order ids the file never submitted. `lobster::Rebuilder` therefore keeps, per
price level:
* **known** quantity: orders submitted in the file, tracked order by order;
* **seed** quantity: unknown, pre-existing orders, tracked only in aggregate.

`lobster::validate` applies each message, then compares the best N levels with the vendor's
snapshot row. When they disagree it *reconciles*: it changes only seed quantities, never known
orders, so the book agrees again before the next message. Its report separates a perfect
reconstruction from a reconciled one:

| Field | Meaning |
|---|---|
| `matched`, `matched_share` | rows that agreed before any reconciliation |
| `first_mismatch` | first row (1-based) that did not |
| `reconciled_rows`, `seed_added`, `seed_removed` | how often, and by how many shares, the seeds were changed |
| `unexplained_rows` | contradictions seeds cannot absorb: known orders exceed the vendor's size, or a known level is missing from it. Should be 0 |
| `unknown_order_events`, `unknown_order_volume` | cancellations, deletions and executions of orders never submitted in the file |
| `size_mismatches`, `duplicate_submissions` | internal inconsistencies of the message file |

Levels deeper than N are never compared: the vendor file does not show them.

**Checked on simulated days** (`crates/runner/tests/lobster.rs`):
* **Round trip.** A 20 s market with a market maker, 5 informed traders and 2 arbitrageurs was
  written as LOBSTER files (165,052 rows, 10 levels) and read back. Every row matched without
  reconciliation, with no unknown orders and no size mismatches. The file's visible executions
  equal the run's trades.
* **Independent top of book.** The written best bid and ask, with sizes, agree at every event
  time with the engine feed rebuilt by `lob::MboBook`, which shares no code with the writer.
* **Mid-session start.** The same files cut in half begin with a book the file never built, as
  a real file does at 9:30. Results:
  * row 1 needed reconciliation, adding 15,009 shares of seeds;
  * the other 82,525 of 82,526 rows (99.999%) matched unaided;
  * 936 events touched unknown orders (17,949 shares);
  * no contradiction was unexplained.

## 3. Provenance

`provenance::Provenance` records:
* the source and the instrument and date;
* the data level: L1, L2 or MBO;
* the book levels;
* each input file's path, size and FNV-1a 64-bit hash;
* the known limits of the source.

`hftsim lobster-check` prints it beside every validation, and `hftsim lobster-export` writes it
next to the files. FNV-1a detects accidental change, not tampering.

LOBSTER message files are order-level (MBO) data for the *visible* book. They carry no hidden
orders' resting state and no participant identities, and nothing before the start time.

## 4. Pure replay: a queue model, checked against the exchange

`replay::replay_passive` answers the question every passive backtest has to answer: when would a
hypothetical resting order have filled? The historical market cannot react to it (ARCHITECTURE
§10 mode a), so a FIFO queue model with conservative rules decides (RISKS K3):

* **Joining.** The order joins the back of its level after every message stamped at or before
  its entry. Everything resting there is ahead of it, including unknown pre-existing orders.
* **Cancellations.** Cancellations of orders ahead move it up; those behind do nothing.
* **Executions at its price.** An execution of an order ahead (or of an unknown, pre-existing
  order) only shortens the queue. An execution of an order that joined behind it is volume that
  would have reached it first, so it fills it, up to what history shows: no invented demand.
* **Trade-throughs.** An execution on its side at a worse price means its level was exhausted:
  that volume would have reached it first, so it fills it, up to what is left. Until 2026-11-18
  the rest filled at once, which invents volume when the trade-through was small (found by the
  real-data check in §6). The table below was rerun with the fix and is unchanged: it counts
  fills and their timing, not their size.
* **Taker remainders** (optional, `replay_passive_with`, for feeds that publish takers' orders).
  A taker that ended with volume unfilled at its limit had exhausted every resting order it
  could reach. An order resting within that limit, entered before the taker's stamp, gets up to
  that volume. Whoever supplies the takers must check that premise (§6).
* **Never.** Hidden executions and crosses never fill it, and an order that would cross the
  book on entry is flagged as marketable, not filled.

With MBO data the order ids alone place unknown orders ahead, so the fills do not depend on
snapshots. Snapshots only make the reported queue ahead complete (tested).

### The check against ground truth

The simulator knows what really happened. `hftsim replay-check` (and `runner::replay_check`)
works in four steps:
1. It runs a market with probe agents (`agents::ProbeTrader`) that rest small orders at the
   touch and cancel them after a fixed life.
2. It writes the market's event log as LOBSTER messages *without* the probes' own book events
   and executions: what a vendor file would show if the probes had been someone's hypothetical
   orders.
3. It replays each probe through that stream (entry at its acceptance, expiry at its
   cancellation).
4. It compares the modelled fills with the exchange's.

Settings: seed 2026, 120 s, one market maker (step 7), 10 informed traders, and 6 co-located
probes that place an order every 300 ms on average with a 3 s life.

| Background | Probe size | Probes | Filled by both | Exchange only | Model only | Neither | Same nanosecond | Model later: median / p90 | Fills visible without the probe |
|---|---|---|---|---|---|---|---|---|---|
| default | 1 share | 986 | 924 | 1 | **0** | 61 | 518 | 10 ms / 104 ms | 518 |
| default | 5 shares | 863 | 795 | 6 | **0** | 62 | 0 | 30 ms / 194 ms | 0 |
| zero intelligence | 1 share | 979 | 917 | 0 | **0** | 62 | 502 | 10 ms / 78 ms | 502 |
| zero intelligence | 5 shares | 888 | 813 | 11 | **0** | 64 | 0 | 20 ms / 155 ms | 0 |

In every configuration the model filled no probe that the exchange left unfilled, and none
earlier than the exchange. It found *every* fill that a stream without the probe can show at the
same nanosecond: 518 of 518 and 502 of 502. Those are the fills where the aggressor that hit the
probe went on to trade behind it or through its level in the same command.

**The blind spot.** The other fills came from aggressors whose last shares went to the probe.
Without the probe, history shows no trade for that volume, so the model can only fill the probe
when a later aggressor shows up: 10–30 ms later at the median. With 5-share probes *every* fill
was of this kind. The event log explains why: aggressive orders in this market execute a median
of 2 shares (90th percentile 3, none 6 or more), so a 5-share probe absorbs any aggressor that
reaches it. The blind spot comes from building "history" from a world that contained the probe.
A genuinely probe-free history would show that last volume trading against the order behind.

**What this establishes.** The replay model implements the same FIFO logic as the matching
engine. Given exact order-level data, it is never optimistic.

**What it does not.**
* **Impact.** Real orders change other traders' behaviour, and replay cannot show that (RISKS K3).
* **Data quality.** It says nothing about real data's gaps: hidden orders, missing ids at L2,
  feed timestamps.
* **Real data.** Checked on Bitstamp in §6, leave-one-out against real fills: never optimistic,
  and far more conservative than in the simulator.
* **Realism of the market.** The tiny aggressive orders are a fact about this simulator (RISKS
  K26), not about real markets.

## 5. Simulated sessions as LOBSTER files

`hftsim lobster-export DIR [--levels N] [run flags]` runs one market and writes its public feed
as LOBSTER files:
* symbol `HFTSIM`, pseudo-date 2000-01-01, simulation time 0 = 9:30;
* a `provenance.json` holding the run's manifest.

Mapping:
* `BookAdd` → submission;
* a `BookUpdate` that shrinks an order → partial cancellation;
* `BookRemove` → deletion of the rest;
* a continuous-trading trade → execution of the resting order;
* a halt and its resumption → type 7.

Orders that never rest do not appear, as in exchange order-level feeds. Auction trades have no
equivalent here and are refused.

No simulated agent cuts an order in place, so simulated files have no partial cancellations
(type 2). Real LOBSTER files have many. The writer's type-2 path is tested directly through the
matching engine (TESTING.md, phase 5, mutant D04).

Uses:
* any LOBSTER tool can read simulated data;
* the same analysis code runs on simulated and real files;
* the round-trip tests above.

## 6. Bitstamp: real order-level data, checked against the exchange's snapshots

LOBSTER's sample now needs an academic e-mail, proof of purchase and staff approval (§7).
Bitstamp publishes order-level data without an account. Its WebSocket API v2 documentation was
read ([V]):
* `live_orders_btcusd`: every order's creation, change and deletion, with id, side, price,
  remaining amount, amount traded, subtype and a timestamp. Event ids chain each message to the
  one before, so gaps show;
* `detail_order_book_btcusd`: the best 100 orders per side, with ids, sent after every change;
* `live_trades_btcusd`: trades, naming both orders.

That is order-level data *and* order-level vendor snapshots, so the rebuild can be checked order
by order. LOBSTER's check (§2) compares only aggregate price levels.

### Recording

`python -m hftsim.bitstamp record` subscribes to the three channels. It writes every order and
trade message exactly as received, prefixed by the local receive time, plus the first snapshot of
each second, and stops at a time limit or a size cap. It uses the standard library only: a small
RFC 6455 WebSocket client over `ssl`, tested on synthetic frames.

| Recording | `data/bitstamp/btcusd-20261118T011941Z/` |
|---|---|
| Window | 2026-11-18 01:19:42 – 01:38:38 UTC: 18 min 56 s, stopped by the 200 MB cap |
| Messages | 365,180 order events, 348 trades, 1,136 snapshots |
| Integrity | no reconnects, no event-id gaps, events already in time order |
| Order events | 182,404 created, 182,448 deleted, 328 changed; no stop orders |
| Orders created, by subtype | MOC 173,155 (95%), IOC 6,448, limit 2,770, FOK 29, market 2 |

The recorder checked the cap after writing each message, so this run ended 422 bytes past
200,000,000. It now stops before the message that would pass the cap.

### Conversion

`python -m hftsim.bitstamp convert` writes `derived/events.csv` and `derived/snapshots.csv`
(formats in `crates/data/src/l3.rs`):
* prices become cents and amounts satoshis, converted exactly from the decimal strings (a value
  with finer digits is refused);
* stop orders are left out, since they do not rest until triggered (none appeared);
* deletions carry amount 0.

`derived/convert.json` records the counts above, the event stamps' unit and whether ids rise with
time.

### The check

`hftsim l3-check` (`data::l3`) rebuilds the book from the events and compares it with every
snapshot: the same ids, prices, amounts and priority order (price, then lower id) for the 100
listed orders per side. Three features of a real feed are handled explicitly:

1. **The opening book is unknown.** When recording starts, the book is already full of orders,
   and the event stream mentions such an order only when it changes or leaves. Before each
   comparison a snapshot may therefore *reveal* orders the stream has not mentioned: listed ones
   are added, and revealed ones it no longer shows inside its listed range are retired. A
   snapshot never changes an order the stream has created, changed or deleted.
2. **Event stamps are coarse.** Every order event's "microsecond" timestamp is a whole
   millisecond (all 365,180 are multiples of 1,000), while snapshots carry true microseconds. An
   event stamped `t` happened in `[t, t + 1 ms)`, so events stamped less than 1 ms before a
   snapshot cannot be ordered against it. The snapshot matches at its own time if the book equals
   it before, part-way through or after those events. This assumes stamps are truncated, not
   rounded. It is consistent with the data: no snapshot needed an event stamped after it.
3. **Snapshots can lag the order channel.** A snapshot that matches no state at its own time is
   searched for among the book states within ±500 ms.

| Class | Meaning | Snapshots |
|---|---|---|
| exact | equals the rebuilt book at its own time, nothing revealed | **976**, 120 of them only within the 1 ms stamp |
| explained | equals it after revealing orders placed before the recording | **9** |
| matched with offset | equals the book at an earlier event boundary | **151** |
| unexplained | none of these | **0** |

Other counts:
* **Revealed orders: 288**, all with ids below the stream's first created order. Created ids rise
  with time: 99.07% of consecutive creations, every exception within 8,179 id units, and none
  below the first created id. A missed creation would therefore show up as a revealed order with
  a newer id. There were none.
* **No revealed order** was later corrected or retired without an event.
* **Unknown-order events: 40.** These are changes or deletions of orders the book never held:
  placed before the recording and deeper than the 100 listed.

### Would it catch errors? A break test

`python -m hftsim.bitstamp power` breaks the converted events in one known way at a time and
reruns the check against the unchanged snapshots. The target is the best-ranked order in the
middle snapshot (#569) that the stream created, never changed and deleted more than 2 s later.

| Break | Unexplained snapshots | Caught |
|---|---|---|
| none (control) | 0 | clean |
| the order's deletion dropped | 565, from #572 on | yes |
| its amount +1 satoshi | 4 | yes |
| its price +1 cent | 4 | yes |
| its creation 1 s late | 3 | yes |
| its creation dropped | 0, but it is revealed with an id above the first created id | yes, by the id check |
| every event +600 ms | 1,132 of 1,136 | yes |
| every event −600 ms | 1,114 of 1,136 | yes |

A dropped creation looks exactly like an order placed before the recording. Only the id check
catches it, which is why that check exists.

### What the data says about the feed

* **Snapshots are sometimes stale.** 151 of 1,136 snapshots (13%) show a book older than their
  stamp. In each, the first event the snapshot misses is stamped between 1.0 and 23 ms before
  it (median 1.7 ms), at least one full stamp unit, so it certainly happened first. The state
  the snapshot shows began 1.7–101 ms before its stamp (median 4.6 ms). A snapshot is not "the
  book as of its stamp" to the millisecond.
* **Every incoming order is first published as resting.** The rebuilt book became crossed or
  locked 2,974 times: 2,938 times through an order's creation, 36 through a change, always the
  arriving order itself. Every episode ended within the same millisecond stamp.
  * 2,703 of them (91%) were MOC orders that would have crossed. Each was deleted in that
    millisecond with nothing traded. The documentation names subtype 5 only "MOC"; this is how a
    maker-or-cancel (post-only) order behaves.
  * The other 271 traded: IOC 219, FOK 38, limit 12, market 2.
  * One vendor snapshot (#1124) caught such a moment, bid 64,416.21 above ask 64,416.16, and the
    rebuild reproduced it.
  * For replay, an `order_created` event is not resting liquidity until its millisecond has
    settled. A fill model must not trade against it.
* **Receive times are not usable for latency.** The per-minute minimum of receive time minus
  exchange time is steady at −787 to −764 ms. So the local clock is ~0.77 s behind Bitstamp's,
  drifting ~23 ms over the 19 minutes. In bursts, messages also queued for up to ~4 s (maximum
  +3.3 s). Whether that queueing happened at Bitstamp or in the recorder is not known. No data
  was lost (event ids chain without gaps), but without a synchronised clock the receive times say
  nothing about network latency.

### The replay fill model against real fills

`python -m hftsim.bitstamp replay` first writes the recording as a LOBSTER message file
(`to_lobster`). As in LOBSTER, an order appears only for what of it rests:
* each fill of a resting order is an execution at its price. Bitstamp's `amount_traded` is the
  fill in that event, not a running total;
* takers, named by each trade's `type` (0 buy, 1 sell), never rest. Their trades appear as the
  makers' executions, and their `changed` events, which carry execution prices, are skipped;
* orders the snapshots show before the stream mentions them are submitted just before the first
  event.

The conversion reconciles exactly. Maker executions equal the trades channel to the satoshi (348
trades, 18.08 BTC). No resting order was cut, moved or enlarged, and every deletion's remaining
amount agrees with the fills before it.

Each trade is its own execution row. Two more files describe the takers LOBSTER leaves out:
* `lobster_fills.csv` names the taker of each execution row;
* `lobster_takers.csv` gives each taker's arrival, limit, size, fills and unfilled remainder.

Of the 235 takers, 156 filled completely, 77 were cancelled with a remainder (IOC) and 2 rested
one (limit). That is 28.65 BTC left unfilled in all.

**Leave-one-out.** `hftsim replay-truth` replays each order submitted during the recording as a
copy, with the same entry time, side, price and size, and compares the copy's modelled fills
with the order's real ones:
* a copy expires when its original was cancelled;
* one whose original filled in full, or still rested at the end, waits to the end of the
  recording.

This is the real-data counterpart of the probe check in §4: there the simulator knew the truth,
here the exchange's own fills are the truth. Left out: the 288 opening-book orders, and 9,309
orders gone within their entry millisecond (a copy joins after that millisecond, so it could never
rest beside its original). 172,862 orders are compared; 298 of them really traded.

**Two corrections came out of this check.**
* **Trade-throughs were optimistic in size.** The first version filled a copy's whole remaining
  size as soon as history traded through its price. But only the volume that traded through
  would have reached it, and that may be small. It now fills that volume (§4). On this
  recording the old rule had credited 0.72 BTC (15% of the model's volume) that never traded
  through. The earlier overfill check could not see it: it covers only cancelled orders.
* **"History minus the order" is not "history without it".** Deleting an order's messages leaves
  a hole. Had the order never existed, the volume it took would have gone to the next orders in
  line, or stayed with the taker. The **counterfactual** stream re-matches each filled order's
  volume in price-time priority: first to orders behind it at its price, then to worse prices
  within the taker's limit. What is left stays with the taker as unfilled remainder. Orders that
  never traded need no counterfactual: removing them changes nothing else.

**The taker-remainder rule** (`replay_passive_with`) uses what Bitstamp publishes and LOBSTER
does not: the taker's own order. A taker that ends with volume unfilled at its limit had exhausted
every resting order it could reach. A copy resting at a price within that limit, entered before
the taker's stamp and not cancelled within it, would therefore have received up to that volume.

The premise is checked per taker, on the book once the taker is done, and a taker that fails it
gives the rule nothing. It must have no resting order left within its limit, and it must not be
a fill-or-kill order (killed whole while liquidity rests). All 79 takers with a remainder passed.

| Stream | Rule | Never filled → filled | Earlier | Overfilled | Same ms | Later | Never | Volume found |
|---|---|---|---|---|---|---|---|---|
| history minus the order | queue + trade-through | **0** | **0** | **0** | 97 | 102 | 99 | 22% |
| history minus the order | + taker remainder | **0** | **0** | **0** | 169 | 35 | 94 | 26% |
| history without the order | queue + trade-through | **0** | **0** | **0** | 196 | 87 | 15 | 32% |
| history without the order | + taker remainder | **0** | **0** | **0** | 292 | 1 | 5 | 99.6% |

"Never filled → filled" counts the 172,564 orders that never traded. "Same ms / Later / Never"
counts the 298 that did; "Earlier" and "Overfilled" are among them. "Volume found" is the
model's share of the 18.08 BTC really traded.

* **Never optimistic, in every variant.** No order that never traded got a fill, no copy filled
  earlier than its original, and none for more than it really traded. That holds with the
  taker-remainder rule too: it gave nothing to any of the 172,564 orders that never traded.
* **Where the volume would have gone.** Had each filled order not existed:
  * 3.18 BTC (18%) would have reached orders behind it;
  * 2.12 BTC (12%) would have traded at worse prices;
  * **12.78 BTC (71%) would have stayed with the takers, unfilled.**

  The touch is thin and takers' limits sit at it. That is why the blind spot looked so large:
  a stream built from makers alone cannot show most of the volume a resting order would get.
  On this venue, LOBSTER-style data would understate passive fills badly. Bitstamp's published
  taker orders recover them.
* **The residual misses are timestamp conservatism.** In the strongest variant 6 of 298 fills
  are missed or late:
  * in 5, the volume went to orders that, by sequence, came after the original within its own
    millisecond. The model counts every order stamped in that millisecond as ahead of the copy;
  * in 1, the order was filled inside its own entry millisecond, before its copy could join.
* **The counterfactual shares the model's assumptions.** Price-time priority, and no trader
  reacting to the extra order. So its near-complete recovery shows that the model's queue
  tracking and the rule agree with the exchange's sequence to within one millisecond stamp. It
  does not show that the market would not have reacted (impact, RISKS K3). The test that needs
  no construction is the first column, and it is 0 in every variant.
* **For backtests on this feed:** use the taker-remainder rule. Without it, pure replay gives a
  heavy lower bound on passive fills. The rule's safety rests on its premise, so the exporter
  checks the premise per taker, and the rule is off when the taker files are not given.

### What this establishes, and what it does not

It establishes:
* the adapter and the rebuild are correct on real order-level data from one venue. Every
  snapshot of a 19-minute recording is reproduced, and single-order errors are caught;
* the feed's quirks are measured, not assumed: the unknown opening book, millisecond event
  stamps, stale snapshots and transient crossings;
* the replay fill model is never optimistic against 172,862 real orders, with or without the
  taker-remainder rule. With the rule, and history rebuilt without each order, it recovers 292 of
  298 real fills at the right millisecond and 99.6% of the volume.

It does not establish:
* **Equities.** Nothing is shown about Nasdaq data (LOBSTER, ITCH): a different venue with
  different order types and timestamps. The LOBSTER path is still untested on a real file.
* **Beyond the top 100.** Orders deeper than the snapshots list are never compared.
* **Other windows.** One 19-minute window, about 01:20–01:39 UTC on one day. A busier period
  may behave differently.
* **Hypothetical orders.** The leave-one-out copies are orders someone really placed. A
  strategy's own orders could sit elsewhere in the queue or at other prices. The rules apply to
  them the same way, but the fill rates above are for this order flow.
* **Venues without taker data.** The taker-remainder rule needs the taker's own order. LOBSTER
  and ITCH do not carry it, so on those feeds the conservative rules alone apply.
* **Impact.** Replay still cannot show how the market would have reacted to an extra order.

## 7. Not done yet

* **Real LOBSTER data.** Not validated. lobsterdata.com no longer offers anonymous samples: a
  sample needs an academic e-mail, proof of purchase and staff approval, or a conference trial
  account. The phase-5 exit criterion is met on Bitstamp data
  (§6), not on equities.
* **Other adapters.** Not built:
  * ITCH 5.0. Nasdaq publishes sample files without an account (the smallest, Nasdaq BX, is
    391 MB). Field layouts are not yet verified (RESEARCH_SURVEY §E);
  * Databento MBO (needs an account);
  * crypto L2.
* **Hybrid replay** (ARCHITECTURE §10 mode b). Historical flow replayed into the engine as orders,
  so live agents' fills come from the matching engine itself. Not built.
* **Queue models for L2-only data**, which must assume where cancellations sit in the queue. The
  probe check above is the harness to measure their error against ground truth.

## 8. Commands

Write a simulated session as LOBSTER files:

```bash
hftsim lobster-export out/sim --levels 10 --seed 1 --secs 60 --mm 7 --informed 10
```

Rebuild a LOBSTER day and check it against its snapshots (the level count comes from the file
name):

```bash
hftsim lobster-check AAPL_2012-06-21_34200000_57600000_message_10.csv AAPL_2012-06-21_34200000_57600000_orderbook_10.csv
```

Or every stock-day under a folder: message and order-book files are paired by name. A pair that
fails does not stop the others. Files without a partner are listed.

```bash
hftsim lobster-check path/to/lobster-files
```

Check the replay fill model against the exchange in the simulator:

```bash
hftsim replay-check --probes 6 --probe-interval-ms 300 --probe-life-ms 3000 --seed 2026 --secs 120 --mm 7 --informed 10
```

The same three commands from Python, as `hftsim.lobster.export`, `check` and `replay_check`
(standard library only; they call the binary and return the JSON as dicts).

Record Bitstamp data and check it (§6). Run Python with `-I`, as for any downloaded data. Each
command writes its report under the run's `derived/` folder:

```bash
python -I -m hftsim.bitstamp record --minutes 30 --cap-mb 200 --out ../data/bitstamp/btcusd-run
python -I -m hftsim.bitstamp check ../data/bitstamp/btcusd-run      # converts first; check.json
python -I -m hftsim.bitstamp power ../data/bitstamp/btcusd-run      # break test; power.json
python -I -m hftsim.bitstamp crossings ../data/bitstamp/btcusd-run  # crossings.json
python -I -m hftsim.bitstamp replay ../data/bitstamp/btcusd-run     # fills vs model; replay.json
```

`replay` writes `lobster_message.csv`. Any LOBSTER message file can be checked the same way,
with orders submitted before `--from-seconds` treated as its opening book:

```bash
hftsim replay-truth derived/lobster_message.csv --from-seconds 4783.581000
```

`check` converts again whenever the raw files are newer than the converted ones or the converter
has changed. The binary also runs directly on converted files:

```bash
hftsim l3-check derived/events.csv derived/snapshots.csv --depth 100 --tolerance-us 500000 --event-stamp-us 1000
```
