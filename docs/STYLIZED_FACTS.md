# Stylised facts and the calibrated background market (phase 2)

The background market is a population of simple agents (noise, fundamental and execution agents,
[AGENTS.md](AGENTS.md)) that later phases put strategies into. Before any strategy result can
mean anything, the background must show the basic statistical regularities of real order books.
This document states the targets, how each one is measured, the calibrated configuration, the
results, and the deviations that remain.

**Everything here is simulation evidence.** A background that reproduces these facts is plausible,
not validated: the facts are coarse summaries, many different mechanisms produce them, and nothing
here has been compared with historical data yet (phase 5). Tags [V]/[S]/[M] follow
[RESEARCH_SURVEY.md](RESEARCH_SURVEY.md): verified, search-snippet only, from memory.

---

## 1. Targets

| Fact | Target (the phase-2 tolerance) | Basis |
|---|---|---|
| Cancellations | ≥ 90% of resting orders that finish end cancelled, not filled | SEC MIDAS 2012–13: about 96–97% of orders cancelled [S] |
| Executed share | ≤ 10% of submitted quantity executes | MIDAS trade-to-order volume about 2.5–4.2% [S] |
| Spread | spread is one tick ≥ 50% of two-sided time | Design target for a liquid large-tick instrument ($100.00, one-cent tick), the regime described by Bouchaud et al. (2018) [M] and Gould et al. (2013) [V survey]. The 50% threshold is our judgement, not a measured figure |
| Order-sign memory | sign autocorrelation positive at ≥ 95% of lags 1–100, decaying as a power law C(ℓ) ∝ ℓ^−γ with γ ∈ [0.3, 0.8] | Long memory of order signs (Lillo & Farmer 2004; Bouchaud et al. 2004) [M]. The band is chosen to cover the commonly reported exponents, about 0.4–0.7 [M] |
| Concave impact | metaorder impact I ∝ Q^δ with δ ∈ [0.3, 0.7], over ≥ 3 size bins, positive in every bin | Square-root impact, δ ≈ 0.5 (Tóth et al. 2011 [V]; Almgren et al. 2005 [M]) |

Reported but not targets: 1-second mid-price returns (standard deviation, excess kurtosis,
lag-1 autocorrelation, autocorrelation of absolute returns, variance ratio at 10 s). Section 5
explains why they matter.

**Phase-2 exit criterion** (ARCHITECTURE §14), made precise. Over 8 independent seeds × 10
simulated minutes:
1. every seed meets the cancellation, executed-share and spread targets;
2. the impact exponent and the sign-memory exponent, estimated on all seeds pooled, fall inside
   their bands;
3. at least 6 of 8 seeds meet all five targets on their own.

Pooling is used for the exponents because one 10-minute run estimates them with a standard
deviation of about 0.1 (section 4); the per-seed rule is a sanity check, not the test. The
`phase_2_acceptance` test implements this criterion on the calibration seeds.

## 2. Measurement (`crates/analysis`)

Every statistic is computed from the matching engine's own event log (the true book, not any
agent's view) over a window that skips the first 60 simulated seconds while the book fills.

1. **Book path.** The public events are replayed through `MboBook`; best bid and ask are recorded
   after every timestamp.
2. **Spread.** Time-weighted share of two-sided time at 1, 2, 3, 4 and ≥ 5 ticks.
3. **Order flow.** Accepted orders and quantity, executed quantity (from fills), and for every
   order that rested: whether it ended cancelled or fully filled.
4. **Signs.** One sign per aggressive order: an IOC that trades against three resting orders is one
   sign, not three. The autocorrelation is fitted by least squares in log-log space over
   log-spaced lags 5–500 (a factor 1.25 apart, so each decade weighs the same).
5. **Impact.** Every metaorder (parent order of an execution agent) that starts and ends inside
   the window is measured as follows:
   * the mid at its end minus the mid at its start, in ticks, signed so that a move against the
     metaorder is positive;
   * metaorders are grouped by executed quantity into bins with fixed edges a factor 2 apart
     ([16, 32), [32, 64), …);
   * bins with fewer than 30 metaorders are reported but not fitted;
   * I = Y·Q^δ is fitted by least squares in log-log space over the bin means, weighted by count.

   An earlier version used 8 equal-count bins. That is a poor estimator under a fat-tailed size
   distribution. With Pareto α = 1.5, seven of the eight bins crowded into a factor-3 size range,
   and the top bin spanned Q ≈ 90 to 10,000. Averaging a concave impact over such a wide bin biases δ
   *downward* (Jensen's inequality), so that estimator flattered concavity. The change was made
   after the impact target had failed on most seeds. The argument for it is statistical, but
   because it came after seeing results, the holdout seeds of section 4.2 are the cleaner test.
6. **Pooling.** Seeds are independent replications of one market, so impact samples are pooled
   across seeds before binning, and sign autocorrelations are averaged across seeds before fitting.

## 3. The calibrated background (`BackgroundConfig::default`)

Instrument: `InstrumentSpec::example()`, prices in cents, one-cent tick, single-share lots,
reference $100.00. Fundamental value (`calibrated_value`, 1-second mean interval) makes a one-tick
move every second on average, of which 1% are 10-tick jumps. Its variance is about 2 ticks² per
second, roughly a 2% daily volatility for a $100 stock over a 6.5-hour session.

| Group | Count | Behaviour (defaults) | Fact it produces |
|---|---|---|---|
| Noise traders | 100 | Poisson actions every 10 ms on average. First, orders older than 1 s are cancelled. Then:<br>• 35%: cancel a random own order (no action if it has none);<br>• 0.5%: a marketable order (IOC, 2-tick protection);<br>• otherwise: a 1–5 lot limit order a geometric(0.02) number of ticks from an anchor, improving the quote 10% of the time when the spread allows.<br>The anchor is an exponential moving average of the trader's own view of the mid. Its memory is spread log-uniformly over 0.8–8 s across the 100 traders. | Cancellations, executed share, one-tick spread; latent liquidity that resists price moves, giving concave impact |
| Fundamental traders | 20 | Re-quote every 50 ms on average and observe value with N(0, 1 tick) noise. Each targets 100 lots per tick of mispricing beyond a personal threshold (thresholds spread uniformly over 0–6 ticks), up to 2,000 lots. They place passive orders at the touch, never beyond their value estimate. | Pull the price toward value; uniform thresholds make total opposing supply grow like the square of the displacement, the latent-liquidity shape behind √Q impact |
| Execution agents | 4 | Random parents: random side, Pareto(α = 1.5) size from 10 to 5,000 child orders, 1 s idle on average between parents. Each parent is worked by TWAP with one 2-lot marketable child every 10 ms. | Long-memory order signs: splitting Pareto-sized parents gives γ = α − 1 = 0.5 (Lillo, Mike & Farmer 2005 [M]); the metaorders for the impact measurement |

Agents are spread over three locations (10% co-located, 60% proximity, 30% remote), one shared
market-data feed group per location, all in snapshot mode.

`BackgroundConfig::zero_intelligence()` is the same market without the noise traders' anchor:
placement is measured from the current best quote, as in the Santa Fe model of Smith et al. 2003
[M]. Section 5 explains why both exist.

## 4. Results

All runs: 10 simulated minutes, window 60–600 s, about 80,000–100,000 trades and 4.8 million
engine commands each, roughly 15–40× faster than real time per run on this laptop (four runs at
once, other work competing).

### 4.1 Calibration seeds 2026–2033

| Seed | 1-tick spread | Cancel share | Executed | γ | δ (fitted bins) | Return ACF(1) | VR(10) | Excess kurtosis | Targets met |
|---|---|---|---|---|---|---|---|---|---|
| 2026 | 96.5% | 98.5% | 0.54% | 0.45 | 0.62 (4) | −0.13 | 0.96 | 0.98 | 5/5 |
| 2027 | 97.2% | 98.6% | 0.76% | 0.53 | 0.69 (4) | −0.15 | 1.04 | 0.66 | 5/5 |
| 2028 | 96.5% | 98.5% | 0.30% | 0.62 | 0.62 (4) | −0.23 | 0.84 | 1.59 | 5/5 |
| 2029 | 96.7% | 98.6% | 0.75% | 0.64 | 0.52 (4) | −0.08 | 1.11 | 0.85 | 5/5 |
| 2030 | 96.2% | 98.5% | 0.65% | 0.48 | 0.48 (4) | −0.24 | 0.71 | 1.84 | 5/5 |
| 2031 | 94.6% | 98.2% | 0.35% | 0.44 | 0.50 (5) | −0.29 | 0.77 | 0.45 | 5/5 |
| 2032 | 96.1% | 98.5% | 0.37% | 0.43 | 0.58 (4) | −0.21 | 0.84 | 1.35 | 5/5 |
| 2033 | 97.6% | 98.7% | 0.98% | 0.81 | 0.78 (4) | −0.12 | 0.88 | 0.83 | 3/5 |
| **Across seeds** | | | | mean 0.55, sd 0.13 | mean 0.60, sd 0.10 | mean −0.18 | mean 0.90 | mean 1.07 | 7/8 all five |

Pooled over the 8 seeds (13,375 metaorders): **δ = 0.56** (r² 0.99) over 6 fitted bins, with mean
impact 0.24, 0.35, 0.53, 0.71, 0.90 and 1.58 ticks for Q in [16, 32) … [512, 1024); **γ = 0.53**
(r² 0.98), close to the 0.5 that metaorder splitting predicts.

### 4.2 Holdout seeds

**Set A, 3001–3008.** First run once after every calibration choice was fixed. Rerun after the
noise-trader fix of section 4.4 (step 9), which was a correction, not a tuning choice:

| Seed | 1-tick spread | Cancel share | Executed | γ | δ (fitted bins) | Return ACF(1) | VR(10) | Excess kurtosis | Targets met |
|---|---|---|---|---|---|---|---|---|---|
| 3001 | 94.6% | 98.3% | 0.42% | 0.69 | 0.61 (4) | −0.39 | 0.38 | 0.67 | 5/5 |
| 3002 | 96.4% | 98.5% | 0.63% | 0.54 | 0.65 (4) | −0.18 | 0.66 | 0.22 | 5/5 |
| 3003 | 95.1% | 98.3% | 0.25% | 0.64 | 0.65 (4) | −0.29 | 0.71 | 1.18 | 5/5 |
| 3004 | 96.4% | 98.5% | 0.73% | 0.43 | 0.34 (4) | −0.29 | 0.75 | 0.51 | 5/5 |
| 3005 | 97.7% | 98.7% | 0.59% | 0.62 | 0.55 (4) | −0.06 | 1.12 | 0.38 | 5/5 |
| 3006 | 94.7% | 98.3% | 0.19% | 0.51 | 0.53 (4) | −0.32 | 0.63 | 0.64 | 5/5 |
| 3007 | 94.1% | 98.2% | 0.31% | 0.41 | 0.49 (4) | −0.26 | 0.70 | 1.84 | 5/5 |
| 3008 | 95.8% | 98.5% | 0.35% | 0.67 | 0.53 (4) | −0.28 | 0.65 | 0.99 | 5/5 |
| **Across seeds** | | | | mean 0.56, sd 0.11 | mean 0.54, sd 0.10 | mean −0.26 | mean 0.70 | mean 0.80 | 8/8 all five |

Pooled (13,535 metaorders): **δ = 0.53** (r² 0.98) over 6 fitted bins; **γ = 0.54** (r² 0.98).

**Set B, 4001–4008** (fresh; run once, on the final code):

| Seed | 1-tick spread | Cancel share | Executed | γ | δ (fitted bins) | Return ACF(1) | VR(10) | Excess kurtosis | Targets met |
|---|---|---|---|---|---|---|---|---|---|
| 4001 | 95.9% | 98.4% | 0.52% | 0.38 | 0.66 (4) | −0.22 | 1.18 | 1.00 | 5/5 |
| 4002 | 96.4% | 98.4% | 0.17% | 0.41 | 0.68 (4) | −0.09 | 1.53 | 1.60 | 5/5 |
| 4003 | 96.4% | 98.5% | 0.31% | 0.51 | 0.52 (4) | −0.18 | 0.80 | 1.82 | 5/5 |
| 4004 | 96.9% | 98.6% | 0.88% | 0.48 | 0.41 (4) | −0.15 | 0.61 | 1.04 | 5/5 |
| 4005 | 96.5% | 98.5% | 0.59% | 0.48 | 0.63 (5) | −0.18 | 0.75 | 1.33 | 5/5 |
| 4006 | 97.8% | 98.7% | 0.50% | 0.87 | 0.69 (4) | −0.09 | 1.37 | 0.86 | 4/5 |
| 4007 | 96.6% | 98.5% | 1.32% | 0.36 | 0.53 (4) | −0.11 | 0.73 | 1.33 | 5/5 |
| 4008 | 97.6% | 98.7% | 0.60% | 0.49 | 0.71 (4), just above the band | −0.13 | 1.13 | 1.57 | 4/5 |
| **Across seeds** | | | | mean 0.50, sd 0.16 | mean 0.60, sd 0.11 | mean −0.14 | mean 1.01 | mean 1.32 | 6/8 all five |

Pooled (13,335 metaorders): **δ = 0.58** (r² 0.97) over 7 fitted bins; **γ = 0.46** (r² 0.98).

**Verdict.** The exit criterion holds on all three seed sets (24 seeds):
* every seed meets the three structural targets;
* the pooled exponents are inside their bands: δ = 0.56, 0.53, 0.58 and γ = 0.53, 0.54, 0.46;
* 21 of 24 seeds meet all five targets on their own (7, 8 and 6 of 8). Every miss is an exponent
  less than one cross-seed standard deviation outside its band (δ 0.71 and 0.78; γ 0.81 and 0.87).

No fundamental trader ends any of these runs at its position cap.

### 4.3 Sensitivity (calibration seeds 2026–2033)

| Variant | Pooled δ | Per-seed δ, mean ± sd | Seeds passing impact | Pooled γ | Return ACF(1) | VR(10) |
|---|---|---|---|---|---|---|
| Pure zero intelligence, no anchor (`zero_intelligence()`) | 0.66 (r² 0.96) | 0.71 ± 0.09 | 3/8 | 0.53 | −0.01 | 0.99 |
| Single anchor memory, 3 s | 0.51 (r² 0.97) | 0.55 ± 0.11 | 7/8 | 0.52 | −0.23 | 0.85 |
| **Memories 0.8–8 s (default)** | **0.56 (r² 0.99)** | **0.60 ± 0.10** | **7/8** | **0.53** | **−0.18** | **0.90** |
| Memories 0.3–30 s | 0.59 (r² 0.99) | 0.62 ± 0.09 | 7/8 | 0.53 | −0.18 | 0.75 |
| Default, but value moves every 100 ms (about 20 ticks²/s) | 0.79 (r² 0.96), **fails** | 0.85 ± 0.18 | 2/8 | 0.52 | −0.03 | 1.64 |

What this shows:

* **Sign memory comes from the execution agents alone.** γ is 0.52–0.53 in every variant.
* **Concavity has two sources.** The fundamental traders' quadratic supply alone gives δ ≈ 0.66–0.71,
  at the edge of the band (3/8 seeds pass). The noise traders' anchor makes liquidity resist price
  moves and brings δ to about 0.55.
* **The anchor costs short-horizon efficiency.** It moves the 1-second return autocorrelation from
  −0.01 to about −0.2 (section 5).
* **The value process must be one the market can follow.** With value moving ten times faster, the
  price ends runs up to 58 ticks away from value. The fundamental traders trade only passively, so
  they cannot pull it back, and in 3 of 8 seeds most or all of them end at their position cap.
  Returns trend (VR 1.6) and impact turns nearly linear (pooled δ 0.79).
* **The memory spread barely matters.** One 3 s memory, the default 0.8–8 s and a wide 0.3–30 s all
  pass. The default was chosen earlier (section 4.4) and validated on holdout seeds.

### 4.4 Calibration history (the honest record)

| Step | Change | Result |
|---|---|---|
| 1 | Initial: 25% of noise orders at the touch | Price stuck (touch inflow about 4× aggressive outflow); 2/5 targets |
| 2 | Liquidity spread deeper (geometric 0.02), fewer marketable orders, faster executors | 4/5; impact linear, δ 1.04 (equal-count estimator, as in steps 2–5) |
| 3 | 20 value traders, thresholds 0–10 ticks, 40 lots/tick, every 200 ms | δ 0.80 |
| 4 | Thresholds 0–6, 100 lots/tick, every 50 ms | δ 0.83: value traders alone did not make impact concave |
| 5 | Noise traders anchored to a moving average (one memory, about 3 s) | Seed 2026: 5/5 with δ 0.57, but seeds 1–5 gave δ 0.86, 0.57, 0.93, 0.76 and 0.87: a lucky seed, not a calibration |
| 6 | Impact estimator replaced (section 2.5) and multi-seed runs with pooling. Value slowed from about 10 + jumps to 2 ticks²/s for realism; the old value outran the passive fundamental traders | 8 seeds: pooled δ 0.46, 7/8 seeds. Returns now mean-revert (ACF(1) −0.26) |
| 7 | Heterogeneous anchor memories tried (three ranges, defined per action at first); 0.8–8 s kept; memory then redefined in seconds, independent of action rate | Pooled δ 0.49, γ 0.52 |
| 8 | Holdout set A, run once | Pooled δ 0.51, γ 0.53, 8/8 seeds |
| 9 | **Correction:** a noise trader that drew a cancel with nothing to cancel fell through to a marketable order (code did not match its documentation). Fixed; it now does nothing. This mostly matters in the first actions before the measurement window, but it changes every random path, so steps 7–8 were rerun | Section 4.1 and set A of 4.2 |
| 10 | Holdout set B, run once on the final code | Section 4.2 |

Steps 5–7 were chosen after looking at results on the calibration seeds, hence the holdouts.

## 5. Deviations and limitations

1. **Short-horizon mean reversion (the important one).** One-second mid returns have lag-1
   autocorrelation between −0.14 and −0.26 depending on the seed set, and the 10-second variance
   ratio is 0.7–1.0. The anchor causes it: without the anchor these are −0.01 and 0.99. Some negative
   autocorrelation at very short horizons is a known microstructure effect (Cont 2001 [M]), but this
   magnitude has not been compared with data. **A mean-reversion or passive market-making strategy can
   profit from it inside the simulator.** The rule for later phases:
   * report every strategy result on both backgrounds (`default` and `zero_intelligence`);
   * treat an edge that exists only on the anchored background as an artefact.

   Adaptive liquidity providers (phase 3) may reduce the effect; in real markets they absorb
   predictable order flow (Lillo & Farmer 2004 [M]). It will be re-measured then.
2. **No volatility clustering and mild tails.** The autocorrelation of absolute 1-second returns
   stays within about ±0.1 and does not decay slowly, and excess kurtosis is about 1. Real intraday
   returns are strongly heavy-tailed with long-lived volatility clustering (Cont 2001 [M]). Nothing
   in the background produces activity bursts: no Hawkes-type clustering and no intraday seasonality.
   Volatility-sensitive results (for example volatility-adjusted quoting) need a different background.
3. **Impact is measured only from start to end of a metaorder.** Impact decay after the end, and the
   permanent share, are not measured. Because the child rate is fixed, metaorder duration grows with
   size, so the two are confounded.
4. **Participation is high.** While a parent is active, its child orders make up roughly half of
   the aggressive volume.
5. **Estimation noise.** One 10-minute run estimates δ and γ with standard deviations of about
   0.1–0.16. Single-run exponents should not be quoted.
6. **The tolerances are ours.** They are set from the cited ranges and judgement, not taken from one
   source; the spread threshold is a design target.
7. **The value process is exogenous**, and the same for all fundamental traders apart from their
   observation noise.

## 6. Reproduce

```text
cargo run --release -p agents --example stylized_facts                           # seed 2026, one run
cargo run --release -p agents --example stylized_facts -- --seeds 8              # section 4.1
cargo run --release -p agents --example stylized_facts -- --seeds 8 --seed 3001  # section 4.2, set A
cargo run --release -p agents --example stylized_facts -- --seeds 8 --seed 4001  # section 4.2, set B
cargo run --release -p agents --example stylized_facts -- --seeds 8 --memory-lo-ms 0                          # no anchor
cargo run --release -p agents --example stylized_facts -- --seeds 8 --memory-lo-ms 3000 --memory-hi-ms 3000   # one memory
cargo run --release -p agents --example stylized_facts -- --seeds 8 --memory-lo-ms 300 --memory-hi-ms 30000   # wide spread
cargo run --release -p agents --example stylized_facts -- --seeds 8 --value-ms 100                            # fast value
cargo test --release -p agents --test agents -- --ignored                        # the exit criterion
```

The simulation is deterministic: the same seed and configuration give the same event log on any
platform, because the agents use only exactly rounded IEEE operations and `sim::detmath`. The
fitted statistics use the platform math library and could differ in the last digits. Wall-clock
speed depends on the machine and its load.
