# Matching rulebook (v0.1)

This is the normative specification of the exchange in `crates/lob`. "The engine" is
`MatchingEngine` (`crates/lob/src/engine.rs`). Every rule below has hand-computed scenario
tests (`crates/lob/tests/scenarios.rs`). The differential fuzzer
(`crates/lob/tests/differential.rs`) also checks each rule against an independent,
deliberately naive implementation of this document (`crates/lob/src/reference.rs`).

Real venues disagree on many of these points. Where they do, the **Venue precedent** notes
say which convention we follow and which venue does something else. Sources and their
verification level are in [RESEARCH_SURVEY.md](RESEARCH_SURVEY.md). This rulebook describes
one internally consistent venue, not a replica of any real exchange.

---

## R1. Units, identifiers, time

| Item | Rule |
|---|---|
| Price | `i64` in instrument price units (e.g. cents). Must be a multiple of `tick_size` and within `[min_price, max_price]`. Negative prices are valid if the bounds allow them (some futures and spreads have traded negative). |
| Quantity | `i64`, strictly positive, a multiple of `lot_size`, at most `max_order_qty`. |
| Order id | Assigned by the engine: 1, 2, 3, … one per `New` command, **including rejected ones**. A modified order keeps its id. |
| Trade id | 1, 2, 3, … per execution. |
| Event sequence | Every event carries `seq`, contiguous from 1 across the engine's life. |
| Time | Every command carries a nanosecond timestamp. It must be ≥ the last processed one; equal timestamps are allowed. A decreasing timestamp is refused with `EngineError::NonMonotonicTime`, with no events and no state change. |
| Clock and randomness | The engine reads no clock and draws no random numbers. Its output is a pure function of its input sequence. |
| Reference price | Starts at `InstrumentSpec::reference_price` (e.g. the previous close) and is replaced by every trade price. It breaks ties in auctions (R7). |

`EngineError` is reserved for **caller errors** (non-monotonic time, invalid phase transition):
nothing happens. Everything a participant can get wrong is a **business reject** delivered as an
event.

## R2. Validation of new orders

The checks run in this order; the first failure decides the reason. The order is part of the spec
because the reason code is observable.

1. Phase: `Closed` → `MarketClosed`; `Halted` → `TradingHalted`.
2. Quantity: ≤ 0 → `InvalidQty`; not a multiple of the lot → `LotSize`; above the maximum → `MaxQty`.
3. Price (limit orders): not on the tick grid → `TickSize`; outside the bounds → `PriceBounds`.
4. Post-only on a market order or a non-GTC order → `InvalidPostOnly`.
5. In an auction call phase: IOC or FOK → `TifNotAllowedInPhase`; post-only → `InvalidPostOnly`.
6. A post-only order that would execute on arrival → `PostOnlyWouldCross`.

A rejected order consumes an order id and produces exactly one `Rejected` event.

## R3. Continuous matching: price-time priority

* An incoming order is **marketable** against a resting order when buy limit ≥ resting ask or sell
  limit ≤ resting bid. Market orders are always marketable.
* It executes against the opposite side best price first, and in arrival order (FIFO) within a
  price, until it is filled or no longer marketable.
* **Execution price = the resting order's price.** The incoming order receives any price improvement.
* Each execution produces `Trade`, then the resting order's `Fill` (`Maker`), then the incoming
  order's `Fill` (`Taker`).
* After matching:
  * a GTC limit order's remainder rests at its limit price, at the back of the queue (`BookAdd`);
  * the remainder of an IOC order or **any** market order is cancelled (`Cancelled{IocRemainder}`).
    Market orders never rest in continuous trading, even with TIF = GTC.
* **Invariant:** in `Continuous` and `Closed` the book is never crossed (best bid < best ask) and holds
  no market orders.

## R4. Fill-or-kill

A FOK order executes its full quantity on arrival or not at all. Feasibility is computed by scanning
the opposite side in priority order within the limit price, applying self-trade prevention (R5)
exactly as the real walk would:

* liquidity that `CancelResting` would cancel contributes nothing;
* reaching an own order under `CancelAggressor` or `CancelBoth` ends the scan.

If the order is infeasible: `Accepted`, then `Cancelled{FokUnfilled}` for the full quantity, and
**nothing else changes**. In particular, no self-trade-prevention cancels happen.

## R5. Self-trade prevention (STP)

The **incoming (aggressor) order's** instruction applies, in continuous trading only:

| Mode | When the next resting order belongs to the same owner |
|---|---|
| `None` | Trade normally (self-trades allowed). |
| `CancelResting` | Cancel the resting order (`Cancelled{SelfTrade}`, `BookRemove`) and keep matching. |
| `CancelAggressor` | Cancel the incoming order's remainder (`Cancelled{SelfTrade}`). A GTC remainder does **not** rest. |
| `CancelBoth` | Cancel the resting order, then the incoming remainder. |

STP does not apply to auction uncrosses (R7). Allocation there is by priority alone.

*Venue precedent:* on CME Globex the aggressing order's self-match instruction (tag 8000) chooses
which side is cancelled, and "cancel resting" is the default when the tag is absent. "Cancel
oldest / newest" is Nasdaq's naming. Snippet-level verification.

## R6. Auction call phase

* Orders rest **without matching**; the book may cross (negative spread).
* Limit orders rest in their price level. Market orders must be GTC and rest in a per-side
  market-order queue that has priority over all limit orders in the uncross.
* IOC, FOK and post-only orders are rejected (R2).
* Cancels and modifies are accepted. A modify during the call phase never executes.
* `indicative_auction()` returns the price, volume and imbalance an uncross would produce now.

## R7. Uncross: single-price auction

Runs when the phase changes from `Auction` to `Continuous` or `Closed` (opening, closing and
reopening auctions).

**Price determination.** Let D be every tick-grid price from the lowest to the highest limit price
present on either side; if no limit orders rest, D = {reference price}. For p in D:

* B(p) = market buys + limit buys priced ≥ p;
* S(p) = market sells + limit sells priced ≤ p;
* V(p) = min(B(p), S(p)) and I(p) = B(p) − S(p).

1. Keep the prices with maximum V. If the maximum is 0, nothing trades.
2. Among those, keep the prices with minimum |I| (smallest surplus).
3. If I > 0 at every remaining price, take the **highest**; if I < 0 at every one, the **lowest**
   (market pressure).
4. Otherwise take the price **closest to the reference price**, the higher one on a tie.

D contains every tick, not just limit prices. Strictly between two adjacent limit prices nothing
changes eligibility, and such an interior price can have a strictly smaller surplus than either
neighbouring limit (see the scenario test `uncross_can_price_strictly_between_limit_prices`). The
engine scores limit prices plus one interval per gap in O(levels); the reference model scores every
tick by brute force.

**Two consequences of monotonicity.** B only falls and S only rises with p, so V = min(B, S) rises
then falls, and the prices that maximise it form one contiguous run of ticks. I = B − S only falls,
so the prices minimising |I| inside that run are contiguous too. Hence:

* step 4 always has a unique answer (the reference price is on the tick grid), and the "higher price
  on a tie" clause can never fire; it is kept only as a total definition;
* under market pressure (step 3) the chosen price is always a limit price, never the inside of a gap.
  If the top of a gap is optimal with a buy surplus, the limit price just above it is optimal as well.

Mutation testing confirmed both: the two planted bugs in exactly this tie-break code are the only
two the test suite does not detect, because neither can change any output ([TESTING.md](TESTING.md)).

**Execution at the uncross price p\*.**

* Eligible buys, in allocation priority: market orders (time priority), then limit buys priced ≥ p\*
  (higher price first, then time). Sells mirror this.
* Buys and sells are paired in that order until V(p\*) is exhausted. Each pair produces `Trade`
  (aggressor `None`), then the buy `Fill`, then the sell `Fill`, both with liquidity `Auction`.
  Pairing is a reporting convention; every execution prints at p\*.
* Unexecuted **market** orders are then cancelled (`Cancelled{AuctionMarketRemainder}`,
  `BookRemove`): buy side first, then sell side, each in time priority. Unexecuted limit orders stay.
* The reference price becomes p\* if anything traded.

**Post-condition: the book is not crossed after an uncross.** Suppose a buy at b and a sell at a ≤ b
both survive with open quantity. If p\* ∈ [a, b], both were eligible, but one side's eligible
quantity is fully executed, a contradiction. Say p\* > b. Sells execute in ascending price, so every
executed sell is priced ≤ a ≤ b, and the buy side was exhausted, so V(p\*) = B(p\*). At p = b:
B(b) ≥ B(p\*) + the b-buy > V(p\*), and S(b) ≥ the executed sells + the surviving a-sell > V(p\*). So
V(b) > V(p\*), contradicting step 1. The case p\* < a is symmetric. The fuzzer checks this invariant
after every command.

*Venue precedent:* modelled on European call auctions (Xetra, Euronext). Maximum executable volume
with "closest to the reference price" as the last resort is confirmed for Xetra at snippet level. The
minimum-surplus and market-pressure steps, the tick-grid domain and the treatment of leftover market
orders are this venue's own rules and were **not** verified against a primary source. The Nasdaq
crosses also minimise imbalance after maximising volume, with further tie-breaks we could not
confirm.

## R8. Trading phases

| Phase | New / Modify | Cancel / CancelAll | Matching |
|---|---|---|---|
| `Continuous` | accepted | accepted | continuous (R3) |
| `Auction` | accepted (R6 restrictions) | accepted | none until uncross |
| `Halted` | rejected `TradingHalted` | accepted | none |
| `Closed` | rejected `MarketClosed` | accepted | none |

Allowed transitions (`SetPhase`); anything else, including a transition to the current phase, is
refused with `EngineError::InvalidPhaseTransition` and changes nothing:

| from \ to | Continuous | Auction | Halted | Closed |
|---|---|---|---|---|
| Continuous | – | yes | yes | yes |
| Auction | yes, **uncross** | – | yes, no uncross | yes, **uncross** |
| Halted | only if clean | yes | – | only if clean |
| Closed | only if clean | yes | yes | – |

"Clean" means no resting market orders and no crossed book. A halt that interrupts a call phase can
therefore only end through another auction. Halts and auctions are triggered externally by the
simulator's session schedule, not by price moves inside the engine (see *Not implemented*).

*Venue precedent:* US LULD pauses (5 minutes after 15 seconds in a limit state) and Xetra volatility
interruptions (an auction of at least 2 minutes with a random end) both reopen through an auction.
That is why a halted, crossed book may only reopen via `Auction`. Snippet-level verification.

## R9. Cancel/replace (`Modify`)

`Modify { price, qty }` sets a new limit price and a new **total** order quantity, including what has
already been filled. This follows FIX `OrderQty` convention as documented by vendors (CQG, Kalshi);
we did not reach the FIX specification text. With total-quantity semantics, a modify that races a
fill cannot raise the participant's intended total exposure, which matters once latency is
simulated.

Validation order: phase (`MarketClosed` / `TradingHalted`) → `UnknownOrder` → `NotOwner` →
`MarketOrderNotModifiable` → quantity checks (R2.2) → price checks (R2.3). On failure:
`RequestRejected`, order unchanged.

Then, with `cum` = quantity already filled:

1. `qty ≤ cum`: the order is done. `Cancelled{ModifiedBelowFilled}` for the open quantity, then
   `BookRemove`. FIX practice varies (reject vs cancel the remainder); we cancel because the
   participant asked for less exposure, and rejecting would leave the larger order working.
2. A post-only order whose new price would execute now (continuous trading) is rejected with
   `RequestRejected{PostOnlyWouldCross}`; the order is unchanged.
3. Same price and new open quantity (`qty − cum`) ≤ current open quantity: **priority kept**.
   `Replaced{priority_kept: true}`, then `BookUpdate`.
4. Otherwise (price change or size increase): **priority lost**. `Replaced{priority_kept: false}`,
   then `BookRemove`. In continuous trading the order is then matched as an incoming order (R3) with
   its own STP instruction (R5), and any remainder re-enters at the back of the queue with the same
   order id (`BookAdd`). In a call phase it re-enters without matching.

*Venue precedent:* CME (a quantity decrease keeps priority; an increase or a price change loses it).
Nasdaq differs: an OUCH Replace always loses priority, and only a partial cancel keeps it. A future
rulebook option could select the Nasdaq convention.

## R10. Cancel and mass cancel

* `Cancel`: `UnknownOrder` (never existed, already filled or cancelled; this is also the
  "too late to cancel" answer) → `NotOwner` → `Cancelled{Requested}` + `BookRemove`. Accepted in every
  phase.
* `CancelAll { owner }`: every live order of the owner, including market orders resting in a call
  phase, in ascending order id: `Cancelled{MassCancel}` + `BookRemove` each. No live orders: no events.

## R11. Event model and order

Two streams are interleaved in one sequence:

* **Private execution reports**, routed by `owner`: `Accepted`, `Rejected`, `Fill`, `Cancelled`,
  `Replaced`, `RequestRejected`.
* **Public market-by-order feed**, anonymous: `BookAdd`, `BookUpdate`, `BookRemove`, `Trade`,
  `AuctionUncross`, `PhaseChanged`.

A `Trade` implicitly executes the resting order: the passive one in continuous trading, both orders
in an auction. A fully executed order leaves the book with no further event, as in ITCH. The public
feed alone is sufficient to rebuild the full order-by-order book: `MboBook` does this, and the tests
compare it with the engine after every command.

Event order per command:

| Command | Events |
|---|---|
| New, rejected | `Rejected` |
| New, call phase | `Accepted`, `BookAdd` |
| New, FOK infeasible | `Accepted`, `Cancelled{FokUnfilled}` |
| New, continuous | `Accepted`; per walk step one of: `Trade`, `Fill(maker)`, `Fill(taker)` / STP cancel-resting: `Cancelled(resting)`, `BookRemove` / STP cancel-aggressor: `Cancelled(incoming)` (walk ends) / STP cancel-both: `Cancelled(resting)`, `BookRemove`, `Cancelled(incoming)` (walk ends); then `BookAdd` (GTC limit remainder) or `Cancelled{IocRemainder}` (other remainder) |
| Cancel | `Cancelled{Requested}`, `BookRemove`; or `RequestRejected` |
| Modify | `RequestRejected`; or `Cancelled{ModifiedBelowFilled}`, `BookRemove`; or `Replaced(kept)`, `BookUpdate`; or `Replaced(lost)`, `BookRemove`, [walk events as for New], [`BookAdd`] |
| CancelAll | (`Cancelled{MassCancel}`, `BookRemove`) per order, ascending id |
| SetPhase from Auction to Continuous/Closed | `AuctionUncross`, (`Trade`, `Fill(buy)`, `Fill(sell)`)\*, (`Cancelled{AuctionMarketRemainder}`, `BookRemove`)\*, `PhaseChanged` |
| Other SetPhase | `PhaseChanged` |

*Venue precedent:* ITCH 5.0 represents a replace as a new order reference number. Databento
normalises it as a delete plus an add, which is what our `BookRemove` + `BookAdd` with the same id
amounts to for book building.

---

## Not implemented (deliberate, for later phases)

* Allocation algorithms other than FIFO: pro-rata, LMM, top-order and the other CME Globex
  algorithms.
* Hidden, iceberg and minimum-quantity orders; stop orders; DAY/GTD expiry.
* Market-order price protection (CME "protection points") and limit price collars.
* Automatic volatility halts: LULD bands, Xetra volatility interruptions, CME Velocity Logic. Today
  halts and auctions come only from `SetPhase`.
* Trade busts and corrections; implied/spread matching; multi-instrument books (one engine per
  instrument for now).
* Fees, rebates, positions and margin. These belong to the gateway/clearing layer, not to matching.
