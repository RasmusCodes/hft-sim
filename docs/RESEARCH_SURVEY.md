# Research survey: matching engines and market-making models

Compiled 2026-11-13 for phase 0 (architecture + order book). Purpose: decide what the exchange,
simulator and strategies must model, and keep a register of which facts and formulas are verified
and which still need checking before they are relied on.

## How this was researched, and how far to trust it

Two research passes ran in parallel, one on exchange rules and one on the literature. Their access
was limited:

* cmegroup.com, euronext.com, eurexchange.com, nasdaqtrader.com and martinfowler.com could not be
  read directly, and arXiv PDFs would not render.
* So most exchange-rule facts come from **search snippets of the exchanges' own pages**, and most
  citations from **bibliographic index records** (RePEc / EconPapers / IDEAS, arXiv abstract pages).

Every item below carries a tag:

| Tag | Meaning |
|---|---|
| **[V]** | Verified from an index record or abstract page that was read |
| **[S]** | Search-snippet level only: plausible and sourced, not read in full |
| **[M]** | From memory, not checked against a source (standard references, listed for completeness) |

Nothing tagged [S] or [M] should be implemented or quoted as fact until it has been checked against
the primary text. Section D lists the formulas that must be transcribed from the papers first.

---

## A. Exchange matching engines

### A1. How production engines are built

* **Single-threaded, sequenced, in-memory business logic** is the established design, not a
  simplification. In LMAX's architecture one business-logic thread holds all state in memory, uses
  event sourcing for recovery and is fed through lock-free ring buffers (the Disruptor). The quoted
  throughput is 6 million orders per second, which an LMAX engineer later attributed to the matching
  processor alone [S] (Fowler 2011).
  → Our core is a single-threaded deterministic state machine. Concurrency belongs around it
  (gateways, feeds, risk) or across independent simulations.
* **Exchange-side latency is tens to low hundreds of microseconds.** Deutsche Börse's 2017 T7 slides
  give about 90 µs average round trip for Xetra and a 76 µs median for Eurex [S]. Nasdaq's technology
  at SIX was reported at about 37 µs average OUCH round trip [S, vendor article]. This is two to three
  orders of magnitude above the in-process matching cost we measure ([BENCHMARKS.md](BENCHMARKS.md)).
  In the simulator, exchange latency is therefore a **modelled parameter**: our implementation speed
  only limits how many simulated events per second we can afford.
* **Geography:** Chicago–New Jersey is about 4.0 ms one way by microwave versus about 6.55 ms over
  optimised fibre [S, secondary sources]. This is the canonical setting for latency arbitrage between
  futures and equities.

### A2. Matching rules differ by venue

| Topic | What we found | Our rulebook |
|---|---|---|
| Allocation | CME Globex tag 1142 lists FIFO, FIFO with LMM, pro-rata, configurable, threshold pro-rata (with or without LMM), institutional prioritisation, allocation, and EBS size priority [S]. | FIFO only (R3); pro-rata later |
| Modify priority | CME: a quantity decrease keeps priority; an increase, price change or account change loses it [S]. Nasdaq OUCH: a Replace always loses priority, and only a partial cancel keeps it [S]. | CME convention (R9) |
| Replace quantity | Vendor FIX docs (CQG, Kalshi) treat `OrderQty` in a cancel/replace as the new **total** including the filled quantity [S]. For `OrderQty ≤ CumQty`, practice varies between rejecting and stopping the order (FIX errata case D11) [S]. | Total-quantity semantics; at or below filled = order done (R9) |
| Self-trade prevention | CME: the **aggressing** order's instruction (tag 8000) decides which side is cancelled; cancel-resting is the default [S]. Nasdaq: cancel oldest / newest [S]. | Aggressor's instruction; four modes (R5) |
| Market orders | CME "market with protection": the order fills only within a protection band around the best opposite price, and the remainder rests as a limit at the band edge [S]. | Not yet: market orders sweep (risk K14) |
| Auctions | Xetra: maximum executable volume, with the price closest to the reference price as the last resort [S]. Minimum surplus and market pressure as explicit steps were not confirmed. Euronext criteria were not reachable. Nasdaq's opening cross minimises imbalance after maximising volume [S]. | Four-step rule over the tick grid (R7) |
| Volatility halts | US LULD: 15 s in a limit state triggers a 5-minute pause, reopened by an auction at the listing exchange [S]. Xetra volatility interruption: an extra auction of at least 2 minutes with a **random end** [S]. CME Velocity Logic: a brief "reserved" state, then reopening [S]. | Halts/auctions driven by the session schedule (R8); automatic triggers later |

The random end of Xetra's volatility auction is worth copying in the simulator's session scheduler.
It exists to stop participants gaming a known uncross time.

### A3. Market-data formats (for the historical-data adapters)

* **Nasdaq TotalView-ITCH 5.0:** order-level messages (add, add with attribution, executed, executed
  with price, cancel, delete, replace, non-cross trade, cross trade, imbalance indicator) [S].
  Databento normalises a replace as delete + add with a new id [S]. *Since phase 5:* the field
  layouts are implemented from memory [M] and checked against a real day (Nasdaq BX, 2019-12-30,
  29.2 million messages). Every type's length matches the data's own length prefixes, and the
  book rebuild finds no inconsistency (HISTORICAL_DATA §7). The specification text itself was not
  read.
* **LOBSTER** (Huang & Polak 2011): a message file per day with event types 1 = new limit order,
  2 = partial cancel, 3 = full deletion, 4 = execution of a visible order, 5 = execution of a hidden
  order, 6 = cross trade, 7 = trading halt. Prices are dollars × 10,000 and direction is ±1. An
  aligned order-book file holds price and size per level [S, via third-party documentation; the
  official table was not read].
* Our engine emits an MBO stream of the same shape (add / reduce / remove / trade / phase), so one
  book builder (`MboBook`) serves the simulator and historical replay alike.
* **Bitstamp WebSocket API v2** (phase 5, read in its documentation [V]). With no account it offers
  `live_orders_<pair>` (order created / changed / deleted, with id, side, price, remaining
  amount, amount traded, subtype and a microsecond-unit timestamp, chained by event ids),
  `detail_order_book_<pair>` (best 100 orders per side with ids, after every change) and
  `live_trades_<pair>` (both order ids). Subtype codes: 0 limit, 1 instant, 2 market, 3 daily,
  4 IOC, 5 MOC, 6 FOK, 7 cash sell, 8 GTD, 20–27 stop and take-profit variants. *Measured, not
  documented:* order-event stamps are whole milliseconds, while snapshot stamps are true
  microseconds (HISTORICAL_DATA §6).

### A4. Message-traffic statistics (to calibrate synthetic flow)

* SEC MIDAS 2012–13: trade-to-order volume ratio about 2.5–4.2%, i.e. about 96–97% of orders
  cancelled rather than executed [S].
* Cancel-to-trade ratios rise with distance from the touch: about 4.6 inside the spread, 9.8 at the
  spread and 117 more than 50 bp away (SEC, 2014) [S].
* Consequence for the engine: cancels dominate traffic, so cancel must be O(1). Ours is: a hash
  lookup plus a list unlink (a level lookup only when the level must be found).

### A5. Existing simulators

* **ABIDES** (Byrd, Hybinette & Hybinette Balch 2020) [V abstract]: discrete-event multi-agent market
  simulator with a configurable pairwise, noisy agent-to-exchange latency matrix and a per-agent
  computation-delay model. This is the **baseline our latency layer must at least match**. We add
  per-link FIFO ordering, gateway queueing and separate market-data feed delays (see ARCHITECTURE §5).
* **JAX-LOB** (Frey et al. 2023) [V abstract]: a GPU-batched order book for reinforcement learning,
  reporting up to 75× per-message speedups. A different trade-off: massive batch parallelism, with
  simpler matching and no message-level latency.

---

## B. Market-making and microstructure models: what each gives the project

### B1. Inventory risk (research questions 1 and 7)

* **Ho & Stoll (1981)** [M]: dealer quotes shift with inventory to manage risk. The starting point
  for the "inventory-aware" market maker.
* **Avellaneda & Stoikov (2008)** [V citation]: mid-price as arithmetic Brownian motion, fills arriving
  at Poisson rate A·exp(−k·δ) at distance δ [M], exponential utility over horizon T. Results:
  * reservation price r = s − q·γ·σ²·(T − t), skewed against inventory q;
  * total spread δᵃ + δᵇ = γ·σ²·(T − t) + (2/γ)·ln(1 + γ/k).

  The structure is confirmed by a secondary source [S]; the exact expressions must be checked
  against the paper before implementation.
* **Guéant, Lehalle & Fernandez-Tapia (2013)** [V citation]: adds inventory bounds; the HJB reduces
  to a system of linear ODEs with closed-form asymptotic quotes. The closed form was **not**
  transcribed (see section D).
* **Cartea, Jaimungal & Penalva (2015)** [M]: textbook treatment with alpha signals and adverse
  selection.

### B2. Information and adverse selection (research questions 2 and 5)

* **Glosten & Milgrom (1985)** and **Kyle (1985)** [M]: the spread compensates for trading against
  better-informed counterparties, and price impact reflects information. Our "informed trader"
  agents implement this channel: noisy, delayed signals about a latent fundamental value.
* **Menkveld (2013)** [V citation]: an actual HFT market maker earned money on the spread and lost
  on positioning. The 2011 working paper reports per trade €1.55 spread profit, a €0.68 positioning
  loss and €0.88 gross profit [S, working-paper figures]. → Market-maker PnL must be **decomposed**,
  not reported as one number (ARCHITECTURE §8).
* **Easley, López de Prado & O'Hara (2012)** [V]: order-flow toxicity (VPIN) as an adverse-selection
  gauge.
* **Lehalle & Mounjid (2017)** [V]: limit-order placement with adverse-selection risk, where latency
  changes the optimal decision. The closest single paper to research questions 3 and 4 combined.

### B3. Order flow, queues and short-horizon prediction (research questions 3 and 6)

* **Cont, Kukanov & Stoikov (2014)** [V, arXiv:1011.6402 via ar5iv]: price changes are linear in
  order-flow imbalance at the best quotes, with slope inversely proportional to depth. The OFI
  definition was transcribed in phase 3. Phase 4 read the empirical set-up: 10-second mid-price
  changes regressed on contemporaneous OFI by OLS in each half-hour subsample, for 50 US stocks in
  April 2010, with an average R² of 65% (68% with a quadratic term). Depth is the average of
  (bid + ask queue)/2 over each subsample's events, as in our metric.
* **Stoikov (2018)** [V]: the micro-price, an estimator of the future mid from imbalance and spread.
  A natural signal for order-flow-aware quoting.
* **Cont, Stoikov & Talreja (2010)** [V] and **Huang, Lehalle & Rosenbaum (2015)** [V]: queue dynamics
  and queue-reactive order-book models. Candidate generators for realistic background flow, and
  models of fill probability as a function of queue position.
* **Moallemi & Yuan** (working paper, SSRN 2996221) [S]: the value of queue position is of the same
  order as the spread. → The engine exposes `queue_ahead()`, and agents can estimate their position
  from the MBO feed.
* **Bacry, Mastromatteo & Muzy (2015)** [V]: Hawkes processes for clustered, self-exciting order flow.

### B4. Speed and latency (research question 4)

* **Budish, Cramton & Shim (2015)** [V]: continuous limit order books create mechanical arbitrage
  races. They propose frequent batch auctions; our call-auction machinery can run that as an
  experiment.
* **Aquilina, Budish & O'Neill (2022)** [V citation; figures S]: in FTSE 100 stocks, races account for
  about 20% of volume, the modal race lasts 5–10 µs, and the latency-arbitrage "tax" is about 0.5 bp.
  Eliminating it would cut the cost of liquidity by about 17%, roughly $5 billion a year in global
  equities. → Simulated time must resolve microseconds and below (we use nanoseconds), and race
  detection belongs in the metrics. *Phase 4:* the race-duration definition comes from the NBER
  Digest's summary of the paper [S]: from the first message that produces a successful trade or
  cancel to the first failed one. The paper's own window and grouping rules were not read; ours
  are choices ([METRICS.md](METRICS.md)).
* **Baron, Brogaard, Hagströmer & Kirilenko (2019)** [V]: *relative* latency rank explains
  performance differences between HFT firms. → Latency experiments should vary rank, not only
  absolute latency.
* **Hasbrouck & Saar (2013)** [V]: low-latency activity measured at millisecond horizons.

### B5. Execution and impact (institutional agents, implementation shortfall)

* **Almgren & Chriss (2001)** [V], **Obizhaeva & Wang (2013)** [V], **Perold (1988)** [M]:
  implementation shortfall and optimal slicing, the basis for the TWAP/VWAP/POV/IS execution agents.
* **Tóth et al. (2011)** [V]: square-root impact. A stylised fact the simulated market should
  reproduce before any impact-related result is trusted.
* **Bouchaud, Bonart, Donier & Gould (2018)** [M] and **Gould et al. (2013)** [V survey]: stylised facts
  of order books, which double as calibration targets.

### B6. Measurement

* SEC Rule 605 effective and realized spreads. The 2024 amendments (Release 34-99679) keep the
  5-minute realized-spread horizon and add 50 ms, 1 s, 15 s and 1 min [S, Federal Register and law-firm
  summaries]. Our metrics will compute realized spreads at configurable horizons, defaulting to that
  set. *Phase 4:* the current eCFR texts of 17 CFR 242.600(b) and 242.605(a)(1) were read [V]. They
  give the average effective, realized and quoted spread definitions, the percentage versions (a
  ratio of averages), and the five horizons. Implemented in `analysis::metrics`
  ([METRICS.md](METRICS.md)).
* Amihud (2002) illiquidity, Roll (1984) implied spread, Lee & Ready (1991) trade signing,
  Hasbrouck (2007) [all M]. In simulation trade direction is known exactly (the `aggressor` field),
  so trade-signing algorithms are only needed for historical data.

### B7. Order-flow memory, latent liquidity and stylised facts (added in phase 2)

Used to build and calibrate the background market ([STYLIZED_FACTS.md](STYLIZED_FACTS.md)). All
from memory; none of the primary texts were re-read.

* **Smith, Farmer, Gillemot & Krishnamurthy (2003)** [M]: the zero-intelligence continuous double
  auction (Santa Fe model): Poisson limit orders, market orders and cancellations. The template of
  `NoiseTrader`.
* **Lillo & Farmer (2004)** [M] and **Bouchaud, Gefen, Potters & Wyart (2004)** [M]: market-order signs
  have long memory, with autocorrelation decaying as a power law of exponent γ < 1. Prices stay
  close to diffusive because impact is partly transient (Bouchaud et al.) or depends on how
  predictable an order was (Lillo & Farmer).
* **Lillo, Mike & Farmer (2005)** [M]: metaorders with Pareto-distributed sizes (tail α), split into
  many child orders, give sign autocorrelation decaying with γ = α − 1. Used to set α = 1.5; the
  measured γ is 0.52.
* **Tóth et al. (2011)** [V] (also B5) and **Donier, Bonart, Mastromatteo & Bouchaud (2015)** [M]:
  latent liquidity that is revealed as the price moves, with a locally linear latent book, gives
  square-root impact. The noise traders' anchor and the fundamental traders' spread-out thresholds
  are crude stand-ins for this mechanism, not implementations of these models.
* **Almgren, Thum, Hauptmann & Li (2005)** [M]: impact exponent about 0.6 in institutional
  executions.
* **Cont (2001)** [M]: stylised facts of returns: no linear autocorrelation except at very short
  horizons where microstructure effects appear, heavy tails, volatility clustering.

---

## C. Design consequences

1. A deterministic single-threaded core (LMAX pattern), with parallelism across simulations. **Done in phase 0.**
2. Latency at the message level, per participant and per direction, with per-link FIFO and queueing
   at the gateway (beyond ABIDES's pairwise matrix). Agents see only a delayed MBO/L2 feed.
   **Phase 1.**
3. Market-maker PnL decomposed into spread capture, adverse selection (mark-outs at several
   horizons), inventory revaluation and fees (Menkveld). **Done in phase 4**, exact to the fee unit
   ([METRICS.md](METRICS.md)).
4. Nanosecond timestamps and race detection (Aquilina et al.). **Done:** timestamps in phase 0. The
   race metric (phase 4) includes cancels the gateway refused.
5. Queue position as a first-class observable (Moallemi & Yuan). **`queue_ahead` done.**
6. Background flow calibrated to stylised facts: cancel ratios, spread distribution, order-sign
   autocorrelation, square-root impact. The benchmark generator is not calibrated (BENCHMARKS.md).
   **Phase 2.**
7. No profitability claim from simulation alone. Strategy results must be replayed on historical MBO
   data and the two clearly separated. **Phase 5.**

## D. Formula register: verify before implementing

| Formula | Needed for | Status | Before implementation |
|---|---|---|---|
| A–S reservation price and total spread | A–S market maker | **Verified (phase 3)** as the first-order expansion in τ of the exact GLFT solution, which itself matches a direct numerical HJB solution. The A–S paper itself was not read | [MARKET_MAKING.md](MARKET_MAKING.md) §1 |
| A–S fill intensity A·exp(−kδ) | A–S, GLFT, calibration | **[V]** as GLFT's model assumption (arXiv:1105.3115 §2, read via ar5iv); online estimator verified on synthetic data | Same |
| GLFT exact quotes, asymptotics and closed form | GLFT market maker | **[V] and verified (phase 3)**: transcribed from arXiv:1105.3115 (ar5iv). The exact solution matches a direct numerical HJB solution to 1e-6 ticks; the closed form is within 2% on the inventory term when α/η is small | Same |
| Order-flow imbalance (CKS) | Research question 6, OFI-aware quoting | **[V] (phase 3)**: transcribed from arXiv:1011.6402 §2.1 (ar5iv); unit-tested on each event type | Same |
| Micro-price | Order-flow-aware quoting | **[S] (phase 3)**: definition from papers citing Stoikov (2018); the original was not available. Estimator verified against brute-force simulation | Same |
| Effective / realized / quoted spread | Metrics | **[V] (phase 4)**: Rule 600(b) definitions and Rule 605(a)(1) horizons, read on eCFR; hand-tested | Done ([METRICS.md](METRICS.md)) |
| Amihud illiquidity | Metrics | [M]; implemented on intraday intervals (Amihud used daily returns) | The original was not re-read |
| Roll implied spread 2√(−γ₁) | Metrics | [M]; hand-tested | The original was not re-read |
| Implementation shortfall (delay / execution / opportunity) | Metrics, execution agents | [M]; exact identity tested | Perold (1988) was not re-read |
| Latency race and its duration | Research question 4 | **[S] (phase 4)**: duration definition from the NBER Digest summary; window and grouping are choices | Read the paper's methodology section before comparing race statistics with its figures |
| Almgren–Chriss trajectory: share done after τ = t/T is 1 − sinh(κ(1 − τ))/sinh(κ) | `ExecutionAgent` (phase 2) | [M], implemented | κ is a free urgency parameter. Before deriving κ from risk aversion, volatility and temporary impact, transcribe the κ formula from the paper |
| Sign memory from metaorder splitting: γ = α − 1 | Background calibration (phase 2) | [M], used | Consistent with simulation (α = 1.5 gives a measured γ of 0.52); check the derivation's assumptions before relying on it elsewhere |
| Thresholds uniform on [0, θ_max] give opposing supply ≈ N·x²/(2θ_max), hence I ∝ √Q | `FundamentalTrader` population (phase 2) | derived here | An elementary calculation, not taken from a paper; holds only while no trader hits its position cap |

## E. Not verified (carried over from the agents' notes)

* FIX specification text for `OrderQty` in 35=G and the `OrderQty ≤ CumQty` case (only vendor docs and forum snippets).
* Euronext auction criteria and market-order handling after the uncross; Xetra minimum-surplus and market-pressure steps.
* The ITCH 5.0 specification text (layouts checked against data instead, HISTORICAL_DATA §7);
  Nasdaq rules 4752/4754/4757 text; cross tie-breaks beyond imbalance.
* Nasdaq BX's priority rules: 0.2% of BX's executions on 2019-12-30 pass an older order at the
  same price, and the rule that allows it was not found (HISTORICAL_DATA §7).
* CME Velocity Logic duration and Stop Spike Logic mechanics.
* Millisecond-level lifetimes of fleeting orders; post-2013 MIDAS cancel rates.
* LOBSTER's official event table and order-book layout. Since phase 5 they are at secondary-source
  level: event types, time, price scale, column layout and file names come from a reconstructor
  library's documentation, a loader script and a course page. Empty-level dummy prices and halt
  codes are still from memory (HISTORICAL_DATA.md §1). LOBSTER's own page did not render.
* What Bitstamp's "MOC" (subtype 5) stands for: the WebSocket documentation gives only the
  abbreviation, and its REST API page did not render here. "Maker-or-cancel" rests on behaviour:
  every MOC order that would have crossed was deleted untraded in the same millisecond.
* DOIs for Avellaneda–Stoikov and LOBSTER.

---

## References

Bibliographic details: [V] where tagged in section B; otherwise [S] or [M] as noted.

* Aquilina, M., Budish, E., O'Neill, P. (2022). Quantifying the high-frequency trading "arms race". *Quarterly Journal of Economics* 137(1), 493–564.
* Almgren, R., Chriss, N. (2001). Optimal execution of portfolio transactions. *Journal of Risk* 3(2), 5–39.
* Almgren, R., Thum, C., Hauptmann, E., Li, H. (2005). Direct estimation of equity market impact. *Risk* 18(7), 58–62. [M]
* Bouchaud, J.-P., Gefen, Y., Potters, M., Wyart, M. (2004). Fluctuations and response in financial markets: the subtle nature of "random" price changes. *Quantitative Finance* 4(2), 176–190. [M]
* Cont, R. (2001). Empirical properties of asset returns: stylized facts and statistical issues. *Quantitative Finance* 1(2), 223–236. [M]
* Donier, J., Bonart, J., Mastromatteo, I., Bouchaud, J.-P. (2015). A fully consistent, minimal model for non-linear market impact. *Quantitative Finance* 15(7), 1109–1121. [M]
* Finch, T. (2009). Incremental calculation of weighted mean and variance. University of Cambridge Computing Service, technical note. [M]
* Lillo, F., Farmer, J. D. (2004). The long memory of the efficient market. *Studies in Nonlinear Dynamics & Econometrics* 8(3). [M]
* Lillo, F., Mike, S., Farmer, J. D. (2005). Theory for long memory in supply and demand. *Physical Review E* 71, 066122. [M]
* Smith, E., Farmer, J. D., Gillemot, L., Krishnamurthy, S. (2003). Statistical theory of the continuous double auction. *Quantitative Finance* 3(6), 481–514. [M]
* Amihud, Y. (2002). Illiquidity and stock returns: cross-section and time-series effects. *Journal of Financial Markets* 5(1), 31–56. [M]
* Avellaneda, M., Stoikov, S. (2008). High-frequency trading in a limit order book. *Quantitative Finance* 8(3), 217–224.
* Bacry, E., Mastromatteo, I., Muzy, J.-F. (2015). Hawkes processes in finance. *Market Microstructure and Liquidity* 1(1), 1550005.
* Baron, M., Brogaard, J., Hagströmer, B., Kirilenko, A. (2019). Risk and return in high-frequency trading. *Journal of Financial and Quantitative Analysis* 54(3), 993–1024.
* Bouchaud, J.-P., Bonart, J., Donier, J., Gould, M. (2018). *Trades, Quotes and Prices*. Cambridge University Press. [M]
* Budish, E., Cramton, P., Shim, J. (2015). The high-frequency trading arms race: frequent batch auctions as a market design response. *Quarterly Journal of Economics* 130(4), 1547–1621.
* Byrd, D., Hybinette, M., Hybinette Balch, T. (2020). ABIDES: Towards high-fidelity multi-agent market simulation. *Proc. ACM SIGSIM-PADS 2020*. arXiv:1904.12066.
* Cartea, Á., Jaimungal, S., Penalva, J. (2015). *Algorithmic and High-Frequency Trading*. Cambridge University Press. [M]
* Cont, R., Kukanov, A., Stoikov, S. (2014). The price impact of order book events. *Journal of Financial Econometrics* 12(1), 47–88. arXiv:1011.6402. (§2 read via ar5iv in phase 3.)
* Cont, R., Stoikov, S., Talreja, R. (2010). A stochastic model for order book dynamics. *Operations Research* 58(3), 549–563.
* Easley, D., López de Prado, M., O'Hara, M. (2012). Flow toxicity and liquidity in a high-frequency world. *Review of Financial Studies* 25(5), 1457–1493.
* Fowler, M. (2011). The LMAX Architecture. martinfowler.com/articles/lmax.html. [S]
* Frey, S., Li, K., Nagy, P., Sapora, S., Lu, C., Zohren, S., Foerster, J., Calinescu, A. (2023). JAX-LOB: a GPU-accelerated limit order book simulator to unlock large scale reinforcement learning for trading. *Proc. ICAIF '23*, 583–591. arXiv:2308.13289.
* Glosten, L., Milgrom, P. (1985). Bid, ask and transaction prices in a specialist market with heterogeneously informed traders. *Journal of Financial Economics* 14(1), 71–100. [M]
* Gould, M., Porter, M., Williams, S., McDonald, M., Fenn, D., Howison, S. (2013). Limit order books. *Quantitative Finance* 13(11), 1709–1742.
* Guéant, O., Lehalle, C.-A., Fernandez-Tapia, J. (2013). Dealing with the inventory risk: a solution to the market making problem. *Mathematics and Financial Economics* 7(4), 477–507. arXiv:1105.3115. (Model, Propositions 1–3 and Theorems 1–2 read in full via ar5iv in phase 3.)
* Guéant, O. (2017). Optimal market making. *Applied Mathematical Finance*. arXiv:1605.01862. [S]
* Guilbaud, F., Pham, H. (2013). Optimal high-frequency trading with limit and market orders. *Quantitative Finance* 13(1), 79–94.
* Hasbrouck, J. (2007). *Empirical Market Microstructure*. Oxford University Press. [M]
* Hasbrouck, J., Saar, G. (2013). Low-latency trading. *Journal of Financial Markets* 16(4), 646–679.
* Ho, T., Stoll, H. (1981). Optimal dealer pricing under transactions and return uncertainty. *Journal of Financial Economics* 9(1), 47–73. [M]
* Huang, R., Polak, T. (2011). LOBSTER: Limit order book reconstruction system. SSRN working paper. [S]
* Huang, W., Lehalle, C.-A., Rosenbaum, M. (2015). Simulating and analyzing order book data: the queue-reactive model. *Journal of the American Statistical Association* 110(509), 107–122.
* Kyle, A. (1985). Continuous auctions and insider trading. *Econometrica* 53(6), 1315–1335. [M]
* Lee, C., Ready, M. (1991). Inferring trade direction from intraday data. *Journal of Finance* 46(2), 733–746. [M]
* Lehalle, C.-A., Mounjid, O. (2017). Limit order strategic placement with adverse selection risk and the role of latency. *Market Microstructure and Liquidity* 3(1), 1750009.
* Menkveld, A. (2013). High frequency trading and the new market makers. *Journal of Financial Markets* 16(4), 712–740.
* Moallemi, C., Yuan, K. (2017). A model for queue position valuation in a limit order book. Working paper, SSRN 2996221. [S]
* Obizhaeva, A., Wang, J. (2013). Optimal trading strategy and supply/demand dynamics. *Journal of Financial Markets* 16(1), 1–32.
* Perold, A. (1988). The implementation shortfall: paper versus reality. *Journal of Portfolio Management* 14(3), 4–9. [M]
* Roll, R. (1984). A simple implicit measure of the effective bid-ask spread in an efficient market. *Journal of Finance* 39(4), 1127–1139. [M]
* SEC (2024). Disclosure of Order Execution Information, Release 34-99679 (amended Rule 605). [S]
* 17 CFR 242.600 (definitions) and 242.605 (disclosure of order execution information), current eCFR text, read 2026-11-16. [V]
* NBER Digest (October 2021). "The Race to Exploit Stock Price Differences between Exchanges" (summary of Aquilina, Budish & O'Neill). [S]
* Stoikov, S. (2018). The micro-price: a high-frequency estimator of future prices. *Quantitative Finance* 18(12), 1959–1966.
* Tóth, B., Lempérière, Y., Deremble, C., de Lataillade, J., Kockelkoren, J., Bouchaud, J.-P. (2011). Anomalous price impact and the critical nature of liquidity in financial markets. *Physical Review X* 1, 021006.
