# Market making (phase 3)

`crates/agents/src/mm/` holds the market-making models, the online estimators they need, a
ten-step ladder of quoting strategies and the `MarketMaker` agent that runs any of them inside the
simulator. The phase-3 exit rule (ARCHITECTURE §14) was: formulas transcribed and verified first,
then each strategy unit-tested on synthetic paths. This document records where each formula comes
from, how it was checked, how the strategies are built, and what the first runs show and do not
show.

**Nothing here says any strategy is profitable.** The runs in section 6 describe behaviour in a
synthetic market whose known artefacts (STYLIZED_FACTS §5) favour passive liquidity provision.

---

## 1. Formula register

Tags as in [RESEARCH_SURVEY.md](RESEARCH_SURVEY.md): [V] read in the primary text, [S]
secondary sources only, [M] from memory.

| Formula | Source | Status | How it is checked |
|---|---|---|---|
| GLFT model: dS = σ dW; fills at A·e^(−kδ) per side; inventory in ±Q; maximise E[−exp(−γ(X_T + q_T S_T))] | Guéant, Lehalle & Fernandez-Tapia (2013), §2 | **[V]**, arXiv:1105.3115 as rendered by ar5iv | Model statement only |
| Linear ODE system v̇_q = αq²v_q − η(v_{q−1} + v_{q+1}), with α = kγσ²/2 and η = A(1 + γ/k)^−(1 + k/γ); solution v(t) = exp(−M(T − t))·1 | GLFT Propositions 1–2 | **[V]** | `exact_solution_matches_a_direct_numerical_solution_of_the_hjb` |
| Optimal depths δᵇ = (1/k)ln(v_q/v_{q+1}) + (1/γ)ln(1 + γ/k), δᵃ = (1/k)ln(v_q/v_{q−1}) + (1/γ)ln(1 + γ/k) | GLFT Theorem 1 | **[V]** | Same test |
| T → ∞ limit from the eigenvector of M's smallest eigenvalue | GLFT Theorem 2 | **[V]** | `quotes_converge_to_the_asymptotic_solution_and_skew_against_inventory` |
| Closed form δᵇ ≈ (1/γ)ln(1 + γ/k) + ((2q+1)/2)·√(σ²γ/(2kA)·(1 + γ/k)^(1 + k/γ)), δᵃ mirrored | GLFT §4 | **[V]** | Within 2% of the exact limit's inventory term when α/η is small (`closed_form_approximates_…`) |
| Avellaneda–Stoikov: reservation price s − qγσ²τ, spread γσ²τ + (2/γ)ln(1 + γ/k) | Avellaneda & Stoikov (2008) | **[M]**: the paper was not read | Shown to be exactly the first-order expansion in τ of the GLFT solution: the error falls more than 90-fold when τ falls 10-fold (`avellaneda_stoikov_is_the_first_order_expansion_…`). Derivation below |
| Order-flow imbalance eₙ (four indicator terms on best prices and sizes); OFI = Σ eₙ; ΔP ≈ β·OFI with β falling with depth | Cont, Kukanov & Stoikov (2014), §2 | **[V]**, arXiv:1011.6402 as rendered by ar5iv | `ofi_follows_the_cks_cases`: each of the paper's event types |
| Micro-price: the expected mid after many mid changes, given imbalance and spread (a martingale), estimated as a Markov chain: g₁ = (I − Q)⁻¹R, B = (I − Q)⁻¹T, G = Σ Bⁱg₁ | Stoikov (2018) as described by the papers citing it (arXiv:2507.09734, arXiv:2411.13594) | **[S]**: the original (SSRN 2970694) was not available. The Markov-chain construction is derived here | `micro_price_matches_the_simulated_long_run_expected_mid`: brute-force simulation of a synthetic book |
| Fill intensity λ(δ) = A·e^(−kδ), fitted to how often aggressive orders reach depth δ | Standard calibration of the A–S/GLFT intensity [M] | Estimator built here | `intensity_of_an_exponential_depth_profile_is_recovered` |
| Volatility σ² = Σr²/ΣΔt on a time grid, exponential forgetting | Realised variance [M] | Estimator built here | `volatility_of_a_random_walk_is_recovered` |
| Mark-out: mid h later minus fill price, signed for the market maker (a realised-spread measure) | Standard [M] | Built here | `markouts_are_signed_for_the_market_maker` |

### Why the GLFT check is independent

The numerical check does not use GLFT's change of variables. With CARA utility and an arithmetic
Brownian price, the value function factors as u = −exp(−γ(x + qs))·θ_q(t); this is a generic step.
Substituting into the HJB of §2 gives, in τ = T − t,

dθ_q/dτ = ½σ²γ²q²θ_q + min_δ A·e^(−kδ)(e^(−γδ)θ_{q+1} − θ_q) + min_δ A·e^(−kδ)(e^(−γδ)θ_{q−1} − θ_q),

with θ_q(0) = 1 and the bid term absent at q = Q. The test integrates this by RK4 (step 0.01 s, 30 s,
Q = 6, GLFT's Figure 1 parameters), finding each minimum by golden-section search. It then compares
the implied optimal depths with GLFT's closed solution: they agree to within 10⁻⁶ ticks for every
inventory.

### Why Avellaneda–Stoikov is the first-order expansion

For small τ, v(τ) ≈ 1 − τM·1. For an interior inventory, (M·1)_q = αq² − 2η. Then
ln(v_q/v_{q+1}) ≈ τα((q+1)² − q²) = τα(2q + 1). The GLFT bid depth is therefore
(1/k)·τα(2q + 1) + (1/γ)ln(1 + γ/k), which is qγσ²τ + ½γσ²τ + (1/γ)ln(1 + γ/k), exactly the A–S bid
depth (and likewise for the ask). The second-order terms of ln v_q turn out to be −αητ² for every
interior q, so they cancel in the ratio and the remaining error is third order (the test asserts
"faster than first order"). The expansion fails within two units of the inventory bound, where a
neighbour is missing. The A–S formulas are therefore accurate for short horizons and small
inventories. For long horizons they over-skew: their skew grows with τ without bound, while the
exact quotes settle to the asymptotic solution.

## 2. Estimators (`estimators.rs`)

Every estimator uses only what the market maker sees itself: its delayed book view, the trades on
its feed and its own fills. Forgetting is exponential in time.

| Estimator | Input | Output | Defaults in the agent |
|---|---|---|---|
| `VolEstimator` | mid, sampled at most once per second | σ, ticks/√s | half-life 60 s |
| `OfiTracker` | best prices and sizes at every book change | OFI and OFI ÷ average top-of-book depth | half-life 1 s |
| `IntensityEstimator` | each aggressive order's deepest execution, measured from the mid before it | A, k | half-life 120 s; depths 0.5–5 ticks |
| `Markouts` | own fills, mid one horizon later | running average and total, ticks | horizon 1 s, weight 0.1 |
| `MicroPrice` | best quotes at every change of state | adjustment per (imbalance bucket, spread) state | 10 buckets, spreads 1–2 ticks, refit every 1,000 transitions |

The intensity estimator measures the **whole market's** aggressive flow. In the calibrated market
it gives Â ≈ 600–1,100 per second and k̂ ≈ 4–6 per tick. That is an upper bound on one market
maker's fills, which also depend on its queue position. So the ladder uses fixed A and k (section 3)
and keeps the online estimate for experiments.

## 3. The ladder (`strategy.rs`)

A `QuotingStrategy` is a pure function from a `Snapshot` to price levels:
* a reference price (mid, size-weighted mid or micro-price), optionally shifted by order flow;
* a depth per side (fixed, linear inventory skew, Avellaneda–Stoikov or GLFT), widened after toxic
  fills;
* one or more levels within a hard inventory bound.

Prices round to the nearest tick, with ties away from the reference. They never sit at or through
the opposite best of the agent's view, and the two sides never cross each other: if they land on one
tick, the side that rounding pushed further inside steps back. Sizes are cut so that no fill can
carry inventory beyond the bound.

Each step adds one idea to the previous one, so neighbouring steps are comparable:

| Step | Name | Adds | Main parameters (defaults) |
|---|---|---|---|
| 1 | fixed spread | mid ± half-spread | half-spread 0.5 tick (joins the touch) |
| 2 | inventory-aware | linear skew against inventory | 0.1 tick per unit |
| 3 | micro-price | reference = micro-price estimate | – |
| 4 | Avellaneda–Stoikov | A–S quotes, rolling horizon | γ = 3.5, τ = 0.03 s |
| 5 | GLFT | asymptotic exact GLFT quotes, Q = the inventory bound | γ = 3.5 |
| 6 | volatility-adjusted | σ from the online estimator | half-life 60 s |
| 7 | order-flow-aware | reference shifted by OFI and imbalance | 0.5 tick per unit of normalised OFI, 0.25 tick per unit of 2I − 1 |
| 8 | adverse-selection-aware | widen both sides while recent mark-outs are below −0.25 tick | +1 tick |
| 9 | queue-position-aware | keep an order within a tick of the target if at most half its level is ahead of it | – |
| 10 | multi-level | three levels per side, one tick apart | – |

Shared defaults (`LadderParams`): inventory bound 10 units (one unit = 5 lots); σ = 1 tick/√s, the
background's own 1-second volatility; k = 5 per tick, from the intensity estimator; A = 130 units/s, a
market maker's own fill rate at the touch in a probe run, converted to δ = 0. γ and the A–S horizon
were then chosen so that steps 2, 4 and 5 skew equally near zero inventory (0.1 tick per unit,
checked in a test). The steps therefore differ in the shape of their inventory response, not in
risk appetite. These are illustrative choices, not optimised ones.

On a one-tick market, any depth below one tick rounds to the touch. Small skews therefore show up
only once inventory has moved the depth across a whole tick.

## 4. The agent (`agent.rs`)

`MarketMaker` keeps the estimators current and decides at three triggers:
* after each market-data packet (at most every 0.5 ms);
* after each own fill, since inventory changed;
* on a 100 ms heartbeat.

It reconciles the strategy's levels with its live orders:
* an order at a wanted price is kept (cut down by a priority-keeping modify if larger than wanted);
* an order the queue rule says to keep is kept;
* everything else is cancelled;
* missing levels get new orders: post-only by default, with an STP instruction, sized through
  `Limits`, which count open orders as if filled.

The kill switch cancels everything and stops the agent for good, without flattening the position.
`stop_at` cancels everything at the stop time.

Statistics (`MmStats`) are the agent's own and are approximations where noted:
* **spread capture:** measured against the mid of its view at the fill;
* **two-sided share:** sampled at each decision, counting orders in flight as live;
* **message counts.**

PnL is taken from the exchange ledger, marked at the final mid.

**Known simplifications:**
* the agent does not requote while its view of the book is one-sided, and keeps the last two-sided
  quotes;
* the queue rule is a stickiness heuristic, with the share of the level ahead standing in for fill
  probability;
* partial fills make inventory fractional in units: GLFT uses the nearest whole unit, the other
  rules use it as is;
* every level has the same size.

## 5. Tests

* **Unit tests on synthetic inputs** (22 in `mm::`):
  * the formula checks above;
  * every estimator against known truth;
  * each ladder step on hand-computed snapshots: fixed quotes, skew directions and sizes, the A–S
    formula after rounding, GLFT equal to the exact asymptotic solution, wider quotes and stronger
    skew with higher σ, OFI and imbalance shifts, toxicity widening, the queue rule's decision
    table, multi-level sizes against the bound;
  * a sweep of every step over markets, inventories, micro-prices and signal extremes: never at or
    through the opposite best, never self-crossed.
* **Integration tests** (`tests/market_makers.rs`, 7 tests). Every check reads the exchange:
  * all ten steps quote in the calibrated market with fills only as maker, inventory always within
    the bound, own fill counts matching the exchange, and two-sided quotes most of the time;
  * runs reproduce;
  * a stopped market maker leaves nothing resting;
  * in a scripted market, the quotes move exactly as computed by hand when inventory changes, and
    the kill switch trips within 10 µs of a scripted collapse and pulls every quote;
  * the queue rule sends fewer cancels than the step before it;
  * four market makers alongside the background keep every book and ledger consistent.
* **Mutation testing:** see [TESTING.md](TESTING.md), phase 3.

## 6. First runs (simulation only)

`mm_ladder --seeds 4 --minutes 5`, with and without `--informed 10`. Each step trades alone in the
calibrated market. The setup:
* one unit is 5 shares, with an inventory bound of 10 units;
* co-located latency;
* the same four seeds for every step (common random numbers), so differences between steps are
  paired;
* PnL in dollars, marked at the final mid, with no fees configured.

| Step | Default background | Zero-intelligence background | Default + 10 informed | Zero-intelligence + 10 informed |
|---|---|---|---|---|
| 1 fixed spread | +32.3 ± 4.0 | +9.5 ± 4.1 | +28.4 ± 4.0 | +3.8 ± 4.0 |
| 2 inventory-aware | +44.5 ± 3.0 | +33.8 ± 3.5 | +45.4 ± 2.5 | +34.5 ± 1.3 |
| 3 micro-price | +61.9 ± 2.0 | +51.7 ± 2.1 | +62.4 ± 1.6 | +57.3 ± 2.1 |
| 4 Avellaneda–Stoikov | +48.4 ± 1.8 | +34.4 ± 3.8 | +48.9 ± 2.7 | +35.2 ± 1.9 |
| 5 GLFT | +38.1 ± 4.0 | +19.0 ± 4.8 | +38.5 ± 4.2 | +13.4 ± 2.1 |
| 6 volatility-adjusted | +43.9 ± 4.3 | +29.8 ± 4.4 | +43.3 ± 4.5 | +36.5 ± 6.8 |
| 7 order-flow-aware | +68.5 ± 2.1 | +68.7 ± 3.8 | +71.1 ± 2.1 | +75.1 ± 4.1 |
| 8 adverse-selection-aware | +70.5 ± 1.9 | +71.2 ± 4.0 | +71.9 ± 0.5 | +74.3 ± 4.7 |
| 9 queue-position-aware | +54.5 ± 3.1 | +49.9 ± 2.9 | +53.6 ± 1.6 | +55.3 ± 5.1 |
| 10 multi-level | +64.0 ± 2.9 | +64.0 ± 2.5 | +66.5 ± 0.8 | +69.8 ± 6.1 |

Mean ± standard error over 4 seeds of 5 simulated minutes. In every cell the market maker:
* fills 10,000–20,000 times;
* earns a positive 1-second mark-out (+0.01 to +0.11 ticks per unit);
* quotes both sides 85–100% of the time.

Its largest position is 29–30 shares with the linear skew (steps 2) and 37–50 shares otherwise.

What the runs show about the simulator (not about real markets):

* **Passive quoting is profitable here almost regardless of the rule.** 39 of 40 cells have a
  positive mean, and the 40th (fixed spread, zero-intelligence background, informed traders) is
  +3.8 ± 4.0. That is what K21 (mean reversion) and K23 (no informed flow in the background)
  predict.
* **Ten informed traders barely matter.** Mark-outs stay positive. An adverse-selection study
  (research question 2) has to vary the informed share and quality well beyond this.
* **Paired step-to-step differences with the same sign in all four scenarios:**
  * inventory skew over fixed spread: +12 to +31;
  * micro-price over mid: +17 to +23;
  * order-flow skew: +25 to +39, the largest;
  * the queue stickiness rule: −16 to −21 (keeping quotes up to a tick away from target leaves stale
    quotes to be picked off);
  * multi-level over single level: +9 to +15.
* **Avellaneda–Stoikov and GLFT trail the micro-price step** (−13 to −22 each). This comes from the
  parameterisation, not a flaw in the models. The defaults match the inventory skew per unit (0.1
  tick), but the models' optimal base half-spread is about 0.2 tick against step 2's 0.5. On a
  one-tick grid that means they leave the touch only at a larger inventory, so they carry more
  inventory (largest positions 47–50 shares against 37–45).
* **Online σ helps GLFT** (+5 to +23). The market maker's estimate is 0.9–1.35 ticks/√s, above the
  fixed 1.0 on the zero-intelligence background, so it skews harder.
* **The toxicity guard almost never fires** (+1 to +3), because mark-outs are positive.

Four seeds of five minutes are a smoke test, not a study. Phase 4 adds more seeds, confidence
intervals, a PnL decomposition, and sweeps over parameters and the informed share. Those results
are in [RESEARCH_RESULTS.md](RESEARCH_RESULTS.md). Two of them revise this section:
* **The background is not free of adverse selection.** Adverse selection at 1 s costs a GLFT maker
  55–60% of its spread capture even without informed traders, because the split metaorders are
  persistent (RQ1 control).
* **The A–S/GLFT gap closes with γ.** At γ = 14 GLFT's skew is 0.26 tick per unit, and it earned
  more than at γ = 3.5 on both backgrounds (RQ1).

## 7. What this does not establish

* **That any strategy is profitable.** The background has no informed aggressive flow of its own,
  and its 1-second mean reversion (STYLIZED_FACTS §5, RISKS K21) rewards passive quoting. Both make
  market making look easier than it is.
* **That the ladder's ordering is meaningful.** Parameters are illustrative. Differences between
  steps in section 6 come from short runs and few seeds, with wide error bars, and partly reflect
  the artefacts above.
* **That A–S's own derivation was checked.** Its formulas are verified as an approximation of the
  exact GLFT solution, not against its paper.
* **That the micro-price matches Stoikov's estimator in detail.** It matches the definition reported
  by the papers that cite it, and a brute-force simulation.
