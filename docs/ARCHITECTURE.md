# hft-sim architecture (proposal v0.1, 2026-11-13)

`hft-sim` is a research platform for market microstructure. It has a deterministic electronic
exchange, a discrete-event simulator with an explicit latency model, autonomous trading agents, a
ladder of market-making strategies, a Python research layer, historical-data replay and a live
dashboard.

This document proposes the architecture for the whole project. It also records what is built so
far:
* the exchange core (`crates/lob`, phase 0);
* the simulation kernel with its latency model (`crates/sim`, phase 1);
* the trading agents with a background market calibrated to stylised facts (`crates/agents` and
  `crates/analysis`, phase 2);
* the market-making ladder (`crates/agents/src/mm`, phase 3).

All four are finished and tested. Companion documents:

* [MATCHING_RULES.md](MATCHING_RULES.md): the exchange rulebook (normative).
* [SIMULATION.md](SIMULATION.md): the simulation kernel (normative).
* [AGENTS.md](AGENTS.md): the trading agents and presets.
* [STYLIZED_FACTS.md](STYLIZED_FACTS.md): calibration targets, measurement, results and deviations
  of the background market.
* [MARKET_MAKING.md](MARKET_MAKING.md): market-making formulas (sources and verification), the
  ladder, the market-maker agent and first runs.
* [RESEARCH_SURVEY.md](RESEARCH_SURVEY.md): matching engines and market-making literature, with
  verification status.
* [RISKS.md](RISKS.md): technical risk register.
* [TESTING.md](TESTING.md): how correctness is established.
* [BENCHMARKS.md](BENCHMARKS.md): measured performance and methodology.

---

## 1. Starting point (repository inspection)

* `hft-sim/` is a new, self-contained Cargo workspace. No existing exchange, order-book or
  simulation code was reused.
* Development machine toolchain:
  * installed: Rust 1.99 on the GNU target (self-contained linker), Python 3.12, Git;
  * **not** installed: any C/C++ compiler, CMake, Node.js/npm.

  Its local crate cache happened to hold criterion, serde, rayon and clap; nothing uses them.
* Hardware: AMD Ryzen AI 7 PRO 350 laptop (8 cores / 16 threads), 31 GB RAM, Windows 11.

## 2. Design principles

1. **The exchange core is a pure function of its input.** `(timestamp, Command) → events`, with no
   clock, no randomness and no I/O. Strategy and research code depend on it, never the reverse.
2. **Integers in the core.** Prices and quantities are `i64`; floating point appears only in
   strategies and analytics.
3. **One event schema.** The engine's market-by-order (MBO) feed is also the format agents consume,
   the storage format, and the target of historical-data normalisation. Every dataset carries its
   provenance: simulated (with config hash) or historical (venue, source, licence).
4. **Agents never see the true book.** They receive only their own execution reports and the public
   feed, each delayed by the latency model, and build their own book view from it, as a real feed
   handler does.
5. **Reproducible by construction.** One master seed derives every random stream; a run is
   fingerprinted by the digest of its event stream; each experiment writes a manifest.
6. **Correctness before speed.** Rulebook, then hand-computed scenarios, then differential fuzzing
   against an independent reference model, then mutation testing of the test suite. Benchmarks
   report a noise floor next to every latency figure.
7. **Simulation is not validation.** No strategy is called profitable on simulation results alone;
   historical replay is a separate, labelled stage, and its own limits are stated.

## 3. Layers

```
┌──────────────────────── research layer (Python, phase 4) ────────────────────────┐
│ experiment configs · Monte Carlo runner · metrics · notebooks · research report  │
└──────────────▲────────────────────────────────────────────────▲──────────────────┘
               │ PyO3 bindings (package `hftsim`)                │ Parquet / Arrow files
┌──────────────┴──────── simulation kernel (Rust `sim`) — BUILT IN PHASE 1 ┴──────────┐
│ discrete-event scheduler keyed by (time, seq) · latency links (per-link FIFO)     │
│ gateway: pre-trade risk checks, throttles, service-time queue                     │
│ feed publisher: MBO / L2 / L1, per-subscriber delay and conflation                │
│ session scheduler: opening/closing auctions, halts (random auction ends)          │
│ latent fundamental value and signal channels (informed / latency-arb agents)      │
│        ▲ commands           │ execution reports        │ market data              │
│ ┌──────┴──────────────┐ ┌───▼────────────────┐ ┌───────▼──────────────────────┐   │
│ │ agents (2 built; 3) │ │ accounting: cash,  │ │ recorders: events, BBO,      │   │
│ │ Rust trait + Python │ │ positions, fees,   │ │ depth snapshots, agent       │   │
│ │ adapter             │ │ PnL decomposition  │ │ ledgers                      │   │
│ └─────────────────────┘ └────────────────────┘ └──────────────────────────────┘   │
└──────────────┬─────────────────────────────────────────────────────────────────────┘
               │ (timestamp, Command) → [Event]
┌──────────────┴───────────── exchange core (Rust `lob`) — BUILT IN PHASE 0 ─────────┐
│ MatchingEngine (one per instrument) · ReferenceEngine (test oracle)               │
│ MboBook (book from the public feed) · Rng (xoshiro256**) · workload generators    │
└───────────────────────────────────────────────────────────────────────────────────┘

historical adapters (phase 5): LOBSTER · Nasdaq ITCH 5.0 · Databento MBO · crypto L2
        → normalised MBO events (+ provenance) → MboBook → replay into the kernel
server + dashboard (phase 6): WebSocket stream of throttled snapshots → React/TypeScript UI
```

Dependencies point downwards only. The engine knows nothing about agents, latency or analytics,
and can be replaced (for example by a C++ port) behind the same command/event interface. The test
oracle and the recorded event digests would then serve as its conformance suite.

## 4. Exchange core (built)

Specified in [MATCHING_RULES.md](MATCHING_RULES.md). Features: price-time priority; limit and market
orders; GTC/IOC/FOK; post-only; partial fills; multi-level sweeps; cancel; cancel/replace with
CME-style priority rules and FIX total-quantity semantics; mass cancel; tick, lot, price-bound and
maximum-size validation; self-trade prevention; continuous, call-auction, halted and closed phases
with a single-price uncross; trade prints; depth, spread, mid and queue position; sequence numbers
and timestamps on every event.

Data structures (`crates/lob/src/book.rs`):

| Structure | Choice | Why |
|---|---|---|
| Orders | `Vec` arena of 64-byte nodes addressed by `u32` slot, with a free list | No per-order allocation in steady state; no pointers, so memory-safe without `unsafe`; one cache line per order |
| Queue per price | Intrusive doubly linked FIFO (head/tail slots) | O(1) append, O(1) removal from the middle (cancels dominate real traffic) |
| Price levels | One sorted `Vec` per side with the best price **last** | Matching pops from the end; inserts near the touch move few elements; contiguous and cache-friendly |
| Id → slot | Hash map with a fixed multiplicative hash | O(1); no random seed, so nothing depends on process randomness |
| Auction market orders | Separate FIFO per side | Priority over all limits at the uncross |

| Operation | Cost |
|---|---|
| New passive order | hash insert + level lookup (O(1) at the best level, else O(log L)) + append |
| Cancel | hash lookup + level lookup + O(1) unlink |
| Each execution | O(1) |
| Modify keeping priority | hash lookup + level lookup |
| Uncross | O(L + eligible orders), L = number of price levels |
| Mass cancel | O(live orders) |

Measured cost per message: about 120 ns median with ~1,000 live orders and 420 ns with ~190,000
live orders ([BENCHMARKS.md](BENCHMARKS.md)). Simulated exchange latency, the tens of microseconds
real venues spend on network and gateways, is therefore a modelled parameter, not an artefact of our
implementation.

## 5. Simulation kernel and latency model (phase 1, built)

Built in `crates/sim` as proposed below; the normative description is [SIMULATION.md](SIMULATION.md).
Decisions made during the build:

* An engine command takes effect when its service *starts*; its results leave when service *ends*,
  with service time = base + per-event cost, so sweeps and uncrosses hold the engine longer.
* The gateway assigns exchange order ids when it forwards a new order. Agents can cancel by their own
  client id before the acknowledgement returns. A cancel that comes too late gets the same
  `UnknownOrder` answer whether the gateway or the engine notices.
* Latency sampling goes through deterministic `ln`/`exp` built from IEEE basic operations, so a seed
  reproduces bit-for-bit on any platform.
* Market data goes through feeds. A private feed gives one agent its own link and book copy. A
  shared feed group gives one location one delivery and one book view, with members in callback or
  snapshot mode. Groups were added after the first benchmark showed per-subscriber fan-out
  dominating: 1,000 agents went from 0.025× to 5.3× real time.

Not built yet: conflated L1/L2 feeds, agents modelled as busy servers, jitter on exchange-internal
service times, and market-order price protection.

**Scheduler.** One binary heap of events ordered by `(time, seq)`, where `seq` is a global insertion
counter, so simultaneous events resolve deterministically. Event kinds: message arrival on a link,
agent timer, session action, fundamental-value update. One simulation runs on one thread.
Parallelism comes from running many independent simulations at once (section 9).

**Path of one order, with every delay explicit:**

```
agent decides at t0
  → compute delay              (agent profile: how long the strategy "thinks")
  → outbound link              base + jitter (e.g. shifted log-normal), drawn from the link's own RNG
                               stream; FIFO per link: arrival = max(sampled, previous arrival + ε),
                               because one session never reorders its messages
  → gateway                    pre-trade risk checks and throttles; single-server queue with a service
                               time, so bursts queue up (message processing bottlenecks)
  → matching engine            service time (a parameter, calibrated to venue data, not to our CPU)
  → execution reports          inbound link back to the owner (own latency)
  → public MBO events          feed publisher → per-subscriber delay; optional conflation into L2
                               snapshots for slow subscribers
  → agent's MboBook            the agent's view, stale by construction
```

* **Cancel/fill races** fall out naturally. A cancel that reaches the engine after the fill gets
  `UnknownOrder` ("too late to cancel"), and the engine already supports this.
* **Latency profiles** per participant and direction, e.g. co-located (single-digit µs), regular
  (hundreds of µs), remote (milliseconds). Defaults are anchored to published orders of magnitude:
  exchange round trips of tens to low hundreds of µs, and Chicago–New Jersey at about 4.0 ms by
  microwave versus 6.55 ms over fibre ([RESEARCH_SURVEY.md](RESEARCH_SURVEY.md) A1, snippet-level).
* **Signal channels** carry a latent fundamental value (or a leading-market price) to each agent with
  its own delay and noise. This drives informed traders and makes latency-arbitrage races possible;
  races are then measured, not assumed.
* **Session scheduler** opens and closes with call auctions and can halt and reopen. Auction ends can
  be randomised, as in Xetra volatility interruptions.
* **Multiple instruments** (for statistical arbitrage): one engine per instrument under the same
  scheduler, so cross-instrument ordering stays deterministic.

This is a strict superset of ABIDES's model (pairwise noisy agent–exchange latency plus agent
computation delay). It adds per-link ordering, gateway queueing and separate feed delays.

## 6. Agents (phase 2, built) and market makers (phase 3)

**Built in phase 2** (`crates/agents`, [AGENTS.md](AGENTS.md); `crates/analysis`,
[STYLIZED_FACTS.md](STYLIZED_FACTS.md)). Every agent in the table below except the market makers,
plus a calibrated background market and a stylised-facts analysis library. Decisions made during
the build:

* The agent trait is the phase-1 kernel trait (`on_start`, `on_market_data`, `on_report`,
  `on_timer`, `on_signal`), not the sketch below. Signals carry integer values; observation noise
  is applied agent-side (`Perception`).
* Limits are enforced twice, as proposed: by the strategy (`Limits`, assuming every open order
  fills, plus capital as a price-dependent position cap) and by the gateway.
* Marketable orders are IOC limits with price protection (at most N ticks through the agent's view
  of the opposite best), which addresses market-order sweeps (K14) agent-side.
* Every strategy agent carries a loss `KillSwitch` (K15).
* Background calibration needed two mechanisms beyond the proposal. Noise traders anchor their
  limit orders to a slow moving average of the mid (latent liquidity), and fundamental traders
  have thresholds spread over 0–6 ticks. With pure zero-intelligence liquidity, impact was only
  marginally concave. The anchor leaves 1-second returns mean-reverting, so every strategy study
  must also run on the zero-intelligence background (STYLIZED_FACTS §5).

The proposal as written before phase 2:

```rust
pub trait Agent {
    fn on_start(&mut self, ctx: &mut Ctx);
    fn on_market_data(&mut self, ctx: &mut Ctx, event: &Event); // delayed public feed
    fn on_execution(&mut self, ctx: &mut Ctx, event: &Event);   // delayed private reports
    fn on_signal(&mut self, ctx: &mut Ctx, value: f64);         // delayed, noisy information
    fn on_timer(&mut self, ctx: &mut Ctx, timer: u64);
}
// Ctx: now(), send(Command), set_timer(at, id), book() -> &MboBook (own delayed view),
//      position(), cash(), open_orders(), rng() (the agent's own stream)
```

Every agent has configurable **capital, risk limits** (position, order size, open orders, notional,
message rate), **trading frequency** (timer period or event-driven, plus compute delay), **latency
profile**, **information quality** (signal noise and delay), **execution style** and **inventory
constraints**. Limits are enforced twice: by the strategy, and by the gateway's pre-trade checks,
which reject orders and report the rejection like a real broker risk system.

| Agent | Behaviour | Anchor |
|---|---|---|
| Noise / zero-intelligence | Poisson arrivals, random side, size and aggressiveness | Background flow; calibrated to stylised facts |
| Informed | Trades on a noisy, delayed fundamental signal | Glosten–Milgrom, Kyle |
| Momentum / mean-reversion | Signals from its own book view and trade flow | – |
| Fundamental | Patient limit orders towards value | – |
| Statistical arbitrage | Pairs or cointegrated instruments | Needs multiple instruments |
| Institutional execution | TWAP, VWAP, POV, implementation-shortfall schedules | Almgren–Chriss; Obizhaeva–Wang |
| Latency arbitrage | Fast profile; picks off stale quotes after signal jumps | Budish et al.; Aquilina et al. |
| Market makers | The strategy ladder below | Section B of the survey |

**Built in phase 3** (`crates/agents/src/mm`, [MARKET_MAKING.md](MARKET_MAKING.md)): the ten steps
below, as presets of one configurable `QuotingStrategy` run by a `MarketMaker` agent. Decisions made
during the build:
* **Formulas first.** GLFT (exact, asymptotic and closed form) and the CKS order-flow imbalance were
  transcribed from the primary texts. GLFT's exact solution is checked against a direct numerical
  HJB solution, and A–S is verified as its first-order expansion in τ. The micro-price follows the
  definition reported by the papers citing Stoikov (2018), checked against brute-force simulation.
* **Model steps quote GLFT's asymptotic solution** (no artificial session end). A–S uses a rolling
  horizon.
* **Model inputs.** σ is estimated online from step 6 on. A and k stay fixed: the online intensity
  estimator measures the whole market's aggressive flow, not one market maker's fills.
* **Comparable risk appetite.** Defaults give steps 2, 4 and 5 the same inventory skew near zero
  inventory.
* **Quote mechanics.** All quotes are post-only, inventory is hard-bounded, a kill switch is fitted,
  and the queue rule is a stickiness heuristic (no explicit fill-probability model).

The ladder as proposed before phase 3:

| # | Strategy | Quote rule (sketch) |
|---|---|---|
| 1 | Fixed spread | mid ± s/2 |
| 2 | Inventory-aware | skew quotes against inventory, hard limits |
| 3 | Dynamic / micro-price | centre on micro-price instead of mid |
| 4 | Avellaneda–Stoikov | reservation price and optimal spread (formulas to verify, survey section D) |
| 5 | Guéant–Lehalle–Fernandez-Tapia | bounded inventory; closed form checked against the ODE solution |
| 6 | Volatility-adjusted | spread scales with an online σ estimate |
| 7 | Order-flow-aware | skew on OFI and imbalance |
| 8 | Adverse-selection-aware | widen or pull after toxic fills (mark-out-based toxicity) |
| 9 | Queue-position-aware | keep or cancel using `queue_ahead` and fill probability |
| 10 | Multi-level | ladder of sizes and prices |

Parameters: risk aversion γ, inventory limits, quote widths, volatility estimator, update frequency,
post-only usage and STP mode. Python agents use an adapter around the same trait. They are slower
(a callback per event) and suit low-frequency strategies (risk K6).

## 7. Python research layer (phase 4)

Proposed:
* `hftsim` Python package via PyO3/maturin: configure a market (instruments, session schedule, agent
  population, latency profiles), run it, get results as Arrow/Parquet.
* Experiment runner: parameter grids × seed lists; independent runs in parallel processes; **common
  random numbers** (the same seeds across strategies) for paired comparisons with less variance;
  confidence intervals on every reported statistic.
* Manifest per run: config hash, seeds, crate versions, git commit, event digest. Re-running the
  manifest must reproduce the digest.
* Notebooks per research question; the final report separates simulated and historical evidence.

**Built.** One library, two ways in. Phase 4 first built a process boundary, because PyO3,
maturin and pyarrow needed downloads (RISKS K8). The bindings followed on 2026-11-16, once those
were installed. Jupyter is not used.

* **`runner` library and `hftsim` binary** (`crates/runner`). `hftsim run --flag value …` builds
  one market (background, informed traders, latency arbitrageurs, any number of ladder market
  makers, fees, latency locations), runs it and prints one JSON document. The document holds the
  manifest, market metrics, per-agent PnL with its exact decomposition, races and queue statistics
  ([METRICS.md](METRICS.md)). Unknown flags are errors. `export.rs` lays out the event log, top of
  book and participants as Arrow columns.
* **Native extension `hftsim._native`** (`python/native`, PyO3 0.25, built by maturin with the
  stable ABI for Python ≥ 3.10).
  * It calls the same library from the same flags, so it reproduces the binary's digest (tested).
  * The simulation runs with the GIL released, so Python threads run markets in parallel.
  * It is a Cargo workspace of its own. The core workspace keeps zero external dependencies and
    still builds offline.
  * Its only unsafe code is the FFI glue that PyO3's macros generate.
* **Arrow and Parquet** (`hftsim.arrow`, `python -m hftsim.export`).
  * The extension hands over little-endian value buffers and validity bitmaps. pyarrow wraps them
    as arrays without visiting each value.
  * The tables are `events` (one row per engine event, wide, with typed nulls), `book` (top of
    book) and `owners` (participants).
  * Parquet output uses zstd: 3.0 million events of a 180 s market come to 24 MB.
* **Python package** (`python/hftsim`, standard library only):
  * `Scenario` is a frozen dataclass mirroring every flag. Its defaults are tested to equal the
    binary's, and it always passes every flag, so a manifest specifies its run completely.
  * `run_many` runs a grid × seeds in parallel processes (or threads with `use_native=True`) and
    caches results keyed by the build and the flags.
  * `verify` re-runs a manifest and compares digests.
  * `stats` provides Student-t, paired, Wilson and bootstrap intervals.
  * `research` holds the eight experiments.
* **Manifest:** the flags, an FNV-1a config hash (flag order and output path excluded), seed,
  version, git commit (`unknown`: the project is not a git repository), event digest, trades and
  engine commands. Re-running a manifest reproduces the digest. This is tested, and every generated
  report re-runs one of its runs.
* **Reports instead of notebooks.** `python -m hftsim.research rqN` regenerates `reports/rqN.md` and
  `reports/rqN.manifests.json` from the runs. Each report states its design and labels itself as
  simulation evidence.

Each path has its use:
* **Grids** use the process boundary. It gives isolation, a parallel run per core with no shared
  state, reproducibility from the command line alone, and it needs nothing beyond the standard
  library.
* **Event-level work** uses the extension and pyarrow.

**Not built: Python-written agents.** An agent with Python callbacks inside the event loop needs a
safe adapter around `sim::Agent` (RISKS K6) and is left for a later phase.

## 8. Metrics

Market quality and execution: bid/ask spread, depth, effective spread 2D(P − M)/M, realized spread
2D(P − M₊ₕ)/M at configurable horizons (default: the Rule 605 set of 50 ms, 1 s, 15 s, 1 min and
5 min), price impact (effective minus realized), VWAP, TWAP, implementation shortfall
(delay / execution / opportunity components), Amihud illiquidity, order-flow imbalance,
cancellation and fill rates, fill probability against queue position, realized volatility, and
latency-race counts. Trade direction comes from the engine's `aggressor` field, so simulated data
needs no trade-signing heuristics.

Definitions as built, with sources and verification status: [METRICS.md](METRICS.md).

Market-maker PnL is reported as a decomposition that must sum exactly to total PnL. For each fill,
with direction D (+1 buy, −1 sell), price P, quantity Q and mid M at the fill:

* spread capture = D·(M − P)·Q;
* adverse selection at horizon h = D·(M₊ₕ − M)·Q (mark-outs);
* inventory revaluation = the remaining mark-to-market change of held inventory;
* fees and rebates.

Menkveld (2013) found exactly this split in real data: earnings on the spread, losses on positioning.

## 9. Determinism and reproducibility

| Threat | Measure |
|---|---|
| Wall-clock reads | The engine and kernel take time only from events; no `Instant` or `SystemTime` |
| Random state | `Rng::stream(master_seed, entity_id)` per agent and link; adding an agent does not shift the others' streams |
| Hash iteration order | Deterministic hasher; outputs never depend on map iteration (mass cancel sorts ids) |
| Thread scheduling | One simulation per thread; no shared mutable state across runs |
| Floating point | Integer core; strategies avoid platform-dependent transcendental functions in decision paths, or accept per-platform determinism (risk K5) |
| Silent behaviour change | Event-stream digests in tests; pre-allocation-independence test; mutation testing |

## 10. Historical data (phase 5)

* Adapters normalise each source into the MBO schema, with provenance: LOBSTER (message files with
  event types 1–7, validated against its order-book files), Nasdaq ITCH 5.0, Databento MBO, and crypto
  exchange L2 (L2 only: queue position is not observable).
* Book reconstruction reuses `MboBook` and is checked against vendor snapshots where available.
* Replay modes: (a) pure replay, where passive strategies need an explicit queue/fill model with
  conservative assumptions because the historical market cannot react; (b) hybrid, where historical
  background flow is replayed into the engine and live agents trade against it, so their impact is
  real but the background does not respond.
* OHLCV data is not accepted as validation for any HFT result. Results always state which data level
  (L1, L2, MBO) they rest on.

**Status (2026-11-17, part 1).** Built in `crates/data` ([HISTORICAL_DATA.md](HISTORICAL_DATA.md)):
* **LOBSTER adapter:** reads and writes the files, rejecting malformed rows by line.
* **Book reconstruction checked against the vendor's snapshots.** It deviates from the plan above:
  instead of `MboBook` it uses its own rebuilder, because LOBSTER files reference orders placed
  before the file starts. Those are tracked as per-level seeds that the snapshots reconcile.
  `MboBook` serves as the independent check on simulated files.
* **Provenance** on every result.
* **Pure replay (mode a):** a conservative FIFO queue model, checked against the exchange's own
  fills in the simulator.
* **Real order-level data (2026-11-18): Bitstamp's public feed.** `data::l3` rebuilds an
  order-level book and compares it, order by order, with the exchange's own order-level
  snapshots. This is a second deviation from the plan: it is a crypto *order-level* adapter, not
  L2. On a 19-minute BTC/USD recording every one of 1,136 snapshots is reproduced. The phase-5
  exit criterion is therefore met for this venue (HISTORICAL_DATA §6).
* **Pure replay against real fills.** The recording is exported as LOBSTER messages plus taker
  files, and `data::replay::leave_one_out_with` replays each real resting order through the
  stream without itself. On 172,862 orders the model was never optimistic (HISTORICAL_DATA §6).
  * The conservative rules alone find a third of real fills on time.
  * A taker-remainder rule, scored against history rebuilt without each order, finds 292 of
    298 and 99.6% of the volume.
  * The check found and fixed an optimistic trade-through rule.

Not built: ITCH 5.0, Databento and crypto L2 adapters, and hybrid replay (mode b). No real
equity file has been validated: LOBSTER's samples now need an academic application.

## 11. Dashboard (phase 6)

* Backend streams throttled snapshots (≤ 30 Hz) over WebSocket: top-N depth ladder, trade tape,
  spread and mid series, agent positions, PnL and market-maker inventory, strategy statistics. It
  accepts control messages (start, pause, step, speed, parameters).
* Frontend: React + TypeScript (Vite) with canvas-based charts and order-book ladder for high update
  rates.
* Prerequisite: Node.js and the backend crates (an async runtime and a WebSocket server). The
  development machine has neither, so both need a download.

## 12. Language choice: Rust

| Criterion | Rust | C++ |
|---|---|---|
| Available here | Rust 1.99 installed and working | No compiler installed (no MSVC, MinGW, Clang or CMake) |
| Memory safety in the core | Arena + indices checked by the compiler; `#![forbid(unsafe_code)]` in `lob` | Intrusive lists and erase-during-iteration are classic sources of undefined behaviour |
| Performance | 120–420 ns per message median (measured) | Comparable for the same data layout (same LLVM back end with Clang); not measured here |
| Determinism tooling | Deterministic build; std hasher is randomly keyed, so we use our own (handled) | Same issue with `std::unordered_map` iteration order |
| Python bindings | PyO3 + maturin (mature) | pybind11 / nanobind (mature) |
| Build and test on Windows | cargo: one command for build, tests and benches | CMake + vcpkg/conan + a separate test framework |
| Ecosystem / hiring | Smaller in HFT | Industry standard in HFT |

**Decision: Rust.** The deciding factors are memory safety in exactly the kind of code where C++
fails (intrusive structures with frequent removal), one-command builds and tests on the development machine,
and good Python bindings.

**Why there is no C++ head-to-head:** no C++ toolchain is installed, and installing one (MSVC Build
Tools or LLVM-MinGW, gigabytes) is a download that should not happen without asking. The benchmark
also shows that data layout dominates. Going from a 1,000-order to a 190,000-order book costs 3.5× on
the median message, because of cache misses, which no language choice removes. If a comparison is
wanted, the engine sits behind a narrow command/event interface, and the reference model plus the
recorded event digests make a ready conformance suite for a C++ port.

## 13. Research questions → experiments

| # | Question | Experiment (simulation stage) | Historical stage |
|---|---|---|---|
| 1 | Inventory risk and optimal quoting | Sweep γ and inventory limits for A–S / GLFT market makers; measure PnL distribution, inventory variance, spread capture vs adverse selection | Replay MBO with the strategy's quotes under a queue model |
| 2 | When adverse selection makes market making unprofitable | Vary informed-trader share, signal quality and latency; find the break-even frontier of spread revenue vs mark-out losses | Mark-outs of passive fills in MBO data |
| 3 | Queue position and execution probability | Fill probability as a function of `queue_ahead`, depth and order-flow regime; compare with queue-reactive model predictions | Same estimate from MBO data (LOBSTER/ITCH) |
| 4 | Latency and expected PnL | Vary the market maker's latency rank against latency arbitrageurs; PnL, race losses, cancel-too-late rates | Not directly testable; compare race statistics with Aquilina et al. |
| 5 | Informed traders and market-maker profitability | Vary informed population and information horizon; decompose PnL | Mark-outs around information events |
| 6 | OFI and short-term price moves | Regress mid changes on OFI over horizons and depth regimes, in simulation | Same regression on historical L1/MBO (the meaningful test) |
| 7 | Adaptive vs static market makers | Paired Monte Carlo with common random numbers across regimes (volatility, toxicity) | Walk-forward on historical replay |
| 8 | Competition and spreads | Number of market makers vs spread, depth and per-maker PnL | Cross-sectional comparison only; causal claims stay in simulation |

## 14. Roadmap

| Phase | Deliverable | Exit criteria |
|---|---|---|
| 0 ✔ | Exchange core, rulebook, research survey, risks, tests, benchmark | All tests green; differential fuzzing and mutation testing pass; benchmarks with noise floor |
| 1 ✔ | Simulation kernel: scheduler, latency links, gateway, feed publisher, session scheduler, signals | Deterministic replay digests; latency distributions reproduce configured profiles; cancel/fill race tests ([TESTING.md](TESTING.md) phase 1) |
| 2 ✔ | Agent framework and background flow | Stylised facts reproduced (spread distribution, order-sign autocorrelation, cancel ratios, square-root impact) within stated tolerance: met on the calibration seeds and two holdout seed sets, pooled δ 0.53–0.58 and γ 0.46–0.54 ([STYLIZED_FACTS.md](STYLIZED_FACTS.md) §4) |
| 3 ✔ | Market-making ladder | Each strategy unit-tested on synthetic paths; formulas transcribed and verified first: met ([MARKET_MAKING.md](MARKET_MAKING.md) §1, §5) |
| 4 ✔ | Python layer, metrics, Monte Carlo, notebooks (built as a runner library with a command line, PyO3 bindings with Arrow/Parquet export, a standard-library Python package and generated reports; §7) | Metric unit tests on hand-computed cases; PnL decomposition identity holds exactly: met. Every agent's decomposition equals its exchange ledger to the last fee unit, over the whole run and over a window ([METRICS.md](METRICS.md) §4) |
| 5 (in progress) | Historical adapters and validation | Reconstruction matches vendor snapshots; provenance on every result. So far: met on simulated LOBSTER files and on real Bitstamp BTC/USD order-level data (all 1,136 snapshots of a 19-minute recording), where the replay fill model was also checked against real fills (never optimistic); not on a real equity file (§10 status) |
| 6 | Server and dashboard | Sustains a live simulation at target update rate without blocking it |
| 7 | Research report | Every claim tagged as simulation or historical evidence |
