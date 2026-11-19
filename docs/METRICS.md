# Metrics (phase 4)

`analysis::metrics` (crates/analysis/src/metrics.rs) computes every phase-4 metric from the
matching engine's own event log, plus the gateway's log for cancels it refused itself. Nothing reads
an agent's view of the market. The `hftsim` runner reports these metrics for one run; the Python
layer aggregates them over seeds ([README](https://github.com/RasmusCodes/hft-sim#research-layer)).

Source tags as in [RESEARCH_SURVEY.md](RESEARCH_SURVEY.md):
* **[V]**: read in the primary text;
* **[S]**: secondary sources only;
* **[M]**: from memory.

**Choice** marks a decision this project made where a source leaves room, or where no source
applies.

## 1. Conventions

* **Mids.** A mid is carried as twice the mid, (bid + ask), an integer because prices are integers.
  Sums built from mids are therefore exact.
* **The mid an order met.** This is the book after every event before the order's own
  (`BookPath::at(ts − 1)`). Engine timestamps are unique per command, so this is the state the
  matching engine saw on receipt.
* **Windows.** Every metric is computed over a measurement window [warm-up, end]. The runner's
  default warm-up is 60 s, and the background book needs it to settle.
* **Money.** PnL is exact integer arithmetic in half fee units (1/(2·scale) price units ×
  quantity) and is converted to dollars only for output. Prices are in cents.

## 2. Definitions

| Metric | Definition (as implemented) | Function | Source | Test |
|---|---|---|---|---|
| Quoted spread | Best ask − best bid when the taker order arrived, share-weighted over taker executions | `execution_spreads` | Rule 600(b) "average quoted spread" **[V]** eCFR 17 CFR 242.600(b) | `execution_spreads_follow_rule_600` |
| Effective spread | 2·(P − M) for buys, 2·(M − P) for sells, with M the mid on arrival; share-weighted | `execution_spreads` | Rule 600(b) **[V]** | same |
| Realized spread at horizon h | 2·(P − Mₕ) for buys, 2·(Mₕ − P) for sells, with Mₕ the mid h after the execution, or the window's final mid if that comes first | `execution_spreads` | Rule 600(b) **[V]**; horizons 50 ms, 1 s, 15 s, 1 min, 5 min from Rule 605(a)(1) **[V]** eCFR 17 CFR 242.605 | same |
| Price impact | Effective − realized spread at h | `ExecutionSpreads::price_impact` | Standard decomposition [M] | same |
| Percentage spreads | Average spread ÷ average midpoint, a ratio of averages as the rule specifies | `percentage_effective`, `percentage_realized` | Rule 600(b) **[V]** | same |
| Time-weighted spread | Mean bid–ask spread over two-sided time, in ticks | `stylized_facts` (phase 2) | [STYLIZED_FACTS.md](STYLIZED_FACTS.md) | phase-2 tests |
| Top-of-book depth | (bid size + ask size)/2, averaged over best-quote changes (event-weighted) | `ofi_regression` | Choice; CKS use depth at the best quotes [V] | `ofi_regression_recovers_an_exact_linear_book` |
| PnL decomposition | Per fill: spread capture D(M − P)Q, adverse selection D(Mₕ − M)Q, inventory D(M_T − Mₕ)Q, minus fees as charged. Exact identity with the ledger | `pnl_decomposition` | ARCHITECTURE §8; Menkveld (2013) split [S] | `pnl_decomposition_is_exact_and_matches_hand_computation`; agents `pnl_decomposition_matches_every_ledger_exactly` |
| Windowed PnL decomposition | Over (from, T]: fills after `from` split as above, plus carried = pos(from)·(M_T − M(from)). Sums exactly to PnL(T) − PnL(from) | `pnl_decomposition_window` | Choice | `windowed_decomposition_carries_the_opening_position`; ledger test above |
| Implementation shortfall | Against the decision price, positive = cost: delay D(arrival − decision)·target; execution D·Σ(pᵢ − arrival)qᵢ; opportunity D(end − arrival)·unfilled. The parts sum exactly | `implementation_shortfall` | Perold (1988) [M] | `implementation_shortfall_components_add_up` |
| Realized volatility | √(Σr²/elapsed), r the mid change (ticks) between samples taken every interval | `realized_volatility` | Standard [M] | `roll_amihud_and_realized_volatility_by_hand` |
| Amihud illiquidity | Mean over intervals with trading of \|relative mid change\| ÷ traded value | `amihud` | Amihud (2002) [M], who used daily returns and dollar volume; intraday intervals are a choice | same |
| Roll spread | 2√(−γ₁), γ₁ the first-order autocovariance of trade-price changes; none when γ₁ ≥ 0 | `roll_spread` | Roll (1984) [M] | same |
| Order-flow imbalance | eₙ = 1{Pᴮₙ ≥ Pᴮₙ₋₁}qᴮₙ − 1{Pᴮₙ ≤ Pᴮₙ₋₁}qᴮₙ₋₁ − 1{Pᴬₙ ≤ Pᴬₙ₋₁}qᴬₙ + 1{Pᴬₙ ≥ Pᴬₙ₋₁}qᴬₙ₋₁, summed per interval; OLS of the mid change (ticks) on it | `ofi_contribution`, `ofi_regression` | Cont, Kukanov & Stoikov (2014) §2.1 **[V]** (phase 3, ar5iv) | `ofi_regression_recovers_an_exact_linear_book`; phase-3 OFI tests |
| Fill probability against queue position | Orders bucketed by the quantity resting at their price when they joined. Outcome: filled at least partly, or completely, by the window end. Orders still resting at the end are censored (left out). `touch_only` keeps orders that joined or improved the best price on their side | `fill_probability_by_queue` | Choice | `queue_position_and_fill_outcomes` |
| Latency race | Race messages are IOC/FOK orders (aimed at the opposite side at their limit) and cancels (aimed at their own quote). A race is two or more participants' messages at the same (side, price) within a window, at least one succeeding and one failing. Duration runs from the first success to the first failure | `race_messages`, `latency_races` | Aquilina, Budish & O'Neill (2022): the duration definition and the role of failed messages **[S]** (NBER Digest, Oct 2021; FCA Occasional Paper 50 page). Window (500 µs default) and the two-participant rule are choices | `a_sniping_race_is_found_with_its_winner_losers_and_duration`; agents `a_sniping_race_is_measured_including_the_cancel_the_gateway_refused` |
| Cancels too late | An agent's cancel requests in the window that found no order: rejected `UnknownOrder` by the engine, or refused by the gateway because the order was already filled or gone. Divided by all its cancel requests in the window | runner (from `race_messages`) | Choice; ARCHITECTURE §13 asks for "cancel-too-late rates" | covered by the race tests |
| Inventory RMS | Time-weighted root-mean-square position over the window, from the exchange's fills | runner | Choice | runner tests |

## 3. Choices and what they mean for comparisons

* **Execution-quality statistics are per execution, not per order.** Rule 605 reports per covered
  order, measured from the time the order was received. Here every aggressive execution counts,
  and "receipt" means the engine's processing. The definitions follow the rule, but the numbers
  are not comparable with published 605 reports.
* **Mark-outs use the mid, not a fair-value estimate.** Adverse selection is measured against
  later mids, as is usual for mark-outs. In this market the true value is known and could serve as
  the mark. Using the mid keeps the metric computable on historical data (phase 5).
* **Windowed PnL.** An agent that trades during the warm-up enters the window with a position.
  Without the carried term, its window PnL would depend on fills outside the window. With it, PnL
  over the window equals the ledger's PnL at T minus the ledger's PnL at `from`. The identity is
  exact, and the agents test checks it for every agent of a full market.
* **Race detection is exact in simulation.** On real data the losing messages of a race are
  invisible in limit-order-book data; that is the point of Aquilina et al.'s message-level
  methodology. The simulator has both logs. Cancels that the gateway refuses (the order was
  already filled) never reach the engine, so without the gateway log (phase 4) they would be
  missing. Then a race whose only loser is the maker who cancelled too late would go uncounted.
* **Queue position is measured at entry.** Fill probability is conditioned on the quantity ahead
  when the order joined. It is not conditioned on the queue at later times, and not on the order's
  own size.

## 4. Verification

* **Hand-computed unit tests**, 9 of them in `metrics.rs`. Each builds a small market through the
  real `MatchingEngine`, or a book path by hand for OFI, and checks the metric against arithmetic
  done in the comment.
* **Exact identity on full runs** (`crates/agents/tests/metrics.rs`). In a 20 s calibrated market
  with a market maker, informed traders and fees, every agent's decomposition equals its exchange
  ledger to the last fee unit. This holds at three horizons, for the whole run and for the window
  (5 s, 20 s].
* **Race integration test.** A sniping race measured end to end through the kernel, including the
  maker's cancel that the gateway refused.
* **Mutation testing** of the metrics (`tools/mutation_test.ps1 -Only metrics`, [TESTING.md](TESTING.md)).
