//! Building blocks shared by the trading agents: risk limits, wake-up schedules, noisy
//! perception, book-view helpers, order helpers and a loss kill switch.
//!
//! Everything here works through the agent's own `Ctx`: its delayed book view, its own record
//! of orders and fills, and requests that travel its latency path. No helper ever looks at
//! the exchange's true state.
//!
//! Floating point is used only with basic IEEE operations, `sqrt` and the deterministic samplers
//! of `sim::detmath`, so agent behaviour is reproducible on every platform.

use lob::rng::Rng;
use lob::{InstrumentSpec, MboBook, Price, Qty, Side, TimeInForce, Timestamp};
use sim::detmath;
use sim::{ClientOrderId, Ctx, InstrumentId, OrderSpec, Request};

/// Strategy-side risk limits, checked before every order. The gateway enforces its own,
/// separate limits; set these tighter so the agent never relies on being rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest absolute position the agent may reach if all its open orders filled.
    pub max_position: Qty,
    /// Largest single order.
    pub max_order_qty: Qty,
    /// Most live orders per instrument.
    pub max_open_orders: usize,
    /// Capital: the largest gross position value (|position| × price, price units × quantity).
    /// Acts as a price-dependent position limit.
    pub capital: i128,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_position: 1_000, max_order_qty: 100, max_open_orders: 10, capital: i128::MAX }
    }
}

impl Limits {
    /// Largest quantity the agent may still send on `side` at `price`, assuming every open order
    /// fills (the same worst case the gateway checks), rounded down to the lot size.
    pub fn room(&self, ctx: &Ctx<'_>, instrument: InstrumentId, side: Side, price: Price) -> Qty {
        let Some(spec) = ctx.spec(instrument) else { return 0 };
        let orders = ctx.orders();
        if orders.live_count(instrument) >= self.max_open_orders {
            return 0;
        }
        let position = i128::from(orders.position(instrument));
        let (mut open_buy, mut open_sell) = (0i128, 0i128);
        for (_, o) in orders.live().filter(|(_, o)| o.instrument == instrument) {
            match o.side {
                Side::Buy => open_buy += i128::from(o.leaves),
                Side::Sell => open_sell += i128::from(o.leaves),
            }
        }
        let capital_position = self.capital / i128::from(price.abs().max(1));
        let max = i128::from(self.max_position).min(capital_position);
        let room = match side {
            Side::Buy => max - (position + open_buy),
            Side::Sell => max + (position - open_sell),
        };
        let q = room.min(i128::from(self.max_order_qty)).max(0) as Qty;
        q / spec.lot_size * spec.lot_size
    }
}

/// When a timer-driven agent wakes up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Schedule {
    /// Exponentially distributed gaps with this mean (a Poisson process).
    Poisson {
        /// Mean gap in nanoseconds.
        mean_ns: f64,
    },
    /// Fixed gaps.
    Periodic {
        /// Gap in nanoseconds.
        period_ns: i64,
    },
}

impl Schedule {
    /// Next wake-up time after `now`.
    pub fn next(&self, now: Timestamp, rng: &mut Rng) -> Timestamp {
        match *self {
            Schedule::Poisson { mean_ns } => now + (detmath::exponential(mean_ns, rng).round() as i64).max(1),
            Schedule::Periodic { period_ns } => now + period_ns.max(1),
        }
    }
}

/// Information quality: what an agent perceives when a signal value reaches it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Perception {
    /// Standard deviation of independent Gaussian noise added to each observation, in price
    /// units (0 = perfect information). Delay is the agent's signal latency.
    pub noise_sd: f64,
}

impl Perception {
    /// The agent's (noisy) estimate of a true value.
    pub fn observe(&self, value: i64, rng: &mut Rng) -> f64 {
        if self.noise_sd == 0.0 {
            value as f64
        } else {
            value as f64 + self.noise_sd * detmath::standard_normal(rng)
        }
    }
}

/// Exponentially weighted mean and variance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ewma {
    alpha: f64,
    mean: f64,
    var: f64,
    count: u64,
}

impl Ewma {
    /// Weight `alpha` in (0, 1] on each new observation.
    pub fn new(alpha: f64) -> Self {
        Ewma { alpha, mean: 0.0, var: 0.0, count: 0 }
    }

    /// Add an observation.
    pub fn update(&mut self, x: f64) {
        if self.count == 0 {
            self.mean = x;
        } else {
            let diff = x - self.mean;
            let incr = self.alpha * diff;
            self.mean += incr;
            self.var = (1.0 - self.alpha) * (self.var + diff * incr);
        }
        self.count += 1;
    }

    /// Current mean (`None` before the first observation).
    pub fn mean(&self) -> Option<f64> {
        (self.count > 0).then_some(self.mean)
    }

    /// Current standard deviation.
    pub fn sd(&self) -> f64 {
        self.var.max(0.0).sqrt()
    }

    /// Observations so far.
    pub fn count(&self) -> u64 {
        self.count
    }
}

/// Read-only helpers over an agent's (delayed) view of one instrument's book.
#[derive(Clone, Copy)]
pub struct View<'a> {
    /// The agent's book view.
    pub book: &'a MboBook,
    /// Instrument rules.
    pub spec: &'a InstrumentSpec,
}

impl<'a> View<'a> {
    /// The agent's view of `instrument` (`None` if it is not subscribed).
    pub fn of(ctx: &'a Ctx<'_>, instrument: InstrumentId) -> Option<View<'a>> {
        Some(View { book: ctx.book(instrument)?, spec: ctx.spec(instrument)? })
    }

    /// Best bid price.
    pub fn bid(&self) -> Option<Price> {
        self.book.best_bid().map(|x| x.0)
    }

    /// Best ask price.
    pub fn ask(&self) -> Option<Price> {
        self.book.best_ask().map(|x| x.0)
    }

    /// Mid price.
    pub fn mid(&self) -> Option<f64> {
        Some((self.bid()? + self.ask()?) as f64 / 2.0)
    }

    /// Spread in ticks.
    pub fn spread_ticks(&self) -> Option<i64> {
        Some((self.ask()? - self.bid()?) / self.spec.tick_size)
    }

    /// Size-weighted mid ("micro-price" in its simplest, imbalance-weighted form).
    pub fn microprice(&self) -> Option<f64> {
        let (b, bq) = self.book.best_bid()?;
        let (a, aq) = self.book.best_ask()?;
        let total = (bq + aq) as f64;
        if total <= 0.0 {
            return None;
        }
        Some((b as f64 * aq as f64 + a as f64 * bq as f64) / total)
    }

    /// Top-of-book imbalance in [−1, 1] (positive = more bid quantity).
    pub fn imbalance(&self) -> Option<f64> {
        let (_, bq) = self.book.best_bid()?;
        let (_, aq) = self.book.best_ask()?;
        let total = (bq + aq) as f64;
        (total > 0.0).then(|| (bq - aq) as f64 / total)
    }

    /// Best available price estimate: mid, else one side, else last trade, else the reference.
    pub fn reference(&self) -> f64 {
        match (self.bid(), self.ask()) {
            (Some(b), Some(a)) => (b + a) as f64 / 2.0,
            (Some(b), None) => b as f64,
            (None, Some(a)) => a as f64,
            (None, None) => self.book.last_trade().map_or(self.spec.reference_price, |x| x.0) as f64,
        }
    }
}

/// Largest tick-grid price at or below `x`.
pub fn floor_tick(x: f64, tick: Price) -> Price {
    (x / tick as f64).floor() as Price * tick
}

/// Smallest tick-grid price at or above `x`.
pub fn ceil_tick(x: f64, tick: Price) -> Price {
    (x / tick as f64).ceil() as Price * tick
}

/// Clamp a price into the instrument's bounds.
pub fn clamp_price(price: Price, spec: &InstrumentSpec) -> Price {
    price.clamp(spec.min_price, spec.max_price)
}

/// Send a limit order of up to `qty`, cut to what `limits` allow. Returns the client order id,
/// or `None` if nothing could be sent.
pub fn send_limit(
    ctx: &mut Ctx<'_>,
    instrument: InstrumentId,
    side: Side,
    price: Price,
    qty: Qty,
    tif: TimeInForce,
    limits: &Limits,
) -> Option<ClientOrderId> {
    let spec = *ctx.spec(instrument)?;
    let price = clamp_price(price, &spec);
    let qty = qty.min(limits.room(ctx, instrument, side, price)) / spec.lot_size * spec.lot_size;
    if qty <= 0 {
        return None;
    }
    let cid = ctx.next_client_id();
    ctx.send(Request::New(OrderSpec::limit(instrument, cid, side, price, qty).with_tif(tif)));
    Some(cid)
}

/// Marketable IOC order: a limit through the agent's view of the opposite best quote by at most
/// `protection_ticks` (a market order with price protection). Uses the agent's reference
/// price when the opposite side looks empty.
pub fn send_marketable(
    ctx: &mut Ctx<'_>,
    instrument: InstrumentId,
    side: Side,
    qty: Qty,
    protection_ticks: i64,
    limits: &Limits,
) -> Option<ClientOrderId> {
    let price = {
        let view = View::of(ctx, instrument)?;
        let tick = view.spec.tick_size;
        match side {
            Side::Buy => view.ask().unwrap_or_else(|| ceil_tick(view.reference(), tick)) + protection_ticks * tick,
            Side::Sell => view.bid().unwrap_or_else(|| floor_tick(view.reference(), tick)) - protection_ticks * tick,
        }
    };
    send_limit(ctx, instrument, side, price, qty, TimeInForce::Ioc, limits)
}

/// Cancel every live order of the agent on `instrument`.
pub fn cancel_all(ctx: &mut Ctx<'_>, instrument: InstrumentId) {
    if ctx.orders().live().any(|(_, o)| o.instrument == instrument && !o.cancel_pending) {
        ctx.send(Request::CancelAll { instrument });
    }
}

/// Cancel live orders on `instrument` sent before `now − age_ns`. Returns how many.
pub fn cancel_older_than(ctx: &mut Ctx<'_>, instrument: InstrumentId, age_ns: i64) -> usize {
    let cutoff = ctx.now() - age_ns;
    let old: Vec<ClientOrderId> = ctx
        .orders()
        .live()
        .filter(|(_, o)| o.instrument == instrument && !o.cancel_pending && o.sent_at < cutoff)
        .map(|(c, _)| c)
        .collect();
    for &client_id in &old {
        ctx.send(Request::Cancel { instrument, client_id });
    }
    old.len()
}

/// Open quantity of the agent's live orders on one side (excluding ones being cancelled).
pub fn working_qty(ctx: &Ctx<'_>, instrument: InstrumentId, side: Side) -> Qty {
    ctx.orders()
        .live()
        .filter(|(_, o)| o.instrument == instrument && o.side == side && !o.cancel_pending)
        .map(|(_, o)| o.leaves)
        .sum()
}

/// Stops an agent after a loss. On each check it marks the agent's own positions at its own
/// view's reference prices (before fees); below `−max_loss` it cancels all orders and trips for
/// good. A kill switch stops trading; it does not flatten the position.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KillSwitch {
    /// Largest tolerated mark-to-market loss, price units × quantity (`None` = never trips).
    pub max_loss: Option<i128>,
    /// When the switch tripped (`None` while armed).
    pub tripped_at: Option<Timestamp>,
}

impl KillSwitch {
    /// Kill switch that trips at a loss of `max_loss`.
    pub fn at_loss(max_loss: i128) -> Self {
        KillSwitch { max_loss: Some(max_loss), tripped_at: None }
    }

    /// Whether the switch has tripped.
    pub fn tripped(&self) -> bool {
        self.tripped_at.is_some()
    }

    /// Mark-to-market PnL of the agent on these instruments, from its own view.
    pub fn pnl(ctx: &Ctx<'_>, instruments: &[InstrumentId]) -> i128 {
        instruments
            .iter()
            .map(|&i| {
                let mark = View::of(ctx, i)
                    .map(|v| v.reference())
                    .or_else(|| ctx.spec(i).map(|s| s.reference_price as f64))
                    .unwrap_or(0.0);
                ctx.orders().cash(i) + i128::from(ctx.orders().position(i)) * mark.round() as i128
            })
            .sum()
    }

    /// Whether the agent may keep trading; trips (and cancels everything) on a breach.
    pub fn check(&mut self, ctx: &mut Ctx<'_>, instruments: &[InstrumentId]) -> bool {
        if self.tripped() {
            return false;
        }
        let Some(max_loss) = self.max_loss else { return true };
        if Self::pnl(ctx, instruments) < -max_loss {
            self.tripped_at = Some(ctx.now());
            for &i in instruments {
                cancel_all(ctx, i);
            }
            return false;
        }
        true
    }
}

/// Random side, buy or sell with equal probability.
pub fn random_side(rng: &mut Rng) -> Side {
    if rng.chance(0.5) { Side::Buy } else { Side::Sell }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_rounding() {
        assert_eq!(floor_tick(100.9, 5), 100);
        assert_eq!(ceil_tick(100.1, 5), 105);
        assert_eq!(floor_tick(-0.5, 1), -1);
        assert_eq!(ceil_tick(100.0, 5), 100);
    }

    #[test]
    fn ewma_tracks_mean_and_spread() {
        // The EWMA mean of i.i.d. N(50, 2²) draws has sd 2·√(α/(2 − α)) ≈ 0.063 for α = 0.002,
        // so the tolerances below are about five standard deviations.
        let mut e = Ewma::new(0.002);
        let mut rng = Rng::new(1);
        for _ in 0..20_000 {
            e.update(50.0 + 2.0 * detmath::standard_normal(&mut rng));
        }
        assert!((e.mean().unwrap() - 50.0).abs() < 0.3, "mean {}", e.mean().unwrap());
        assert!((e.sd() - 2.0).abs() < 0.2, "sd {}", e.sd());
        assert_eq!(e.count(), 20_000);
        assert_eq!(Ewma::new(0.5).mean(), None);
    }

    #[test]
    fn perception_noise_has_the_configured_spread() {
        let p = Perception { noise_sd: 3.0 };
        let mut rng = Rng::new(2);
        let draws: Vec<f64> = (0..50_000).map(|_| p.observe(1_000, &mut rng) - 1_000.0).collect();
        let var = draws.iter().map(|d| d * d).sum::<f64>() / draws.len() as f64;
        assert!((var.sqrt() - 3.0).abs() < 0.05);
        assert_eq!(Perception::default().observe(7, &mut rng), 7.0);
    }
}
