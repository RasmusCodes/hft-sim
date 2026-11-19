# How correctness is established

Phase 0 (exchange core) first; phase 1 (simulation kernel) is at the end.

## Phase 0: exchange core

The engine must be "deterministic and correctly handle edge cases". A green unit-test suite is weak
evidence on its own, so correctness rests on four independent layers. A fifth step checks that the
tests themselves can detect bugs.

| Layer | What | Where | Size |
|---|---|---|---|
| 1. Rulebook | Every behaviour written down before testing it, including reject precedence and event order | [MATCHING_RULES.md](MATCHING_RULES.md) | R1–R11 |
| 2. Scenarios | Hand-computed expected events for each rule: priority, sweeps, IOC/FOK, STP, post-only, modify semantics, validation, phases, five auction tie-break cases, negative prices, timestamps | `crates/lob/tests/scenarios.rs` | 52 tests |
| 3. Differential fuzzing | Random streams through the engine **and** an independent naive implementation (`ReferenceEngine`), with all cross-checks after every command | `crates/lob/tests/differential.rs` | ~0.5M commands per default run; 18M in the scaled run |
| 4. Determinism | Replays give identical event digests and do not depend on pre-allocation; the benchmark replays each 5M-message stream twice | `differential.rs`, `engine_bench.rs` | |
| 5. Mutation testing | Plant 18 realistic bugs one at a time; the suite must fail for each | `tools/mutation_test.ps1` | 16 caught, 2 provably unobservable |

## Cross-checks after every command (`tests/common/mod.rs`)

1. Engine and reference return the same result and **identical event streams**: every field, sequence
   number and timestamp.
2. Both hold the same book: every live order with owner, price, open/total/filled quantity, in
   priority order.
3. The book **rebuilt from the public feed alone** (`MboBook`) equals the engine's book, order by order
   in queue order. The anonymous feed is therefore complete.
4. `check_invariants()`: levels strictly ordered, no empty levels, level totals equal the sum of their
   orders, linked lists consistent in both directions, time priority strictly increasing along every
   queue, id index consistent, no leaked arena slots, no crossed book or resting market order in
   continuous/closed phases.
5. Execution reports are self-consistent (`Tracker`):
   * sequence numbers are contiguous;
   * an id is never accepted twice;
   * fills, cancels and replaces only refer to open orders;
   * each fill's `leaves` equals the previous open quantity minus the fill;
   * each cancel removes exactly the open quantity;
   * every `Trade` is immediately followed by its two `Fill`s with matching id, price, quantity and
     liquidity flag;
   * after every command, the orders open according to the reports are exactly the orders resting in
     the book, with the same open quantity.
6. Best bid/ask agree across engine, reference and feed book. Phase and reference price agree. In call
   phases the indicative auction (fast O(levels) algorithm against brute force over every tick) agrees.

## Fuzzer design (`crates/lob/src/workload.rs`, `FuzzGen`)

* A narrow price band around a drifting centre, so orders cross constantly.
* Two to eight owners, so self-trade prevention triggers often. Every STP mode, TIF and post-only
  combination is generated.
* About 3% invalid fields (zero/negative/off-lot/oversized quantity, off-tick/out-of-bounds price).
* Cancels and modifies mostly hit live orders, sometimes dead or never-issued ids or the wrong owner.
  Modifies cover size-down, size-up, reprice and at-or-below-filled.
* Phase changes about 4% of the time (15% in the auction-heavy suite), including invalid
  transitions; about 0.5% of timestamps go backwards.
* Specs with tick 5 / lot 10 and with tick 1 / lot 1.

Each suite also enforces a **coverage floor**: a list of rule paths (every reject reason, every
cancel reason, both auction outcomes, both replace outcomes, both caller errors, every phase) that
must each occur at least once. A generator change cannot silently stop exercising a rule.

Coverage of one default-mix run (120 seeds × 2,000 commands = 240,000 commands):

| Path | Count | Path | Count |
|---|---:|---|---:|
| trade: continuous | 16,238 | cancelled: SelfTrade | 4,735 |
| trade: auction | 5,702 | cancelled: FokUnfilled | 5,707 |
| uncross: traded / no trade | 1,213 / 434 | cancelled: IocRemainder | 6,940 |
| replaced: priority kept / lost | 4,659 / 12,086 | cancelled: ModifiedBelowFilled | 520 |
| reject: PostOnlyWouldCross | 935 | cancelled: AuctionMarketRemainder | 76 |
| reject: TifNotAllowedInPhase | 10,592 | cancelled: MassCancel | 4,314 |
| Modify rejected: PostOnlyWouldCross | 73 | Modify rejected: MarketOrderNotModifiable | 811 |
| error: NonMonotonicTime | 1,260 | error: InvalidPhaseTransition | 3,165 |

## Results

* `cargo test --offline`: 62 tests pass (4 RNG known-answer/unit, 52 scenarios, 5 differential/determinism, 1 doctest).
* Scaled run, `LOB_FUZZ_SEEDS=1500`, `LOB_FUZZ_STEPS=4000`, release mode: about 18 million fuzzed
  commands with every cross-check after every command, plus the benchmark-flow and replay tests. All
  pass, in 38 s of test time.
* Benchmark streams: 3 × 5M messages each replayed twice with identical digests; the first 100k
  messages are identical to the reference model.

## Mutation testing

`tools/mutation_test.ps1` copies the workspace to a scratch folder, applies one mutation, runs the
whole suite and restores, for 18 mutations of the kind a real bug would make.

**Script flaw found and fixed on 2026-11-15.** Robocopy restores a file with its original
timestamp, and cargo decides what to rebuild by timestamp. A restored file could therefore look
older than the build of its mutated version, so a mutant's compiled code could leak into the next
mutant's run. That happens when consecutive mutants are in different crates, or across runs
sharing the scratch build folder. It was noticed when a mutant of the analysis crate failed a
simulation digest test that cannot depend on that crate. The script now touches the restored files,
and every result below comes from a full rerun with the fixed script. The lob results are unchanged.

**A second flaw, found on 2026-11-19.** `powershell -File` passes `-Ids A13,S02` as one string, so
a run started that way selected no mutant and reported nothing. The script now splits the list and
fails when no mutant matches. Earlier runs passed the ids from inside PowerShell, so their results
stand.

Results:

| | Mutation | Result |
|---|---|---|
| M01 | A modify to the same size loses priority | caught |
| M02 | A buy limit does not match at an equal price | caught |
| M03 | FOK feasibility ignores cancel-aggressor STP | caught |
| M04 | Equidistant auction tie goes to the lower price | **survived: equivalent** |
| M05 | Auction skips one-tick gaps between limit prices | caught |
| M06 | Buy-surplus auction takes the bottom of the top interval | **survived: equivalent** |
| M07 | STP cancel-both keeps the resting order | caught |
| M08 | Post-only buy at the ask is accepted | caught |
| M09 | Modify to exactly the filled quantity is not "done" | caught |
| M10 | An emptied best level is not removed | caught |
| M11 | Mass cancel follows hash-map order | caught |
| M12 | Auction demand excludes bids at the price | caught |
| M13 | An aggressive modify forgets its fills | caught |
| M14 | Uncross excludes bids priced at the uncross price | caught |
| M15 | Feed handler reduces the wrong order on a trade | caught |
| M16 | Continuous trades do not update the reference price | caught |
| M17 | FOK feasibility ignores the limit price | caught |
| M18 | Auctions do not update the reference price | caught |

The two survivors are **equivalent mutants**: they change code paths that provably cannot alter any
output. B(p) only falls and S(p) only rises with p, so the optimal auction prices always form one
contiguous run of ticks. With the reference price on the grid, a tie in step 4 is impossible (M04),
and under market pressure the chosen price is always a limit price, never the inside of a gap (M06).
The proof is in MATCHING_RULES R7. Every mutation that can change behaviour was caught, and most
were caught independently by both the scenario tests and the differential fuzzer.

## What this does not establish

* **Realism.** The tests prove the engine implements *our* rulebook, not that the rulebook matches
  any real venue (RISKS K10).
* **Cross-platform determinism.** Tested on Windows x86_64 only. `Digest` hashes Rust's derived
  `Hash`, which is not a stable cross-version format; long-lived golden files need a canonical
  encoding (not built yet).
* **Performance regressions.** The benchmark has no pass/fail thresholds.
* **Combined invalid fields.** The fuzzer makes one field invalid at a time, so reject precedence when
  several fields are invalid is covered only by scenario tests.

## Commands

```bash
cargo test --offline
```

```powershell
$env:LOB_FUZZ_SEEDS=1500; $env:LOB_FUZZ_STEPS=4000; cargo test --release --offline --test differential
```

```powershell
powershell -File tools\mutation_test.ps1 -Only lob
```

`-Only sim`, `agents`, `mm`, `metrics` and `data` select the other groups, and `-Ids A13,S02`
runs single mutants.

## Phase 1: simulation kernel

The kernel has no second implementation to diff against. Its tests instead:

* pin **exact timings** computed by hand from the configured delays;
* check **conservation laws**;
* check that **every view agrees with the exchange** once all messages have settled.

| Test (`crates/sim`) | What it establishes |
|---|---|
| `round_trip_is_the_sum_of_every_configured_delay` | An acknowledgement arrives exactly at compute + out + gateway + hop + engine (base + per event) + in. The market-data packet arrives at engine end + feed latency. The gateway-assigned order id equals the engine's |
| `engine_service_time_grows_with_the_events_produced` | A three-level sweep (10 events) holds the engine for 10 × the per-event cost |
| `simultaneous_orders_queue_at_the_engine` | Five simultaneous orders are processed one service time apart |
| `each_gateway_session_handles_one_message_at_a_time` | A burst queues in its own session; other sessions are unaffected |
| `a_jittery_link_never_reorders_one_agents_messages` | 300 orders over a Pareto (α = 0.9) link reach the engine in send order |
| `cancel_fill_race_is_decided_by_latency` | A faster maker cancels in time. A faster taker fills, and the late cancel is answered "unknown order" by the gateway, or by the engine when the engine is slow |
| `agents_see_the_book_only_after_their_feed_latency` | An agent's book view lags the exchange by its feed latency to the nanosecond, including the same-time tie rule |
| `gateway_enforces_risk_limits_and_throttle` | Rejections for maximum order size, open orders, potential position and message rate. The engine never sees rejected orders |
| `an_order_can_be_cancelled_before_its_acknowledgement_arrives` | Cancelling by client id works before the acknowledgement arrives |
| `random_auction_end_is_reproducible_and_uncrosses_on_time` | The auction end falls inside its window, is the same for a seed and differs across seeds; the uncross happens at the drawn time |
| `invalid_scheduled_phase_change_is_a_warning_not_a_crash` | Schedule errors are reported, not fatal |
| `same_seed_same_run_and_streams_are_independent` | The same seed gives an identical digest and identical exchange events; a different seed does not. An extra silent agent leaves exchange events bit-identical |
| `exchange_and_agent_views_agree_once_messages_settle` | After a 12-agent zero-intelligence market settles:<br>• every agent's own position, cash and live orders equal the exchange's ledger and book;<br>• every subscriber's book view equals the true book, order by order;<br>• positions and cash net to zero, and fees equal maker/taker volume × rates;<br>• there are no routing warnings, which also shows the gateway's predicted ids were exact |
| `two_instruments_stay_separate_and_consistent` | The same with two instruments under one scheduler |
| `latency_decides_who_wins_a_stale_quote_race` | After a signal jump, the faster of market maker and sniper wins. Marked at the new value, the slow maker loses exactly 400 |
| `golden_digest_of_the_demo_market` | The demo market's digest, recorded before the scheduler and feed refactor, is unchanged: existing configurations behave bit-identically |
| `shared_snapshot_groups_reproduce_private_feeds_exactly` | Twelve agents on three shared snapshot groups produce the same exchange event stream as on private feeds with equal constant latencies, with 4× fewer deliveries and no callbacks; all views agree once messages settle |
| `shared_group_members_get_each_packet_at_the_same_moment` | One delay draw per packet for the whole group (common jitter, never reordered); callback members are called together; snapshot members get no callbacks but see a current view |
| `a_snapshot_member_sees_the_book_only_after_its_groups_latency` | Staleness through a group, to the nanosecond, including the same-time tie rule |
| `an_unknown_feed_group_is_a_build_error` | Configuration errors are caught when the simulation is built |
| Unit tests (14) | `ln`/`exp` within 2–4 × 10⁻¹⁵ relative error of the platform's; normal and exponential sampler moments; bit-reproducible samples; log-normal median and σ, Pareto tail and spike rate reproduced; links never reorder; token bucket; scheduler tie order, also after slot reuse; ledger with fees |

The demo and the capacity benchmark add one more check: every run, including 10 simulated seconds
with 1,000 agents, reproduces its digest when re-run.

### Mutation testing (phase 1)

`tools/mutation_test.ps1 -Only sim` plants 13 bugs in the kernel. **All 13 are caught** (rerun
2026-11-15 with the corrected script, see the phase-0 section; same verdicts as the 2026-11-14 run):

| | Mutation | Caught by |
|---|---|---|
| S01 | Links may reorder messages | link unit test, both consistency tests |
| S02 | Gateway session does not queue | gateway-session test, golden digest |
| S03 | Gateway predicts the wrong exchange ids | 8 tests (routing fails everywhere) |
| S04 | Engine service time ignores the work done | service-time and round-trip tests |
| S05 | Agents think instantly | round-trip test, golden digest |
| S06 | Throttle never applies | risk test |
| S07 | Position limit ignores open buy orders | risk test |
| S08 | Same-time events run last-in first-out | 7 tests, incl. the scheduler unit test and the staleness test |
| S09 | Agents' book views never update | staleness test and both consistency tests |
| S10 | Random auction end is not random | auction-end test |
| S11 | Agent's own view misses its buys | both consistency tests |
| S12 | Late cancel replies cannot be routed | cancel/fill race test (the slow-engine case), golden digest |
| S13 | Snapshot members still get per-packet callbacks | group-delivery and equivalence tests |

The golden-digest test, added before the feed-group refactor, independently catches four of these
(S02, S04, S05, S12).

S02 is the reason `each_gateway_session_handles_one_message_at_a_time` exists. I wrote it after
asking which kernel bug no test would notice: every earlier multi-message test used zero gateway
time, so removing the gateway queue would have passed.

### What this does not establish (phase 1)

* **Realistic latency magnitudes.** The latency profiles and exchange timings are illustrative orders
  of magnitude, not calibrations.
* **Cross-platform reproducibility, in practice.** `detmath` is designed for it, but everything was
  run on Windows x86_64 only.
* **Kernel speed with many event-driven agents.** Shared feed groups fixed the fan-out cost for
  timer-driven traders, but agents that each need a callback on every packet still cost one
  callback per member per packet ([BENCHMARKS.md](BENCHMARKS.md), kernel section; RISKS K20).
* **Strategy behaviour.** The test agents are scripted or zero-intelligence; real agents start in
  phase 2.

## Phase 2: trading agents and the background market

Agents are tested inside the kernel, and every check reads the exchange (event log, ledgers, book
snapshot), not the agent's own records, except where an agent's report is itself under test.
Deterministic scenarios use zero exchange timing and constant latencies, so outcomes can be worked
out by hand; the rest run on the calibrated background.

| Test (`crates/agents/tests/agents.rs`) | What it establishes |
|---|---|
| `background_is_reproducible_and_the_books_balance` | The same seed gives an identical digest and event log, and a different seed does not. After the background stops and every message settles, positions and cash net to zero, and every agent's own position, cash and live orders equal the exchange's and the gateway's. Every noise and fundamental trader had orders accepted, and the executor finished parents |
| `golden_digest_of_the_small_background` | Behaviour regression: a 3-second background market reproduces a recorded digest |
| `strategies_respect_their_limits_at_the_exchange` | Momentum, mean-reversion, informed, latency-arbitrage and fundamental traders each want more than their limits allow. Rebuilt from the exchange's fills over 60 s: position, order size and live orders never exceed the limits, and the position limit binds |
| `room_applies_order_size_capital_and_lot_rounding` | `Limits::room` with order-size, capital (a price-dependent cap), open-order and position limits, rounded down to a 10-share lot |
| `open_orders_count_against_the_position_limit` | Resting bids count as if filled: with a 20-share limit only two 10-share bids rest, and a sweep leaves the position at exactly 20 |
| `informed_trader_takes_only_quotes_beyond_its_threshold` | It buys 5 at a stale 10,010 ask when value jumps to 10,050, with its limit at value − 2 ticks; sells into the bid when value falls; does nothing when the edge is inside its threshold |
| `latency_arbitrageur_wins_only_when_faster_and_then_unwinds_at_value` | A 1 µs sniper beats a 5 µs maker to a stale quote, then offers the shares back at the new value. Reversed latencies: it still tries, and gets nothing |
| `fundamental_trader_never_pays_more_than_value` | With value fixed at 10,000, none of its bids is above and none of its offers below value |
| `fundamental_trader_never_bids_above_value_even_when_covering_a_short` | The one path where the value cap binds: covering part of a short while the market bid is above value. It bids exactly 10,000 |
| `execution_algorithms_follow_their_schedules` | TWAP, VWAP and Almgren–Chriss parents complete. From the exchange's fills: TWAP is half done at half time, VWAP on a U profile is 30–45% done at a quarter, Almgren–Chriss with κ = 3 is over 65% done at half time, and POV at 10% takes 6–11% of the volume in its window |
| `execution_never_overfills_when_slices_outpace_the_round_trip` | A remote executor slicing every 100 µs fills exactly its 100 shares, so children in flight are counted |
| `pairs_trader_trades_both_legs_of_a_tracking_pair` | Two instruments have values linked by a `Tracking` signal. Rebuilt from both books' fills along the whole run: it trades both legs, the spread position reaches full size, and the net position of the two legs never exceeds one leg's size, so the legs move opposite. It ends balanced within one clip |
| `kill_switch_stops_trading_and_pulls_quotes` | A mean-reversion trader with a 1-cent loss limit trips; no order of its reaches the exchange more than 5 ms after the trip, and none is left resting |
| `kill_switch_pulls_resting_quotes_when_it_trips` | A sniper holding a long and a resting offer trips at exactly 40 ms when the market collapses; its offer is cancelled and its position kept |
| `stylized_facts_quick` | Three simulated minutes of the calibrated background: cancellation, executed-share and spread targets met, sign autocorrelation positive at ≥ 95% of lags 1–100, impact positive in every fitted bin |
| `phase_2_acceptance` (ignored; release, about 1 minute) | The phase-2 exit criterion on 8 seeds × 10 minutes ([STYLIZED_FACTS.md](STYLIZED_FACTS.md) §1) |
| Unit tests: agents (8), analysis (4) | Tick rounding; EWMA mean and spread; perception noise; hysteresis; schedules run from 0 to 1 monotonically; Almgren–Chriss front-loads and tends to TWAP; VWAP follows its profile; Pareto metaorder tail; power-law fits (plain and weighted); impact bins on fixed log edges recover δ = 0.5 and reject δ = 1 and negative bins; autocorrelation of an alternating series |

One unit test written earlier in phase 2 was wrong when it first ran. `ewma_tracks_mean_and_spread`
used α = 0.1, where the final EWMA mean has a standard deviation of 0.46, against a tolerance of 0.5
(about one standard deviation). It now uses α = 0.002, making the tolerances about five standard
deviations. The EWMA code was correct.

### Mutation testing (phase 2)

`tools/mutation_test.ps1 -Only agents` plants 15 bugs in the agents and the analysis library.
**All 15 are caught** (2026-11-15, corrected script, final code):

| | Mutation | Caught by |
|---|---|---|
| A01 | Strategy position limit ignores open buy orders | open-orders limit test |
| A02 | Capital limit ignored | `room` test |
| A03 | Order room not rounded down to the lot | `room` test |
| A04 | Informed trader ignores its threshold when buying | informed-trader test |
| A05 | Fundamental trader may bid above its value estimate | both fundamental-trader tests |
| A06 | Almgren–Chriss schedule back-loads | schedule unit test, execution test |
| A07 | POV counts volume traded before its start | execution test |
| A08 | VWAP ignores its volume profile | VWAP unit test, execution test |
| A09 | Execution ignores children in flight | overfill test |
| A10 | Kill-switch threshold a million times too loose | both kill-switch tests |
| A11 | Kill switch leaves quotes resting | deterministic kill-switch test |
| A12 | Latency arbitrageur bids a tick below the stale ask | latency-race test, deterministic kill-switch test |
| A13 | Pairs trader trades both legs the same way | pairs test |
| A14 | Noise traders ignore their anchor | golden digest only |
| A15 | Metaorder impact not signed by side | quick stylised-facts test |

How the suite got there:
1. The first run (contaminated by the script flaw) had A01, A05 and A11 surviving. Each pointed at
   a real gap: no test stacked resting orders against the position limit, the value cap only binds
   when covering a short, and the statistical kill-switch run tripped without quotes resting. Three
   deterministic scenario tests were added.
2. On the first clean run, A13 survived. The pairs test checked leg balance only at the end, and on
   that random path the trader finished flat. It now checks the whole fill path.

A14 is caught only by the golden digest. That is a behaviour-change detector, not a statement that
the anchor works; the anchor's effect is established by the sensitivity runs in
[STYLIZED_FACTS.md](STYLIZED_FACTS.md) §4.3.

### What this does not establish (phase 2)

* **Realism beyond five targets.** See [STYLIZED_FACTS.md](STYLIZED_FACTS.md) §5, in particular the
  short-horizon mean reversion of the anchored background (RISKS K21).
* **That any strategy is good.** The tests check mechanics: limits, prices, schedules, races, kill
  switches. Profitability is not a test property, and no strategy result here is evidence about real
  markets.
* **Robustness of seed-dependent assertions.** Some integration tests assert behaviour of one seeded
  run (for example that a position limit binds within 60 s). An intended behaviour change can require
  re-checking them, as with the golden digests.

## Phase 3: market making

The exit rule was formulas first, then every strategy unit-tested on synthetic paths. The formula
checks are independent of the code under test:
* the GLFT exact solution is compared with a direct numerical HJB solution (RK4 plus a
  golden-section search for each minimum, no closed form);
* Avellaneda–Stoikov is compared with the exact solution's expansion;
* each estimator is compared with simulated or analytic truth.

[MARKET_MAKING.md](MARKET_MAKING.md) §1 and §5 list the tests.

| Area | Tests | What they establish |
|---|---|---|
| Linear algebra | 3 unit | Jacobi eigen-decomposition reconstructs a random symmetric matrix (residual and orthonormality < 1e-12); Gaussian elimination solves and detects singularity |
| Models | 4 unit | GLFT exact = numerical HJB (1e-6 ticks); A–S = first-order expansion (error falls > 90× per 10× shorter horizon); convergence to the T → ∞ limit with mirror symmetry and the right skew direction; closed form within 2% on the inventory term |
| Estimators | 5 unit | σ of a random walk; every CKS event type; A and k of an exponential depth profile; mark-out signs and settlement timing; micro-price = brute-force long-run expected mid (0.015 tick) |
| Strategy ladder | 10 unit | Hand-computed quotes for each step on synthetic snapshots; a sweep of all ten steps over markets, inventories, micro-prices and signal extremes with no quote at or through the opposite best and no self-cross |
| Agent in the simulator | 7 integration | All ten steps quote with maker-only fills and inventory within the bound, as checked on the exchange log; reproducible runs; clean stop; exact quote moves after an inventory change and a kill-switch trip within 10 µs in a scripted market; the queue rule cuts cancels; four market makers alongside the background keep every book and ledger consistent |

One bug was found by these tests before anything else ran. The rule that keeps the two sides from
landing on the same tick moved the wrong side back: the side that rounding had pushed *less* far
inside. The GLFT test exposed it, and mutant B12 now guards it.

### Mutation testing (phase 3)

`tools/mutation_test.ps1 -Only mm` plants 19 bugs in the market-making code. **All 19 are caught**
(2026-11-15, corrected script):

| | Mutation | Caught by |
|---|---|---|
| B01 | GLFT η with the wrong exponent sign | numerical-HJB test, GLFT strategy test, queue-rule integration test |
| B02 | GLFT finite-horizon quotes invert the v ratio | numerical-HJB, A–S expansion and convergence tests |
| B03 | Avellaneda–Stoikov skew has the wrong sign | A–S expansion test, matched-skew assertion |
| B04 | GLFT closed form off by a factor 2 | closed-form test |
| B05 | OFI counts added supply as demand | CKS cases test |
| B06 | Buy mark-outs have the wrong sign | mark-out test |
| B07 | Micro-price transitions not mirrored | micro-price simulation test |
| B08 | Volatility divides by the sampling step, not elapsed time | volatility test (sparse case, added after B08 first survived) |
| B09 | Intensity counts orders below each depth | intensity test |
| B10 | Buy room grows with long inventory | fixed-spread and multi-level tests, every-step integration test |
| B11 | Bid may sit on the best ask | every-step sweep, queue-rule integration test |
| B12 | Collision rule moves the wrong side | GLFT strategy test |
| B13 | Order-flow skew points the wrong way | order-flow test |
| B14 | Toxicity guard widens on good mark-outs | toxicity test, two integration tests |
| B15 | Queue rule keeps orders at the back | queue-rule decision table |
| B16 | Market maker forgets post-only | every-step test (taker fills appear), kill-switch test |
| B17 | Market maker ignores its kill switch | kill-switch test |
| B18 | Stopping leaves quotes resting | stop test, consistency test |
| B19 | Inventory sign flipped | exact quote-move test, every-step and queue-rule tests |

B08 survived the first run, and it was a real gap rather than an equivalent mutant. With dense
observations the elapsed time between samples is barely more than the 1 s sampling step, so the two
are indistinguishable. In a quiet market with observations seconds apart, the mutant would overstate
σ. The volatility test now includes sparse, irregular observations.

### What this does not establish (phase 3)

* **Strategy quality.** The tests check that each step does what it says, not that any step is
  good. Performance comparisons belong to phase 4 (paired seeds, confidence intervals, both
  backgrounds, with and without informed flow).
* **Fill realism.** The agent's fills come from the simulated book (queue position included), but
  the background's known artefacts apply (RISKS K21, K23).

## Phase 4: metrics, runner and Python layer

The exit rule was "metric unit tests on hand-computed cases; PnL decomposition identity holds
exactly". Both are met, and the identity is checked against the exchange's own ledgers, which do
not share the decomposition's code.

| Area | Tests | What they establish |
|---|---|---|
| Metrics (`analysis::metrics`) | 9 unit | Each builds a small market through the real `MatchingEngine`, or a book path for OFI, and checks against hand arithmetic: Rule 600 spreads (signs, receipt mid, horizon cut at the window end); spreads and spread capture use the mid the order met even when the fill moves it; decomposition parts, fees and the exact total; the horizon cut at the mark; windowed decomposition, equal to the change in whole-run PnL for every owner and window start, including a start exactly at a fill; shortfall parts; Roll, Amihud and volatility; OFI β = 0.025, R² = 1, depth 20; queue buckets with touch-only filtering and censoring; a sniping race with winner, losers and duration, a too-short window, and one participant alone |
| Decomposition on a full market (`agents/tests/metrics.rs`) | 1 integration | The calibrated background, a market maker and informed traders, with fees, over 20 s. For every agent (over 100 traded) and three horizons, the decomposition equals the ledger's PnL to the last fee unit, over the whole run and over (5 s, 20 s]. Position and volume match too |
| Races end to end | 1 integration (agents), 1 kernel test extended | A maker's late cancel, refused at the gateway, is recorded in the gateway log and counted as the maker's race loss; the fast sniper wins and the slow one loses |
| Runner (`crates/runner`) | 7 unit | Valid JSON (non-finite numbers as null); flags parsed and mistakes rejected; the same flags reproduce the run exactly, and another seed changes the digest; the config hash ignores flag order and the output path; the event export gives one row per event, with each kind's own fields and the order's side, in the documented event order; validity bitmaps are least-significant bit first; tables come from the same run (same digest), with one trade row per trade, two fills per trade, and participants by id |
| Python layer (`python/tests`, `python -m unittest discover -s tests` from `python/`) | 12 | t quantiles against table values (df 1, 2, 7, 10, 30), mean and paired intervals by hand, Wilson bounds, a seeded bootstrap; `Scenario` defaults give the same digest as the binary's own defaults; repeated `--mm` flags and the decomposition identity through JSON; grids keep job order, and `verify` reproduces a digest; report formatting and the index. The binary is rebuilt once per process before use, even when one exists (added 2026-11-19, after an existing but outdated binary silently ran a check), and without cargo an existing one is used with a warning |
| Native bindings (`python/tests/test_native.py`; skipped without the extension and pyarrow) | 4 | The in-process run equals the binary's run (manifest, market, agents) with fees, informed traders and arbitrageurs; bad flags raise; native runs in parallel threads reproduce the serial digests; the event table agrees with the report: trades, fills, the market maker's position and window fill count rebuilt from rows, maker-only liquidity, and the final mid from the book table; every table survives a Parquet round trip unchanged, dictionary types included |
| Rebuilt binary against the reports | 8 runs | After the runner was split into a library (for the bindings), one run drawn from each report's manifests was re-run with the rebuilt binary: all 8 digests matched |
| Reproducibility of every report | 1 run per report | Each generated report re-runs its first run without the cache and states whether the digest matched |

### Mutation testing (phase 4)

`tools/mutation_test.ps1 -Only metrics` plants 20 bugs in `analysis::metrics`. For each one it
runs the analysis suite and the agents' metrics tests. **All 20 are caught** (2026-11-16):

| | Mutation | Caught by |
|---|---|---|
| C01 | Effective spread not signed by side | Rule 600 spread test |
| C02 | Spreads use the mid after the execution | mid-the-order-met test |
| C03 | Realized spread looks past the window end | Rule 600 spread test (window ending before the move) |
| C04 | Mark-out horizon not cut at the mark time | hand decomposition test (mark at 0.5 s) |
| C05 | A fill exactly at the window start counts inside the window | windowed decomposition test (starts at fill times) |
| C06 | Carried position revalued backwards | windowed decomposition test, ledger identity on the full market |
| C07 | Spread capture uses the mid after the fill | mid-the-order-met test |
| C08 | Queue ahead includes the order itself | queue test |
| C09 | Touch test compares bids with the worst bid | queue test (bid between two levels) |
| C10 | One participant can race itself | race test (lone participant) |
| C11 | Race window twice as long | race test (2 µs window) |
| C12 | Cancels the gateway refused are ignored | race unit test, end-to-end race test |
| C13 | OFI counts new ask supply as demand | OFI test |
| C14 | Amihud uses absolute, not relative, price changes | Roll/Amihud/volatility test |
| C15 | Roll spread off by a factor 2 | same |
| C16 | Opportunity cost measured from the decision price | shortfall test |
| C17 | Maker volume counts taker fills | decomposition tests |
| C18 | Fees added to PnL | decomposition tests, ledger identity |
| C19 | Depth sums both sides | OFI test |
| C20 | Every take counts as a success | race unit test, end-to-end race test |

Writing the mutants exposed four gaps in the hand-computed tests, and the tests were extended
before the run:
* no fill in the hand-made markets moved the mid (C02, C07);
* no window ended before a price move (C03);
* no mark came before a horizon ended (C04);
* nothing tested a bid between two levels, or a lone participant with both a success and a failure
  (C09, C10).

So every mutant was caught on the first run, and these six only because of the extensions.

### What this does not establish (phase 4)

* **That the metrics mean the same as on real data.** The spread definitions follow Rule 600, but
  they are computed per execution at the engine, not per order from receipt (METRICS.md §3).
* **That the research conclusions hold outside the simulator.** The experiments are tested for
  reproducibility, not for realism. Every report is labelled simulation evidence.
* **The Python statistics beyond their tests.** The t quantile is checked against table values and
  the intervals by hand. They assume independent seeds, which is true by construction here.

## Phase 5 (part 1): historical data

The exit criterion is "reconstruction matches vendor snapshots; provenance on every result". It
is checked on simulated stock-days written in LOBSTER's format, on one real recording (19 minutes
of Bitstamp BTC/USD order-level data), and on one real equity day from Nasdaq BX's ITCH feed. No
real LOBSTER file has been read ([HISTORICAL_DATA.md](HISTORICAL_DATA.md) §6–8).

| Area | Tests | What they establish |
|---|---|---|
| LOBSTER reading and writing (`data::lobster`) | 6 unit | The writer, fed by the real matching engine, maps each book event (a modify that cuts an order in place, an execution, a cancellation) to the right LOBSTER row and size, and reads back with every row matching. Times parse exactly to nanoseconds and round-trip; rows parse, and malformed ones are refused with their line number (unknown type, direction other than ±1, too few fields); empty levels are recognised by size; file names split into symbol, date, start, end, kind and levels. A hand-built day with an unknown opening book is rebuilt: row 1 reconciles 150 seed shares, every later row matches unaided, and the 2 events on unknown orders (90 shares) are counted. Contradictions that seeds cannot absorb are reported, a row-count mismatch is an error, and levels beyond the snapshot depth are not compared |
| Provenance | 1 unit | FNV-1a against its reference vectors; JSON escaping of Windows paths; the provenance record's JSON |
| Replay fill model (`data::replay`) | 12 unit | Hand-computed FIFO cases: fills only after the queue ahead is gone; cancellations ahead move the order up, those behind do not; a trade-through fills only what traded through (10 of a 50 order, not 50); expiry, and nothing seen in the stamp an order was cancelled in; an execution away from the executed order's own price (an order ranked at a non-displayed price) takes its shares out of the queue ahead and fills nothing, not even a copy resting at that price; the unknown opening book counts as ahead (with snapshots it is in the reported queue, and the fills are the same without them); an order that would cross is flagged, not filled. **Taker remainders:** a taker's unfilled 30 reach an earlier order within its limit, but not one beyond the limit, one entered in the taker's own stamp, or one cancelled within it. **Leave-one-out:** a copy never sees its original's messages, so it neither queues behind it nor fills from its executions. A hand-built stream with a FIFO violation in its history yields every class: same-stamp and later first fills, exchange only, model only, neither, overfilled, an order gone within its entry stamp, and the opening book left out. **Counterfactual:** a removed order's 10 go 4 to the order behind it, 5 to the next price within the taker's limit, none beyond the limit, and 1 stays with the taker; the rule then hands that 1 to the copy. Taker files parse |
| Simulated days through LOBSTER files (`runner/tests/lobster.rs`) | 5 integration | **Directory check:** two days in nested folders are found, paired by name and checked, and an order-book file without its message file is listed as unpaired. **Round trip:** 165,052 rows, every row matched with no reconciliation, executions = trades, same digest as `hftsim run`. **Independent top of book:** the written best bid and ask equal `lob::MboBook`'s at every event time. **Mid-session start:** only row 1 reconciles; 99.999% of rows match unaided; nothing unexplained. **Replay against the exchange:** 330 probes, no fill the exchange did not make and none earlier, every visible fill found at the same nanosecond |
| Python (`python/tests/test_lobster.py`) | 2 | Export a scenario, check it: every row matches, executions equal trades, provenance names the simulated symbol; the replay check from Python is never optimistic |
| Order-level check (`data::l3`) | 12 unit | Hand-built streams. A hidden opening book is revealed by the first snapshot (explained), then snapshots are exact. Differences on orders the stream created are unexplained and classified (amount, price, extra), with the example JSON checked exactly. Events stamped at a snapshot's own time, or within one coarse stamp unit before it, cannot be ordered against it. A lagging snapshot is found by the offset search, also when it reveals an order at the same time, and the offset's start and end are both checked. A snapshot never reveals an order the stream creates later. A revealed order is retired only when the stream says nothing about it within the comparison, and one changed without an event is corrected. Priority within a price is checked, crossed books are timed from the first snapshot, and files parse |
| ITCH 5.0 (`data::itch`) | 6 unit | Hand-built binary messages. Each layout parses to the right fields: add, replace, cross. A message too short for its type, or with a side other than B/S, is refused, and a wrong length is reported with its type. A day with adds, an execution, a cancel, a delete, a replace, an unknown reference and an unknown type is rebuilt with every count right. It is written in LOBSTER form and agrees with LOBSTER's rebuild after every message. Every other inconsistency the real day was checked for is made to happen once and counted: a reused order reference, an execution sent under another stock, a timestamp that goes back. An execution at a price other than the displayed one (`C`) is exported at the price it traded at, and LOBSTER's rebuild still reduces the order at its own level; a non-displayed trade is counted as hidden. Executions are checked against arrival order: a replacement's jump is explained by its origin, a plain add's is not, and a newer order trading at a non-displayed price is counted apart, not as a jump. Crossed books are counted by session |
| Python (`python/tests/test_itch.py`) | 2 | A gzipped hand-built day decompresses, and is reused on a second call; `hftsim itch-check` runs end to end (checks, cross-check, replay classes traced by hand). A corrupted gzip trailer fails its CRC check |
| Python (`python/tests/test_bitstamp.py`) | 7 | WebSocket framing over a socket pair: text, long and fragmented messages, a ping answered with a masked pong, masked client frames, close raises. Exact unit conversion, finer digits refused. A synthetic recording converts and checks: a stop order left out, an event-id gap counted, a pre-recording order revealed. The break test catches each break on a synthetic recording. Crossings are classified by subtype and fate. The recording becomes LOBSTER messages row for row: an opening-book order, a partial maker fill and a cancel, an IOC taker that never rests, a rejected MOC, and a taker whose remainder rests. The fills and takers files are checked row for row, including the premise check. `hftsim replay-truth` then classifies the orders in all four variants, each traced by hand |

The replay check was also run at full length on both backgrounds with 1- and 5-share probes, about
3,700 probes in all, with the same result (HISTORICAL_DATA.md §4).

### Real data: Bitstamp BTC/USD (2026-11-18)

`python -I -m hftsim.bitstamp check` on the 19-minute recording (365,180 order events,
HISTORICAL_DATA §6):
* **All 1,136 snapshots reproduced:** 976 exact, 9 explained by orders placed before the
  recording, 151 matched at an earlier event boundary, **0 unexplained**;
* 288 revealed orders, none with an id newer than the stream's first created order;
* no revealed order corrected or retired without an event.

**Break test.** `python -I -m hftsim.bitstamp power` reruns the check on deliberately broken copies
of the events against the unchanged snapshots. The unbroken control stays clean, and each of seven
breaks is caught:
* one order's deletion dropped;
* its amount +1 satoshi;
* its price +1 cent;
* its creation 1 s late;
* its creation dropped (caught only by the id check);
* every event 600 ms later or earlier.

**Independent cross-check.** `python -m hftsim.bitstamp crossings` replays the raw messages with
its own book: a heap per side, seeded from the first snapshot only. It finds the same 2,974
crossed-book episodes as the Rust check.

**Replay fill model against real fills.** `python -I -m hftsim.bitstamp replay` converts the
recording to LOBSTER messages. Maker executions equal the trades channel to the satoshi. It then
replays each of 172,862 resting orders through the stream without itself, in four variants:
history minus the order, or rebuilt without it; with and without the taker-remainder rule
(HISTORICAL_DATA §6).
* **Never optimistic in any variant:** **0** of 172,564 never-filled orders filled by the model,
  **0** first fills earlier than the real ones, **0** copies filled for more than their original.
* **Recovery of the 298 real fills at the same millisecond:** 97, 169, 196 and 292 across the
  four variants. The last also recovers 99.6% of the volume.
* **The premise of the taker-remainder rule held for all 79 takers with a remainder:** no resting
  order was left within their limit, and none was a killed fill-or-kill.
* **A correction:** this check found that trade-throughs filled a copy's whole rest. The fix fills
  only the volume that traded through, which removed 0.72 BTC of modelled volume that never
  traded through.

### Real data: Nasdaq BX ITCH (2019-12-30)

`python -I -m hftsim.itch check` on Nasdaq's public sample day: 29,156,757 messages, 8,906 stocks
(HISTORICAL_DATA §7).
* **Layouts:** every message type's length matches its layout.
* **Book rebuild:** zero unknown references, duplicates, stock mismatches, oversize executions or
  cancellations, out-of-order timestamps and crossed books. Every order is gone by the end of the
  day.
* **Independent rebuild:** for the 8 most-traded stocks, the best 10 levels agree with LOBSTER's
  rebuild after all 1,608,422 book-changing messages.
* **Arrival order:** every one of the 578,839 executions at a displayed price was of the oldest
  order at that price. The other 2,686 executions (`C`) were one increment better than displayed:
  orders ranked at a non-displayed price under BX's Price to Comply rule. 1,244 of those passed an
  older displayed order, and the first run reported them as unexplained queue jumps. An
  independent Python pass over the raw file reproduces these counts.
* **Replay, leave-one-out on 801,154 orders:** no false fill, no early first fill, no overfill.
  93% of the real volume is eventually found. The first run had 539 false fills, 72 early first
  fills and 553 overfills. The two corrections behind the change are tested (the replay row
  above) and guarded by mutants D23 and D24. The same reruns left the Bitstamp results and the
  simulator's probe table unchanged.

### Mutation testing (phase 5)

`tools/mutation_test.ps1 -Only data` plants 47 bugs:
* 12 in `data::lobster` and `data::replay` (D01–D12, 2026-11-17);
* 5 in the leave-one-out check (D13–D17, 2026-11-18);
* 5 in the trade-through cap, the taker-remainder rule and the counterfactual (D18–D22,
  2026-11-18);
* 12 in `data::l3` (L01–L12, 2026-11-18);
* 11 in `data::itch` (I01–I11, 2026-11-19);
* 2 in the replay rules the ITCH day led to (D23–D24, 2026-11-19).

For each one it runs the data suite and the simulated-day tests. **All 47 are caught** (the whole
group rerun on 2026-11-19 after those rules changed the replay code), D04 and D20 only after the
gaps they exposed were closed:

| | Mutation | Caught by |
|---|---|---|
| D01 | LOBSTER direction 1 read as a sell order | hand-built day, depth window, mid-session start |
| D02 | Seconds read as milliseconds | time parsing, top of book against the feed handler, replay check |
| D03 | Ask levels read into the bids | row parsing, hand-built day, mid-session start |
| D04 | Partial cancellation written with the new size, not the cut | engine-driven writer test (added after D04 first survived) |
| D05 | Execution written for the aggressor, not the resting order | round trip, mid-session start, top of book |
| D06 | Events on unknown orders leave the seed untouched | hand-built day |
| D07 | Reconciliation seeds the whole level, counting known orders twice | hand-built day, contradictions test |
| D08 | Bid levels listed worst first | depth window, mid-session start, round trip |
| D09 | Executions behind the order never fill it | three replay unit tests |
| D10 | Cancellations ahead do not move the order up | cancellations test (its queue-overstated check, added before the run) |
| D11 | Trade-through test for bids points the wrong way | trade-through test, replay check |
| D12 | A bid at the ask is not seen as marketable | marketable-order test |
| D13 | A leave-one-out copy queues behind its own original | copy test, leave-one-out classes |
| D14 | A copy sees its original's executions and fills itself with them | copy test, leave-one-out classes |
| D15 | Copies of fully filled orders expire at the real fill | leave-one-out classes (the model volume) |
| D16 | Orders gone within their entry stamp are compared | leave-one-out classes |
| D17 | A same-stamp first fill is counted as earlier | leave-one-out classes |
| D18 | A trade-through fills the whole rest, not what traded through | trade-through test, counterfactual |
| D19 | A selling taker's remainder reaches bids below its limit | taker-remainder test |
| D20 | An order entered in the taker's own stamp gets its remainder | taker-remainder test (after D20 first survived) |
| D21 | The counterfactual re-matches bids beyond the taker's limit | counterfactual (the order beyond the limit) |
| D22 | The counterfactual drops what stays with the taker | counterfactual (the 1 the rule hands on) |
| D23 | An execution away from the order's own price is read as one at it | away-from-own-price test (the copy at the ranked price fills too early) |
| D24 | A copy still sees the stamp it was cancelled in | cancelled-stamp test |

D04 survived the first run, and it was a real gap rather than an equivalent mutant. No simulated
agent ever cuts an order in place, so the simulated days contain no partial cancellations, and
nothing else exercised that branch of the writer. The writer test now drives the matching engine
through a modify, an execution and a cancellation.

D20 also survived its first run. The test's order entered in the taker's own stamp never joined
at all, because no later message arrived, so the entry-time condition was never reached. One
later, unrelated message now makes it join before the taker's remainder is handed out.

| | Mutation in `data::l3` | Caught by |
|---|---|---|
| L01 | Bids ranked worst first | hidden opening book, differences, lagging snapshot that reveals, and 4 more |
| L02 | A snapshot may reveal an order the stream creates later | never-reveal-later test, hidden opening book, same-stamp test, and 2 more |
| L03 | Orders the stream deletes within the window are retired by the snapshot | retire test (within ±5 the stream's own deletion must explain it) |
| L04 | Coarse event stamps treated as exact | stamp-unit test |
| L05 | Offset search without revealing pre-recording orders in its copy | lagging snapshot that reveals |
| L06 | A snapshot that only reveals orders counts as exact | hidden opening book, retire, correction, and 1 more |
| L07 | A book with more orders than the vendor lists still matches | differences (the extra order), retire |
| L08 | Unknown-order events counted against the live book, not every order held | retire (a retired order's deletion is not unknown) |
| L09 | Offset end reported as its start | offset search, lagging snapshot that reveals, stamp-unit test |
| L10 | A locked book is not counted as crossed | crossed-book timing |
| L11 | Revealed-id check points the wrong way | hidden opening book, differences (crossed case) |
| L12 | Price differences not classified | differences: price check, added before the run when no test was found to catch it |

L12 is the D10 pattern again: the gap was found while writing the mutant, and the test came first.

| | Mutation in `data::itch` | Caught by |
|---|---|---|
| I01 | An add's price read 4 bytes early | layouts, inconsistencies, arrival order |
| I02 | An execution's expected length one byte short | wrong length, day rebuild |
| I03 | A delete leaves its shares in the book | day rebuild (the independent rebuild disagrees) |
| I04 | The arrival-order check flags executions at the front | arrival order |
| I05 | A locked book is not counted as crossed | crossed books by session |
| I06 | A replacement forgets its original order's arrival | arrival order (the jump is no longer explained) |
| I07 | An execution at a non-displayed price is exported at the displayed price | inconsistencies |
| I08 | An execution under another stock is not flagged | inconsistencies |
| I09 | Timestamps compared with the first, not the latest | inconsistencies |
| I10 | Reused order references never counted | inconsistencies |
| I11 | Executions at a non-displayed price counted as queue jumps | arrival order |

I07–I10 are caught only by the inconsistencies test, which was written before the run. While
choosing these mutants I found that no test made a reused reference, a stock mismatch or a
backward timestamp happen, or sent an execution with its own price. The real day's zero counts for
those checks would have meant nothing if the counters were broken.

I07 changed meaning on 2026-11-19. It first planted "exported at its printed price", which was
then a bug; since the investigation of BX's queue jumps (HISTORICAL_DATA §7), exporting at the
displayed price is the bug, and I07 plants that instead. D23, D24 and I11 guard the other
changes from that investigation.

### What this does not establish (phase 5, part 1)

* **That the reader handles real LOBSTER files.** The format details marked [S] or [M] in
  HISTORICAL_DATA §1 are untested against a vendor file until one is downloaded.
* **That replay is realistic.** The ground-truth check proves the queue logic, not the absence of
  impact. Real orders change other traders' behaviour.
* **That the order-level check holds elsewhere.** It passed on one 19-minute window of one crypto
  venue. Another window, venue or snapshot depth may expose cases these tests do not cover.
* **That replay fill rates transfer.** The leave-one-out copies are orders someone really placed,
  on one crypto venue for 19 minutes and one equity venue for one day. A strategy's own orders, or
  another venue's takers, could see a different share of fills through the blind spot.
* **That the ITCH layouts match the specification text.** They match the data: lengths, and a
  rebuild without a single inconsistency.
* **How LOBSTER files write executions at a non-displayed price.** Written at the displayed
  price, the replay model could not tell them apart, and the false fills would return (RISKS
  K28).
