# Trading agents (phase 2)

`crates/agents` holds the trading agents and the calibrated background market. Each agent is a
plain Rust struct that implements `sim::Agent`. Its public fields are its parameters, and the
kernel drives it through callbacks (`on_start`, `on_timer`, `on_signal`, `on_report`,
`on_market_data`). Market makers (`agents::mm`) are described in [MARKET_MAKING.md](MARKET_MAKING.md).

What an agent can know is fixed by the kernel ([SIMULATION.md](SIMULATION.md) §3):

* its own book view, built from a delayed market-data feed (private, or shared with its location's
  feed group);
* its own orders, fills, position and cash, from delayed execution reports;
* signal values (fundamental value, news, a leading market), each delayed by its own link;
* its own random stream.

No agent reads the exchange's true state. Every order it sends goes through its compute delay,
its outbound link, the gateway (throttle and pre-trade risk) and the engine queue.

## Building blocks (`common.rs`)

| Item | What it does |
|---|---|
| `Limits` | Strategy-side limits, checked before every order: maximum position *assuming every open order fills*, maximum order size, maximum live orders, and capital (a position cap of capital ÷ price). `room()` returns the largest quantity still allowed, rounded down to the lot. Set them tighter than the gateway's limits so a strategy never relies on being rejected. |
| `Schedule` | When a timer-driven agent wakes: Poisson (exponential gaps with a given mean) or periodic. |
| `Perception` | Information quality: Gaussian observation noise on a signal value (the delay is the signal link's latency). |
| `Ewma` | Exponentially weighted mean and variance (Finch's incremental form). |
| `View` | Helpers over the agent's own book view: bid, ask, mid, spread in ticks, size-weighted micro-price, top-of-book imbalance, a reference price when one side is empty. |
| `send_limit`, `send_marketable` | Order helpers that apply `Limits`. A marketable order is an IOC limit at most `protection_ticks` through the opposite best of the agent's view: a market order with price protection, so no agent can sweep a thin book (risk K14). |
| `cancel_all`, `cancel_older_than`, `working_qty` | Order maintenance. |
| `KillSwitch` | Stops a strategy after a mark-to-market loss (own view, before fees): cancels all its orders and stops for good, recording when it tripped. It does not flatten the position. Wired into every strategy agent (default: never trips). |

All floating point in agents is exactly rounded IEEE arithmetic (`+ − × ÷`, `sqrt`, `floor`,
`round`) or `sim::detmath` (`ln`, `exp`, normal and exponential samplers), never the platform math
library. That keeps runs bit-identical across platforms.

## Agents

### Noise trader (`NoiseTrader`)

Zero-intelligence background flow in the spirit of the Santa Fe model (Smith, Farmer, Gillemot &
Krishnamurthy 2003). At Poisson times (default every 10 ms) it cancels its orders older than
`max_order_age_ns`, then does one of three things:
* cancels one random own order (`p_cancel`; nothing happens if it has none);
* sends a marketable order (`p_market`);
* places a passive limit order of `lots` size. The passive price is a geometric
number of ticks (`depth_decay`) from a base, improving the quote with probability `p_improve` when
the spread allows, and never crossing its own view of the book.

The base is the agent's key modelling choice:
* `anchor_memory_ns: None`: the current same-side best quote. Liquidity then follows the price.
* `anchor_memory_ns: Some(τ)`: an exponential moving average (time constant τ) of its own view
  of the mid. When the price runs away from the anchor, its orders pile up at the touch against
  the move: latent liquidity revealed by price moves, in the spirit of Tóth et al. (2011) and Donier
  et al. (2015). This is what makes impact concave in the background, and it also makes short-horizon
  returns mean-revert ([STYLIZED_FACTS.md](STYLIZED_FACTS.md) §5).

### Fundamental trader (`FundamentalTrader`)

A patient value investor. It observes a value signal with `Perception` noise and targets a position
of `lots_per_tick` × (mispricing beyond its own `threshold_ticks`), capped at `max_lots`. Every
re-quote (default every 500 ms; 50 ms in the background) cancels its orders and joins the bid or
ask toward the target, never bidding above (or offering below) its value estimate. A population
with thresholds spread uniformly over [0, θ_max] has total opposing supply that grows like the
square of the price's displacement, the latent-liquidity shape behind √Q impact.

### Informed trader (`InformedTrader`)

Trades on private information, the adverse-selection risk market makers price into the spread
(Glosten & Milgrom 1985; Kyle 1985). On each signal update (and optional periodic re-checks), when
its estimate is at least `threshold_ticks` above its view of the best ask, it sends an IOC buy
limited to estimate − threshold; sells symmetrically. Information quality is signal latency plus
`Perception` noise.

### Momentum and mean-reversion traders (`MomentumTrader`, `MeanReversionTrader`)

Technical traders that read only their own book view.
* **Momentum** samples the mid periodically (default 100 ms). It compares fast and slow EWMAs; past
  `entry_ticks` it targets ±`position_lots`, and inside `exit_ticks` it flattens (hysteresis). It
  trades toward the target with marketable orders, net of its open orders.
* **Mean reversion** samples the mid periodically (default 200 ms). It computes a z-score against an
  EWMA mean and standard deviation and takes a contrarian target with the same hysteresis. It works
  the target with passive orders at its view of the bid or ask.

### Pairs trader (`PairsTrader`)

Statistical arbitrage on two instruments. It keeps an EWMA mean and standard deviation of the
spread s = mid_B − β·mid_A. Beyond ±`z_entry` it sells the rich leg and buys the cheap one in
ratio β, and inside ±`z_exit` it flattens. Both legs use marketable orders sent back to back, so
legging risk is real. Use a `Tracking` signal (an Ornstein–Uhlenbeck gap to a leader) to give the
two instruments related values.

### Execution agent (`ExecutionAgent`)

Works parent orders one at a time with child orders, then reports each parent: arrival mid,
filled quantity, notional, average price and implementation shortfall (Perold 1988). Schedules
(`Algo`) give the share done after slice k of n:

| Algo | Schedule |
|---|---|
| `Twap` | k/n |
| `Vwap { profile }` | cumulative weight of a volume profile over the horizon |
| `AlmgrenChriss { urgency: κ }` | 1 − sinh(κ(1 − k/n)) / sinh(κ), the optimal trajectory under linear temporary impact (Almgren & Chriss 2001); κ → 0 is TWAP, larger κ front-loads. κ is set directly, not derived from risk aversion and impact parameters. |
| `Pov { rate }` | `rate` × market volume seen on its own feed since the start |

At every slice it sends one marketable child for the shortfall against the schedule, counting
children still in flight, so missed fills are caught up and a parent never overfills.

`MetaorderSource` generates random parents for background flow: Poisson arrivals, random side,
Pareto-distributed size (P(children ≥ c) ∝ c^−α), executed by TWAP at a fixed child rate.
Splitting such parents is the Lillo–Mike–Farmer (2005) mechanism for long-memory order signs,
with sign autocorrelation decaying as lag^−(α − 1).

### Latency arbitrageur (`LatencyArbitrageur`)

Picks off stale quotes after value jumps (Budish, Cramton & Shim 2015; Aquilina, Budish & O'Neill
2022). On a signal update that leaves its view's best ask at least `threshold_ticks` below value,
it sends an IOC at exactly that stale price (bids symmetrically), hoping to arrive before the
quote's owner cancels. On a timer it re-posts its inventory passively at the value price. Who wins
the race is decided by the latency model, not assumed.

### Measurement probe (`ProbeTrader`, phase 5)

An instrument, not a strategy. At Poisson times, while it has no order live, it rests one small
limit order at its view of the best bid or ask, alternating sides, and cancels it after a fixed
life. The exchange's log then shows what really happened to each resting order: the ground truth
`hftsim replay-check` uses to test replay fill models ([HISTORICAL_DATA.md](HISTORICAL_DATA.md) §4).

## Presets (`presets.rs`)

| Item | Purpose |
|---|---|
| `add_background(builder, instrument, value, &cfg)` | Adds the background population for one instrument: three location feed groups in snapshot mode, then noise, fundamental and execution agents. |
| `BackgroundConfig::default()` | The calibrated background ([STYLIZED_FACTS.md](STYLIZED_FACTS.md) §3). |
| `BackgroundConfig::zero_intelligence()` | The same market without the noise traders' anchor. Use it as the second background in every strategy study. |
| `calibrated_value(start, tick, mean_interval_s)` | The value process the defaults were calibrated against (1-second interval). |
| `calibrated_market(seed, mean_interval_s, &cfg)` | A builder holding one instrument, its value signal and the background, ready for strategy agents. |
| `location_profiles()`, `location_of(k)` | The three latency locations and the 10/60/30% split. |

Example: a strategy in the calibrated market.

```rust
let mut m = agents::calibrated_market(seed, 1.0, &agents::BackgroundConfig::default());
let mut momentum = agents::MomentumTrader::new(m.instrument);
momentum.limits = agents::Limits { max_position: 200, max_order_qty: 50, max_open_orders: 5, capital: i128::MAX };
momentum.kill = agents::KillSwitch::at_loss(50_000); // $500
let id = m.builder.add_agent(momentum, sim::AgentConfig::new(sim::LatencyProfile::proximity()).subscribe(m.instrument));
let mut sim = m.builder.build()?;
sim.run_until(600 * 1_000_000_000);
let pnl = sim.ledger(id, m.instrument).gross_value(sim.engine(m.instrument).mid().unwrap_or(0.0).round() as i64);
```

A PnL from such a run is a simulation result on one background, not evidence of profitability
(ARCHITECTURE principle 7; STYLIZED_FACTS §5).

## Writing a new agent

1. Put parameters in public fields with documented defaults in `new()`.
2. Read only `Ctx`: `View::of(ctx, i)`, `ctx.orders()`, signal callbacks. Never assume your own
   orders have arrived; your view lags.
3. Send orders through `send_limit` / `send_marketable` with a `Limits`, and check a `KillSwitch`
   at the top of every callback that trades.
4. Respect `stop_at` and stop re-arming timers once stopped, so runs can go quiet.
5. Use `ctx.rng()` and `sim::detmath` for randomness and transcendental functions.
6. Test it in `crates/agents/tests/agents.rs` against the exchange's event log and ledgers.
