# Research results: simulation stage (phase 4)

> **Everything here is simulation evidence about the hft-sim market, and nothing more.** No result
> below says that a strategy is, or would be, profitable in a real market. Each research question
> also has a historical stage (ARCHITECTURE §13) that has not been run. Phase 5 builds its tools
> ([HISTORICAL_DATA.md](HISTORICAL_DATA.md)): a LOBSTER reader checked against vendor snapshots,
> and a replay fill model checked against the exchange. The only real data so far is 19 minutes of
> Bitstamp BTC/USD order-level data, used to check the book rebuild (every exchange snapshot
> reproduced) and the replay fill model against real fills (never optimistic; with the takers'
> published orders it recovers nearly all of them).
> None of the research questions has been run on it, and a crypto venue says nothing directly
> about the equity-like market simulated here (RISKS K27).

The data are in [reports/](../reports/README.md): one generated report per question, with the
design, every number quoted here and the manifests that reproduce each run. This document reads
those reports.

## How the experiments were run

* **Market.** The calibrated background ([STYLIZED_FACTS.md](STYLIZED_FACTS.md)): 100 noise
  traders, 20 fundamental traders and 4 metaorder executors around a value process that moves
  about once a second. Prices are on a one-cent tick around $100, and the spread is one tick about
  95% of the time. Every question was run on two backgrounds: the anchored default, whose 1 s
  returns mean-revert (RISKS K21), and pure zero intelligence without the anchor.
* **Added agents.** The strategies under study were added with the runner's flags: informed
  traders (IOC orders when their noisy value estimate is at least a tick through the quote),
  latency arbitrageurs, and ladder market makers. The market maker's defaults are γ = 3.5, an
  inventory limit of 10 units of 5 shares, σ = 1 tick/√s, A = 130 and k = 5. All of them reach the
  exchange through the latency model.
* **Statistics.** Each cell has 8 seeds (2026–2033), the same in every cell, so all cells share the
  background's random streams (common random numbers). Each run lasts 180 simulated seconds,
  measured over 60–180 s. Agent PnL is the change in mark-to-market PnL over that window, with any
  position held at 60 s carried in at that mid ([METRICS.md](METRICS.md)). Intervals are 95%
  Student-t across seeds (7 degrees of freedom). Differences between cells are paired on seed, and
  rates pooled over seeds (queue fills, cancels too late) use Wilson intervals.
* **Multiplicity.** The eight reports hold well over a hundred intervals. At 95%, several will
  exclude zero by chance, so isolated borderline results are not findings. The findings below are
  effects that are large against their intervals and appear on both backgrounds.
* **Reproducibility.** Every run's flags and event digest are in `reports/rqN.manifests.json`, and
  each report re-ran one of its runs without the cache to confirm the digest.

## Summary

| # | Finding in the simulator | Both backgrounds? | Historical stage (not run) |
|---|---|---|---|
| 1 | More inventory aversion gave more PnL *and* less inventory risk, through less adverse selection. It did so with or without informed traders, because the background's split metaorders are persistent | yes | replay the quotes on MBO data with a queue model |
| 2 | Signal quality, not the number of informed traders, decided the maker's PnL. Noisy informed flow raised it. 60 precise informed traders brought it to zero only on zero intelligence | yes (zero intelligence closer to break-even) | mark-outs of passive fills in MBO data |
| 3 | Fill probability fell from about 90% at the front of the queue to about 10% behind 500 shares, almost the same in all four regimes | yes | the same estimate on MBO data |
| 4 | A remote maker sent about 3× more cancels too late (8% against 2–3%) but lost little PnL (0–3 dollars of 27–31). This maker has no news signal to race on | yes | race statistics against Aquilina et al. |
| 5 | Adverse selection was about a third complete after 50 ms and complete by 1 s. The informed traders' latency did not matter | yes | mark-outs around information events |
| 6 | OFI's R² rose from about 0.05 (0.1 s) to about 0.6 (10 s), and impact scaled with 1/depth. β × depth was 0.05–0.13, not CKS's stylised 0.5 | yes | the same regression on L1/MBO data (the meaningful test) |
| 7 | The adaptive maker beat the static one in all eight cells, most where volatile and informed | yes | walk-forward on historical replay |
| 8 | More makers: lower PnL each, more in total, deeper book; spreads pinned at one tick | yes | cross-sectional comparison only |

**Across the questions:** the market maker made money in nearly every configuration tried, even
against 60 informed traders. That is a statement about this simulator, and it is why nothing here
is a profitability claim. The background supplies patient flow that pays the spread: noise
traders and metaorder executors. The informed traders, as a group, lost money on the default
background. Whether real order flow is this generous is exactly what phase 5 must measure, through
mark-outs of passive fills in historical data.

## RQ1: inventory risk and optimal quoting

[Report](../reports/rq1.md). One GLFT market maker with 10 informed traders. Risk aversion γ was
swept from 0.5 to 14, the inventory limit set to 20, 50 or 100 shares, and Avellaneda–Stoikov tried
at γ = 3.5. A control without informed traders compares γ = 3.5 and 14.

**Finding.** Within this market, more inventory aversion gave *more* PnL as well as less inventory
risk. The textbook trade-off of less risk for less spread income did not appear. From γ = 3.5 to
γ = 14:

* PnL rose by $9.2 [5.9, 12.5] on the default background and $16.2 [13.1, 19.2] on zero
  intelligence over the 120 s window (paired on seed; positive on every seed);
* inventory RMS fell from about 29–33 to 18 shares;
* the PnL spread across seeds narrowed.

The decomposition locates the gain in adverse selection: the 1 s mark-out loss shrank from about
$33 to $21 (default) and from $49 to $31 (zero intelligence). Spread capture fell only by about $3.
Two other cells point the same way, each holding less inventory and earning more. A tighter limit
(20 shares) gained +$5.9 [1.7, 10.2] and +$15.5 [12.1, 18.9]. Avellaneda–Stoikov at γ = 3.5 gained
+$4.6 [1.8, 7.4] and +$10.3 [7.4, 13.2], with inventory RMS of 22–25 shares against 29–33 for GLFT.
A looser limit (100 shares) changed nothing measurable.

Below γ ≈ 3.5 the results do not differ, and the size of the skew explains why. On a one-tick
grid a quote moves only once its skew reaches about half a tick. GLFT's closed form puts
the skew per 5-share unit of inventory at about 0.03 tick at γ = 0.5, 0.07 at 1.75, 0.10 at 3.5 and
0.26 at 14. (The strategy uses the exact solution, whose skew at γ = 3.5 is tested at 0.1.) At low
γ the quotes therefore hardly move inside the 10-unit limit. At γ = 14 they step back after about
two units.

**The control changes the explanation.** Without any informed traders, γ = 14 still beat γ = 3.5,
by $8.5 [6.7, 10.4] (default) and $10.3 [7.7, 12.9] (zero intelligence). The gain again came from
the price terms: the 1 s mark-out on the default background, and the later revaluation of
inventory on zero intelligence. It did not come from spread capture. So the effect does not need
informed traders. The background's own aggressive flow is persistent: metaorders are split into
child orders over time, which is what gives the background its long-memory order signs and concave
impact. A maker that keeps quoting at
the touch while its position grows is the counterparty to a metaorder that keeps hitting one side
while it moves the price. Skewing on inventory steps out of the way. Informed traders enlarge the
effect on zero intelligence (+$16.2 with them, +$10.3 without), but they are not its source.

**Reading.** In this market, inventory signals persistent one-sided flow, informed or not. Spread
income hardly depends on γ, because the quotes sit at the touch either way; adverse selection
does. This also corrects RISKS K23: even without informed traders, adverse selection at 1 s costs
this maker about 55–60% of its spread capture.

**What it does not show.** That high γ is better in a real market; that this mechanism dominates
when spreads are wider than a tick; or anything about optimal γ in general. The GLFT model assumes
no adverse selection at all; the result is about what its inventory term does in a market that has
some.

## RQ2: when adverse selection makes market making unprofitable

[Report](../reports/rq2.md). One GLFT market maker (γ = 3.5) against 0, 10, 30 or 60 informed
traders whose value signal carries noise of 0.25 or 1 tick.

**Finding.** The break-even frontier was not crossed. With 60 precise informed traders on the
zero-intelligence background, the maker's PnL interval only reached zero ($2.45 [−0.29, 5.20]).
Signal quality mattered more than numbers:

* **Precise signals (0.25 tick).** Each added informed trader cost the maker about 12–14 cents per
  120 s. PnL fell from $15.9 to $8.6 (default background) and from $10.8 to $2.5 (zero
  intelligence) as informed traders went from 0 to 60. Paired on seed, the 60-trader cells were
  −$7.3 [−11.4, −3.2] and −$8.4 [−11.7, −5.0] against no informed traders.
* **Noisy signals (1 tick).** The maker's PnL *rose* with the number of informed traders, by
  +$19.7 [14.2, 25.1] and +$30.4 [25.0, 35.7] at 60. Their orders added more spread capture (about
  $180–190) than adverse selection (about $145–150). With a noise s.d. equal to their 1-tick
  trading threshold, many of their trades are triggered by noise. They pay the spread like noise
  traders.

In every cell with informed traders, adverse selection at 1 s and at 15 s are within 7% of each
other: their information is in the price within a second. Without informed traders on zero
intelligence, the mark-out keeps growing, from $23 at 1 s to $32 at 15 s. That is the persistent
metaorder impact of RQ1's control.

**The informed traders themselves lost money** on the default background: −$278 to −$643 as a
group at 30–60 traders, with wide intervals. On zero intelligence their PnL is indistinguishable
from zero. A likely reason, not isolated here: on the anchored background, part of the price
impact of one-sided flow reverts (RISKS K21), so they pay for impact that does not last.
"Informed" here means informed about the value process, not profitable.

**What it does not show.** Where the frontier lies for a better-protected maker. This one is the
γ = 3.5 setting that RQ1 found dominated by higher γ. Informed flow is also capped: each informed
trader stops at 200 shares, so 60 of them cannot trade without limit in one direction. The result
is specific to informed traders whose signal is the simulator's own value process, delivered
through a proximity-latency link.

## RQ3: queue position and execution probability

[Report](../reports/rq3.md). Every background order that joined or improved the best price, by the
quantity already queued ahead of it, with 0 or 30 informed traders and no market maker. Pooled over
8 seeds: 156,000–194,000 orders per cell.

**Finding.** Fill probability falls steeply and monotonically with the queue ahead, and the shape
is nearly the same on both backgrounds and in both flow regimes:

| queue ahead (shares) | 0 | 1–49 | 50–99 | 100–199 | 200–499 | 500+ |
|---|---|---|---|---|---|---|
| filled at least partly | 90–96% | 48–53% | 31–35% | 27–30% | 22–25% | 8–12% |

Informed flow raises the front-of-queue fill rate a little (90.1% → 92.9% on the default
background, 92.0% → 95.9% on zero intelligence). Mostly, it shortens queues: depth at the best
quotes falls from 812 to 222 shares (default) and from 297 to 100 (zero intelligence). Far fewer
orders then join behind 500 shares or more.

**What it does not show.** The probability for a patient order. These are background orders, and
most of them are cancelled by their owners (noise traders cancel orders older than their maximum
age). The measured rates mix queue depletion with the owners' cancellation behaviour. A queue
model for a resting market maker (K3) needs the hazard of being reached, not this mixture. The
comparison with queue-reactive model predictions (Huang, Lehalle & Rosenbaum 2015) that
ARCHITECTURE §13 asks for was not done.

## RQ6: order-flow imbalance and short-term price moves

[Report](../reports/rq6.md). Mid changes regressed on CKS order-flow imbalance over 0.1 s, 1 s and
10 s intervals, in the RQ3 runs.

**Finding.**

* **The share of mid changes OFI explains grows with the interval.** R² is 0.02–0.09 at 0.1 s,
  0.19–0.44 at 1 s and 0.53–0.65 at 10 s. For comparison, Cont, Kukanov & Stoikov report an
  average R² of 65% for 10-second mid changes in 50 US stocks (verified in the paper). Our 10 s
  regressions have only 12 intervals per run, so those R² are noisy (intervals as wide as
  0.32–0.79).
* **Impact scales with 1/depth, as CKS found.** Across the four regimes, depth at the best quotes
  varies eightfold (100–812 shares) and β by a similar factor. β × depth stays within 0.046–0.061
  at 0.1 s, 0.085–0.112 at 1 s and 0.088–0.132 at 10 s.
* **β × depth is far below the stylised book's 0.5.** It is 0.05–0.13: the mid moves a fifth or
  less of what draining a queue of depth D would predict. One explanation, not tested here, is that
  queues at the touch refill while they are being drained. The phase-5 MBO data can test the
  ratio directly.

**What it does not show.** Anything predictive. The regression is contemporaneous, as in CKS: OFI
over an interval against the price change over the same interval.

## RQ4: latency and expected PnL

[Report](../reports/rq4.md). One market maker (step 7) at three latency locations, with or without
three co-located latency arbitrageurs, plus 10 informed traders.

**Finding.** Latency changed *how often the maker's cancels arrived too late*, but hardly its PnL:

* **Cancels.** The share that found their order already gone was 2.0–3.1% co-located or at
  proximity, against 7.7–8.6% remote. That holds in every cell, and the pooled intervals do not
  overlap.
* **PnL.** Against PnL of $27–31 per 120 s, remote cost nothing measurable on the default
  background: −$1.0 [−3.7, 1.8] without arbitrageurs, −$0.6 [−1.5, 0.4] with them. On zero
  intelligence it cost $2.5–3.0 (both intervals exclude zero). Proximity cost nothing measurable
  anywhere. Adding the arbitrageurs changed the maker's PnL by +$0.2 to +$1.1 (paired), and every
  interval includes zero.
* **Races.** The maker lost 2–12 races per 120 s, while making 5,500–7,200 fills.
* **The arbitrageurs** roughly doubled the races in the default market, from 8–10 to about 20 per
  120 s.

**Reading.** In this market, speed is worth little to this maker, because it has no fast signal to
act on. It quotes from the book alone, and the latency arbitrageurs trade on value jumps it never
sees. Being faster only lets it react sooner to book changes, which matter less. The race that
latency decides needs a maker that hears the news: a value jump, then the sniper's take against
the maker's cancel. The end-to-end race test has exactly that, a maker that pulls its quote on news
against two snipers (TESTING.md, phase 4). No ladder step listens to news, though. The experiment
ARCHITECTURE §13 has in mind, makers at different speeds that all hear the news, is not run here.

**What it does not show.** The value of speed to a maker that does trade on news, or any latency
rank effect among several makers (Baron et al.). The latency profiles are illustrative, not
calibrated to a venue (RISKS K4).

## RQ5: informed traders and market-maker profitability

[Report](../reports/rq5.md). One GLFT maker against 30 informed traders (noise 0.5 tick) whose
signal and orders travel co-located, at proximity or remote.

**Finding.** Informed trading showed up in the maker's PnL as adverse selection that built up within
a second. Its timing did not depend on the informed traders' latency:

* **Timing.** Adverse selection was about a third complete 50 ms after the maker's fills ($18–26)
  and complete by 1 s ($55–77; the 15 s value is the same). It is the dominant cost: spread capture
  was $65–76, and PnL $11–13 (default) and $3–4 (zero intelligence; two of the three intervals
  include zero).
* **Latency.** From co-located to remote, PnL, spread capture and each mark-out moved by less than
  their intervals. Paired on seed, the maker's PnL changed by −$0.7 to +$1.7, and every interval
  includes zero. The value process moves about once a second, so a millisecond's delay changes
  little about which trades the informed traders make.

**What it does not show.** A world where information is a race (several informed traders on the
same news, with different speeds), or information horizons longer than a second. The value process
is the only information source.

## RQ7: adaptive against static market makers

[Report](../reports/rq7.md). A static maker (step 2: mid-centred quotes, linear inventory skew)
against an adaptive one (step 8: micro-price centre, GLFT quotes, online σ, order-flow skew, a
mark-out toxicity guard). Four regimes: the value moves every 500 ms or every 2 s on average, with
0 or 30 informed traders.

**Finding.** The adaptive maker earned more in all eight regime-background cells, by $5.4 to $17.6
per 120 s (paired on seed; every interval excludes zero). The gap was widest where the market was
volatile and informed: +$14.9 (default) and +$17.6 (zero intelligence). There the static maker's
PnL fell to $9.7 and $6.5, while the adaptive one held about $24–25. The gap was narrowest in calm
markets with informed traders (+$5.4, +$7.2).

**Reading.** Adapting pays most where static quoting is most exposed, as one would expect. It is
not a K21 artefact: the zero-intelligence background shows it as strongly. The experiment
compares two bundles, though. Step 8 adds five ideas to step 2, and this design cannot say which
of them earns the gain. The phase-3 smoke runs pointed at order-flow skew
([MARKET_MAKING.md](MARKET_MAKING.md) §6). A step-by-step ablation over these regimes would settle
it.

**What it does not show.** That adaptation pays against real order flow. Every regime here is one
of this simulator's.

## RQ8: competition and spreads

[Report](../reports/rq8.md). 1, 2 or 4 identical market makers (step 7), with 10 informed traders.

**Finding.** Competition showed up in profits and depth, not in spreads:

* **Per-maker PnL fell.** It went from $28.7 to $23.0 to $17.5 on the default background, and from
  $29.4 to $23.5 to $16.2 on zero intelligence. Paired on seed, four makers each earned $11.2
  [8.7, 13.7] and $13.2 [11.2, 15.3] less than a single maker.
* **Total maker PnL rose.** It went from $28.7 to $46.1 to $70.0, and from $29.4 to $47.0 to $64.6.
* **Spreads were already at one tick.** Time-weighted, they stayed at 1.000–1.001 ticks. The
  effective spread narrowed only from 1.005 to 1.002 cents.
* **Depth at the best quotes grew.** It went from 885 to 1,548 shares (default, wide intervals),
  and from 181 to 289 (zero intelligence).

**Reading.** On a one-tick grid, spreads cannot narrow, so competition works through queue share.
Total maker profit kept growing with more makers. Without fees, PnL sums to zero across all
participants, so the extra profit came from others, not only from the other makers. The noise and
fundamental traders' PnL falls as makers are added, but over 120 s it is too noisy to attribute
the shift.

**Mean reversion (RISKS K21) re-measured.** Without a market maker, the lag-1 autocorrelation of
1 s returns on the default background was −0.21 [−0.25, −0.16] (RQ6 runs), inside the −0.14 to
−0.26 range of phase 2. With one step-7 maker it was −0.07 [−0.17, 0.03]; with two and four,
+0.03 and +0.08, and every one of these intervals includes zero. The makers absorb the reverting
part of the background's moves. Their PnL is no higher on the default background than on zero
intelligence ($28.7 against $29.4 for one maker), so there is no sign that they live off the
artefact.

**What it does not show.** Anything about spreads where the tick is not binding. Nor anything
causal about real markets: ARCHITECTURE §13 keeps competition claims in simulation, and real-market
comparisons for this question can only be cross-sectional.
