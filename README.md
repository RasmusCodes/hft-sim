# hft-sim

A research platform for electronic market microstructure: a deterministic exchange, a latency-aware
discrete-event simulator, autonomous trading agents, a ladder of market-making strategies, a Python
research layer and historical-data replay. A live dashboard is planned but not built.

**Status: phases 0–4 complete; phase 5 (historical data) in progress.** Built, specified and tested
so far:

* **Phase 0:** the exchange core, a limit order book and matching engine, plus the architecture
  proposal, research survey and risk register.
* **Phase 1:** the simulation kernel and latency model. Every message between a trader and the
  exchange takes a sampled amount of time, and no trader ever sees the true book.
* **Phase 2:** trading agents (noise, fundamental, informed, momentum, mean reversion, pairs,
  institutional execution, latency arbitrage) and a background market calibrated to five stylised
  facts, with a library that measures them.
* **Phase 3:** a ten-step market-making ladder:
  * fixed spread, inventory skew and micro-price;
  * Avellaneda–Stoikov and Guéant–Lehalle–Fernandez-Tapia;
  * volatility, order-flow, adverse-selection, queue-position and multi-level variants.

  Its formulas were transcribed from the papers and checked numerically before use.
* **Phase 4:** the research layer:
  * metrics from the exchange's own logs, including SEC Rule 600/605 spreads, an exact PnL
    decomposition, latency races, queue fill rates and order-flow imbalance;
  * the `hftsim` command-line runner, with a manifest per run;
  * a Python package for parameter grids × common seeds with confidence intervals. It runs on the
    standard library alone, and has optional PyO3 bindings that export a run's event log to
    Arrow and Parquet;
  * generated reports on the eight research questions.

* **Phase 5, part 1:** historical data ([docs/HISTORICAL_DATA.md](docs/HISTORICAL_DATA.md)):
  * a LOBSTER adapter that reads and writes the files;
  * book reconstruction checked row by row against the vendor's snapshots, including the
    pre-existing book a file never shows;
  * provenance on every dataset;
  * a conservative FIFO replay fill model. Checked against the exchange's own fills of about
    3,700 probe orders, it was never optimistic;
  * **real order-level data from Bitstamp's public feed.** A recorder and converter, and an
    order-by-order check of the rebuilt book against the exchange's own snapshots. On 19 minutes
    of BTC/USD (365,180 order events), all 1,136 snapshots are reproduced, none unexplained.
    Deliberately broken copies of the data are caught, down to one satoshi on one order;
  * **the replay fill model against real fills.** Each of 172,862 resting Bitstamp orders was
    replayed through the stream without itself. In every variant the model never filled an order
    the exchange left unfilled, nor one earlier or for more.
    * The conservative rules alone find a third of real fills on time.
    * Adding a taker-remainder rule (Bitstamp publishes takers' orders), scored against history
      rebuilt without each order, finds 292 of 298 and 99.6% of the volume.
    * The check also found and fixed an optimistic trade-through rule.
  * **a real equity day from Nasdaq's ITCH 5.0 feed** (Nasdaq BX, 2019-12-30: 29.2 million
    messages, 8,906 stocks). Every consistency check passes, and an independent rebuild agrees
    after 1.6 million messages. 0.2% of BX's executions break arrival order (cause not
    identified), so there the replay fill model is nearly, not strictly, conservative: 0.08% false
    fills on 801,154 orders.

  No real LOBSTER file has been validated: LOBSTER's samples now need an academic application.
  Hybrid replay and the Databento adapter are not built
  ([roadmap](docs/ARCHITECTURE.md#14-roadmap)).

> Results from this simulator are simulation results. No strategy will be described as profitable
> because it does well here; historical-market validation is a separate, labelled stage with its own
> limits ([architecture §10](docs/ARCHITECTURE.md#10-historical-data-phase-5), [risks K1–K3](docs/RISKS.md)).

**Website:** [rasmuscodes.github.io/hft-sim](https://rasmuscodes.github.io/hft-sim/) presents the
project and renders all the documents below.

**What this repository holds.** The source, the tests, the documents, and the generated research
reports with the manifests that reproduce every run. It does not hold:
* build output;
* the Python environment and run cache;
* any market data. The Bitstamp recording behind
  [docs/HISTORICAL_DATA.md](docs/HISTORICAL_DATA.md) §6 is not redistributed: Bitstamp's terms
  for its public feed were not checked. `python -I -m hftsim.bitstamp record` records a fresh
  one, and the same commands then rerun every check on it. The Nasdaq ITCH day of §7 is
  Nasdaq's public sample file, to be downloaded from Nasdaq.

## What the exchange core does

`crates/lob` is a dependency-free Rust crate, `#![forbid(unsafe_code)]`, that builds offline.

* Price-time priority matching; limit and market orders; GTC, IOC, FOK; post-only; partial fills and
  multi-level sweeps; trade prints.
* Cancel; cancel/replace with CME-style priority rules and FIX total-quantity semantics; mass cancel.
* Tick-size, lot-size, price-bound and maximum-size validation with a defined reject precedence.
* Self-trade prevention (cancel resting / aggressor / both).
* Trading phases: continuous, call auction with a single-price uncross, halt, closed.
* Bid/ask, spread, mid, depth, queue position; sequential order ids, trade ids and event sequence
  numbers; nanosecond timestamps.
* A **pure function of its input**: no clock, no randomness, no I/O. The same commands always give the
  same events.
* Output: private execution reports plus an anonymous market-by-order feed, from which `MboBook`
  rebuilds the full book exactly as a participant's feed handler would.

Rulebook: [docs/MATCHING_RULES.md](docs/MATCHING_RULES.md).

## What the simulation kernel does

`crates/sim` (also dependency-free and `unsafe`-free) runs the exchange inside a discrete-event
simulation:

* **Every hop is timed.** Agent thinking time, network out, gateway, internal hop, engine queue and
  network back each have their own delay. Each agent also has its own market-data and signal latency.
  Delays come from constant, uniform, log-normal, Pareto or spiky distributions.
* **Links never reorder.** Gateways and engines handle one message at a time, so bursts queue.
* **Agents only see what reached them.** That means their own execution reports and their delayed
  feed, from which the kernel maintains each agent's own book view. Cancel/fill races,
  "too late to cancel" and stale quotes arise naturally.
* **Feeds can be private or shared.** A private feed has its own link and book copy. A shared feed
  group stands for one location's multicast feed: one delivery and one book view for all members.
  Members either get a callback on every packet or just read the view when they act.
* **The gateway** has throttles and pre-trade risk limits, and lets agents use their own order ids,
  even to cancel before the acknowledgement arrives.
* **Exchange-side ledgers** keep integer accounting with maker/taker fees.
* **Session schedules** support opening and closing auctions with random end times.
* **Signal sources** (news, a leading market, a fundamental value) reach each agent with its own
  delay.
* **Bit-reproducible from one seed.** Every entity has its own random stream, and sampling uses no
  platform math library. A digest fingerprints each run.

Specification: [docs/SIMULATION.md](docs/SIMULATION.md).

## What the agents do

`crates/agents` (dependency-free, `unsafe`-free) holds the trading agents. Each one reads only its
own delayed book view, its own fills and its delayed signals:

* **Strategies:** informed, momentum, mean-reversion, pairs (two instruments), latency arbitrage, and
  institutional execution with TWAP, VWAP, Almgren–Chriss and POV schedules plus
  implementation-shortfall reports.
* **Safeguards:** every strategy has risk limits (position assuming open orders fill, order size,
  open orders, capital) and a loss kill switch. Marketable orders carry price protection.
* **Background market:**
  * noise traders whose limit orders anchor to a slow moving average (latent liquidity);
  * fundamental traders with spread-out thresholds;
  * execution agents splitting Pareto-sized parent orders.

`crates/analysis` measures stylised facts from the exchange's event log. On 8 calibration seeds and
two sets of 8 holdout seeds, the background meets all five targets on pooled estimates:
* about 98% of resting orders cancelled;
* at most 1.3% of submitted quantity executed;
* a one-tick spread 94–98% of the time;
* order-sign autocorrelation decaying as lag^−γ with γ 0.46–0.54;
* metaorder impact growing as Q^δ with δ 0.53–0.58.

21 of the 24 seeds also meet all five individually.

The anchor that makes impact concave also makes 1-second returns mean-revert. That deviation is
documented, and strategy studies must also run on a zero-intelligence background. Details:
[docs/AGENTS.md](docs/AGENTS.md), [docs/STYLIZED_FACTS.md](docs/STYLIZED_FACTS.md).

## What the market makers do

`crates/agents/src/mm` contains the market-making code:
* **Models.** Exact GLFT quotes with an inventory bound, their asymptotic and closed-form limits, and
  Avellaneda–Stoikov.
* **Estimators**, all fed only from the market maker's own view: volatility, fill intensity,
  order-flow imbalance, mark-outs and a micro-price.
* **The strategy ladder:** one configurable quoting rule with ten presets, each adding one idea to
  the one before.
* **A `MarketMaker` agent** that reconciles quotes with its live orders: post-only orders,
  priority-keeping size cuts, a hard inventory bound and a kill switch.

How the formulas were checked: the GLFT formulas were transcribed from the paper. Its exact solution
matches a direct numerical solution of the Hamilton–Jacobi–Bellman equation to 10⁻⁶ ticks, and
Avellaneda–Stoikov is verified as its first-order expansion in time. Every strategy is unit-tested
on synthetic snapshots and run in the simulator under exchange-side checks.

In a first smoke run, 39 of 40 step-by-scenario results have a positive mean PnL, and the 40th is
indistinguishable from zero. The reasons do not carry over to real markets: the background has no
informed aggressive flow, and its returns mean-revert. Those results are not evidence of
profitability ([docs/MARKET_MAKING.md](docs/MARKET_MAKING.md) §6–7).

## Research layer

* **Metrics** (`analysis::metrics`, definitions and sources in [docs/METRICS.md](docs/METRICS.md)):
  * quoted, effective and realized spreads and price impact at the Rule 605 horizons;
  * a PnL decomposition into spread capture, adverse selection, inventory and fees. It equals the
    exchange ledger to the last fee unit, over a whole run or a window;
  * implementation shortfall, realized volatility, Amihud, Roll, order-flow imbalance regressions;
  * fill probability by queue position;
  * latency races, including the cancels the gateway refused, and cancels sent too late.
* **`hftsim` runner** (`crates/runner`). `hftsim run --mm 7 --informed 10 --arbs 3 …` runs one
  market and prints JSON: a manifest (flags, config hash, seed, version, event digest), market
  metrics and each strategy agent's PnL with its decomposition. Re-running the flags reproduces the
  digest.
* **Python package** (`python/hftsim`, standard library only):
  * `Scenario` mirrors every flag;
  * `run_many` runs grids × seeds in parallel processes with a cache;
  * `verify` re-runs a manifest;
  * `stats` provides t, paired, Wilson and bootstrap intervals.
* **Native bindings and Parquet (optional).** `hftsim._native` is a PyO3 extension built with
  maturin from `python/native`, and the core crates still build without external dependencies.
  * It runs the same `runner` library in-process, with the GIL released, and reproduces the
    binary's digest.
  * `hftsim.arrow` turns a run into pyarrow tables:
    * `events`: one row per engine event;
    * `book`: the top of book;
    * `owners`: every participant's role.
  * `python -m hftsim.export DIR [flags]` writes those tables as Parquet, plus the JSON report.
  * Example: a 180 s market with 30 informed traders gives 3.0 million events, 24 MB of Parquet.
* **Experiments.** `python -m hftsim.research` runs the eight research questions on both
  backgrounds with common seeds and writes [reports/](reports/README.md). The reading of the reports
  is in [docs/RESEARCH_RESULTS.md](docs/RESEARCH_RESULTS.md).

Generated reports replace the proposed notebooks, and Python-written agents are not supported yet
([architecture §7](docs/ARCHITECTURE.md#7-python-research-layer-phase-4)).

## Evidence

211 Rust tests pass (`cargo test --offline`), plus one slow acceptance test run with `--ignored`,
and there are no compiler warnings. 25 Python tests pass (`python -m unittest discover -s tests`
in `python/`). Four of them need the native extension and pyarrow and are skipped without them.
Details in [docs/TESTING.md](docs/TESTING.md) and [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

* **Exchange correctness:**
  * 52 hand-computed rulebook scenarios;
  * differential fuzzing against an independent naive reference implementation, with every
    cross-check after every command (about 18M commands in the scaled run);
  * the book rebuilt from the public feed must match the engine's after every command;
  * mutation testing: 16 of 18 planted bugs caught, and the other 2 proven unobservable.
* **Kernel correctness:**
  * exact hand-computed timings for every hop; cancel/fill races, stale book views, throttles and
    risk limits;
  * after a random market settles, every agent's own view of its positions, cash, orders and the
    book equals the exchange's;
  * the same seed reproduces a run's digest, including 10 simulated seconds with 1,000 agents;
  * mutation testing: all 13 planted kernel bugs caught.
* **Agent correctness:**
  * limits checked against the exchange's own fills;
  * hand-worked races, value caps and kill-switch trips, plus execution schedules measured from
    exchange fills;
  * the background market's books balance once it goes quiet;
  * mutation testing: all 15 planted agent and analysis bugs caught.

  A flaw in the mutation script itself was found and fixed on 2026-11-15: restored files kept old
  timestamps, so cargo could skip rebuilding them. Every mutant was rerun with the fix.
* **Background realism:** the stylised-fact targets above, checked by an acceptance test on 8
  seeds; known deviations in [docs/STYLIZED_FACTS.md](docs/STYLIZED_FACTS.md) §5.
* **Market-making correctness:**
  * the GLFT exact solution matches a direct numerical HJB solution, and Avellaneda–Stoikov matches
    its first-order expansion;
  * each estimator recovers a known truth;
  * all ten ladder steps pass hand-computed and sweep tests;
  * in the simulator, the market makers trade only as maker and inside their inventory bound, as
    checked on the exchange log;
  * mutation testing: all 19 planted market-making bugs caught (one only after a test gap it
    exposed was closed).
* **Metric correctness:**
  * 9 hand-computed metric tests through the real matching engine;
  * every agent's PnL decomposition equals its exchange ledger to the last fee unit, over a whole
    run and over a window, in a full market with fees;
  * a sniping race measured end to end, including the maker's cancel that the gateway refused;
  * mutation testing: all 20 planted metric bugs caught.
* **Research reproducibility:** each run's flags and event digest are saved, the Python defaults
  are tested to equal the binary's, and every generated report re-runs one of its runs to confirm
  the digest.
* **Historical-data correctness:**
  * a simulated day written as LOBSTER files reads back with every one of 165,052 rows matching;
  * its top of book equals the engine feed handler's at every event time;
  * a file cut mid-session reconciles only its first row;
  * the replay fill model, checked against the exchange's own fills of about 3,700 probe orders,
    never filled an order early or one the exchange did not fill;
  * **real data**, 19 minutes of Bitstamp BTC/USD (365,180 order events): the order-level rebuild
    reproduces all 1,136 exchange snapshots, none unexplained. A break test catches each of seven
    deliberate errors, down to one satoshi on one order. An independent Python replay agrees on
    the 2,974 crossed-book episodes;
  * the replay fill model, run leave-one-out on 172,862 real resting orders in four variants,
    filled none of the 172,564 that never traded, and no order earlier or for more than it
    really filled;
  * a Nasdaq BX ITCH day (29.2 million messages): each message type's length matches its layout,
    and the book rebuild finds no unknown or reused reference, stock mismatch, oversize execution,
    backward timestamp or crossed book. The best 10 levels agree with LOBSTER's independent
    rebuild after 1,608,422 messages;
  * mutation testing: all 44 planted data bugs caught. 22 are in the LOBSTER reader and the
    replay model (including its leave-one-out check, taker-remainder rule and counterfactual),
    two of them only after the test gaps they exposed were closed. 12 are in the order-level
    check, one of them only by a test added while the mutant was written. 10 are in the ITCH
    reader, 4 of them caught only by a test written first, because no earlier test made those
    inconsistencies happen.
* **Engine performance** (Ryzen AI 7 PRO 350 laptop):
  * about 8.6 M messages/s with ~1,000 resting orders and 2.4 M messages/s with ~190,000;
  * median 120–420 ns per message, p99 0.27–1.1 µs;
  * slow-sample tails match the same harness's noise floor (environmental, not engine).
* **Kernel performance** (~20,000 trader actions per simulated second):
  * on shared feed groups, 1,000 agents run 5.3× faster than real time, 200 agents 7.5× and 20 agents
    10.5×;
  * on private feeds, where every agent keeps its own book copy, 20 agents run 3.4× faster than real
    time but 200 agents only 0.26×;
  * the shared-group fix made the 1,000-agent market about 200× faster and is verified to reproduce
    private-feed results exactly when latencies are equal.

## Quick start

Rust 1.85+ required (developed on 1.99, `x86_64-pc-windows-gnu`).

```bash
cargo test --offline
```

```bash
cargo run --release --offline -p sim --example demo
```

```bash
cargo bench --offline -p lob --bench engine_bench
```

```bash
cargo bench --offline -p sim --bench kernel_bench
```

Stylised facts of the background market (one seed, then 8 seeds pooled; about 25 s and 35 s):

```bash
cargo run --release --offline -p agents --example stylized_facts
```

```bash
cargo run --release --offline -p agents --example stylized_facts -- --seeds 8
```

The phase-2 acceptance test (8 seeds × 10 simulated minutes, about a minute):

```bash
cargo test --release --offline -p agents --test agents -- --ignored
```

The market-making ladder on both backgrounds, with informed traders added (a few minutes):

```bash
cargo run --release --offline -p agents --example mm_ladder -- --seeds 4 --minutes 5 --informed 10
```

One market through the runner (JSON on standard output; `hftsim help` lists every flag):

```bash
cargo build --release --offline -p runner
```

```bash
target/release/hftsim run --seed 1 --secs 120 --mm 7 --informed 10 --arbs 3
```

The Python layer (Python 3.10+, standard library only; run from `python/`). First the tests, then
the eight research questions (8 seeds × 180 s × both backgrounds: 624 runs, about 40 minutes on
4 workers; each run holds about 0.4 GB while it runs). Results are cached in `python/.cache`, so
regenerating the reports takes minutes until the binary is rebuilt:

```bash
python -m unittest discover -s tests
```

```bash
python -m hftsim.research all --seeds 8 --secs 180 --warmup 60 --workers 4
```

The native bindings and Parquet export (optional; downloads maturin and pyarrow from PyPI and PyO3
from crates.io). Run from `python/`, in PowerShell:

```powershell
python -m venv .venv; .\.venv\Scripts\python -m pip install -r requirements-optional.txt
```

```powershell
$env:VIRTUAL_ENV = "$PWD\.venv"; .\.venv\Scripts\maturin develop --release
```

```powershell
.\.venv\Scripts\python -m hftsim.export runs\demo --seed 1 --secs 60 --mm 7 --informed 10
```

Historical data: write a simulated day as LOBSTER files, check a LOBSTER day against its own
snapshots, and test the replay fill model against the exchange (examples in
[docs/HISTORICAL_DATA.md](docs/HISTORICAL_DATA.md) §9):

```bash
target/release/hftsim replay-check --probes 6 --probe-interval-ms 300 --seed 2026 --secs 120 --mm 7 --informed 10
```

Record real order-level data from Bitstamp's public feed and check the rebuilt book against the
exchange's snapshots. The commands run Python in isolated mode (`-I`), because they read data from
outside. Isolated mode ignores the current folder, so install the package first: in the virtual
environment above, `maturin develop --release` installs it, or `pip install -e .` from `python/`.
Then:

```bash
python -I -m hftsim.bitstamp record --minutes 30 --cap-mb 200 --out ../data/bitstamp/btcusd-run
```

```bash
python -I -m hftsim.bitstamp check ../data/bitstamp/btcusd-run
```

Then check the replay fill model against the recording's real fills (leave-one-out):

```bash
python -I -m hftsim.bitstamp replay ../data/bitstamp/btcusd-run
```

A Nasdaq ITCH 5.0 sample day (download it yourself from emi.nasdaq.com/ITCH/; the BX file of
2019-12-30 is 391 MB): every stock's book and checks, then the replay check on the 8 most-traded
stocks (about 3 minutes):

```bash
python -I -m hftsim.itch check ../data/itch/20191230-bx/20191230.BX_ITCH_50.gz --replay 8
```

Scaled fuzzing and mutation testing (PowerShell):

```powershell
$env:LOB_FUZZ_SEEDS=1500; $env:LOB_FUZZ_STEPS=4000; cargo test --release --offline --test differential
```

```powershell
powershell -File tools\mutation_test.ps1
```

(About an hour for all 129 mutants; `-Only lob|sim|agents|mm|metrics|data` or `-Ids A13,S02` for
a subset.)

## Layout

```
hft-sim/
├── Cargo.toml                    workspace
├── crates/lob/                   exchange core
│   ├── src/engine.rs             MatchingEngine: commands → events
│   ├── src/book.rs               arena + intrusive FIFO queues + sorted levels
│   ├── src/auction.rs            O(levels) uncross price determination
│   ├── src/reference.rs          naive reference implementation (test oracle)
│   ├── src/mbo.rs                book reconstruction from the public feed
│   ├── src/event.rs, types.rs, instrument.rs
│   ├── src/rng.rs                xoshiro256** with per-entity streams
│   ├── src/workload.rs           fuzz and benchmark command generators
│   ├── tests/                    scenarios, differential fuzzing, shared harness
│   └── benches/engine_bench.rs   throughput, latency, noise floor
├── crates/sim/                   simulation kernel
│   ├── src/simulation.rs         event loop, builder, engine queues, feed and report routing
│   ├── src/latency.rs            latency models, links that never reorder, latency profiles
│   ├── src/gateway.rs            sessions: throttle, risk checks, client-id mapping, ledgers
│   ├── src/agent.rs              Agent trait, Ctx, the agent's own order/position view
│   ├── src/feed.rs               private feeds and shared feed groups (callback / snapshot)
│   ├── src/detmath.rs            platform-independent ln/exp and samplers
│   ├── src/messages.rs, accounting.rs, session.rs, signal.rs, queue.rs
│   ├── src/agents.rs             scripted and zero-intelligence test agents
│   ├── tests/kernel.rs           timing, races, staleness, risk, determinism, consistency
│   ├── examples/demo.rs          one simulated second of a small market
│   └── benches/kernel_bench.rs   simulation capacity
├── crates/agents/                trading agents
│   ├── src/common.rs             limits, schedules, perception, EWMA, book view, order helpers, kill switch
│   ├── src/noise.rs, fundamental.rs, informed.rs, technical.rs, pairs.rs, latency_arb.rs
│   ├── src/execution.rs          TWAP / VWAP / Almgren–Chriss / POV, parent reports, metaorder source
│   ├── src/presets.rs            calibrated background market and value process
│   ├── src/mm/                   market making: models.rs (GLFT, A–S), estimators.rs, strategy.rs
│   │                             (the ladder), agent.rs (MarketMaker), linalg.rs
│   ├── tests/agents.rs           limits at the exchange, races, schedules, kill switch, stylised facts
│   ├── tests/market_makers.rs    every ladder step in the market, scripted inventory and kill-switch cases
│   ├── examples/stylized_facts.rs  measure the background against the phase-2 targets
│   └── examples/mm_ladder.rs     run the ladder on both backgrounds, paired across seeds
├── crates/analysis/              stylised facts from the event log (spread, flow, signs, returns, impact)
│   └── src/metrics.rs            spreads, PnL decomposition, shortfall, OFI, queue fills, races
├── crates/agents/tests/metrics.rs  decomposition = ledger for every agent; a race end to end
├── crates/data/                  historical data: lobster.rs (read, write, rebuild, validate),
│                                 l3.rs (order-level rebuild checked against order-level snapshots),
│                                 itch.rs (Nasdaq ITCH 5.0: reader, book, checks, LOBSTER form),
│                                 provenance.rs, replay.rs (FIFO queue fill model)
├── crates/runner/                library + `hftsim` binary: one market per run → JSON with manifest;
│                                 export.rs: event log and top of book in Arrow layout;
│                                 lobster-export, lobster-check, l3-check, replay-check,
│                                 replay-truth and itch-check commands;
│                                 tests/lobster.rs: simulated days through LOBSTER files, replay check
├── python/hftsim/                runner.py (Scenario, run_many, verify), stats.py, report.py,
│                                 research.py (the eight experiments), arrow.py and export.py
│                                 (pyarrow tables, Parquet; need the native extension),
│                                 lobster.py (LOBSTER export/check, replay check),
│                                 bitstamp.py (record, convert, check, break test, crossings,
│                                 LOBSTER export, replay against real fills), itch.py (ITCH day check)
├── data/                         not in the repository: recorded Bitstamp runs, downloaded ITCH days,
│                                 each with a derived/ folder of check reports
├── python/native/                PyO3 extension hftsim._native (own Cargo workspace; maturin)
├── python/pyproject.toml         maturin build of hftsim with its extension
├── python/tests/                 statistics by hand, Python defaults = binary defaults, reproducibility,
│                                 native = binary, tables = report, Parquet round trip
├── reports/                      generated research-question reports and their run manifests
├── tools/mutation_test.ps1
└── docs/
    ├── ARCHITECTURE.md           full-system proposal, language choice, roadmap
    ├── MATCHING_RULES.md         exchange rulebook (normative)
    ├── SIMULATION.md             simulation kernel and latency model (normative)
    ├── AGENTS.md                 trading agents and presets
    ├── STYLIZED_FACTS.md         calibration targets, method, results, deviations
    ├── MARKET_MAKING.md          formula register and verification, the ladder, the agent, first runs
    ├── METRICS.md                metric definitions, sources, choices, verification
    ├── HISTORICAL_DATA.md        LOBSTER format, reconstruction, provenance, replay fill model,
    │                             the Bitstamp recording and the Nasdaq BX ITCH day, with their checks
    ├── RESEARCH_RESULTS.md       what the eight research reports show (simulation only)
    ├── RESEARCH_SURVEY.md        matching engines and market-making literature, verification tags
    ├── RISKS.md                  technical risk register
    ├── TESTING.md                correctness evidence
    └── BENCHMARKS.md             performance, methodology, raw output
```

## Using the engine

```rust
use lob::{Command, InstrumentSpec, MatchingEngine, NewOrder, Phase, Side, TimeInForce};

let mut eng = MatchingEngine::new(InstrumentSpec::example(), Phase::Continuous)?;
let mut events = Vec::new();
eng.process(1_000, &Command::New(NewOrder::limit(1, Side::Sell, 10_001, 300)), &mut events)?;
let ioc_buy = NewOrder::limit(2, Side::Buy, 10_002, 100).with_tif(TimeInForce::Ioc);
eng.process(2_000, &Command::New(ioc_buy), &mut events)?;
// events: Accepted, BookAdd, Accepted, Trade @ 10_001 x 100, Fill (maker), Fill (taker)
assert_eq!(eng.best_ask(), Some((10_001, 200)));
```

The same example runs as a doctest in `crates/lob/src/lib.rs`.

## Language choice

Rust: memory safety in exactly the kind of index-heavy, remove-heavy code where C++ engines break;
one-command build and test on the development machine, which has no C++ toolchain; PyO3 for the
Python layer.
Why there is no C++ head-to-head is explained in [architecture §12](docs/ARCHITECTURE.md#12-language-choice-rust).
