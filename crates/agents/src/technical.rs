//! Technical traders that read only their own (delayed) view of the book.
//!
//! * [`MomentumTrader`] follows trends: fast minus slow moving average of the mid, entering with
//!   marketable orders (it takes liquidity).
//! * [`MeanReversionTrader`] fades deviations from a moving average with passive quotes at the
//!   touch (it provides liquidity).
//!
//! Both use hysteresis (separate entry and exit thresholds) so they do not flip on every tick.

use lob::{Qty, Side, TimeInForce, Timestamp};
use sim::{Agent, Ctx, InstrumentId};

use crate::common::{
    Ewma, KillSwitch, Limits, Schedule, View, cancel_all, ceil_tick, floor_tick, send_limit, send_marketable,
};

/// Position target from a signal with hysteresis: enter beyond `entry`, flatten inside `exit`,
/// otherwise keep the current target.
fn hysteresis(signal: f64, entry: f64, exit: f64, current: i64, size: i64) -> i64 {
    if signal > entry {
        size
    } else if signal < -entry {
        -size
    } else if signal.abs() < exit {
        0
    } else {
        current
    }
}

/// Position including open orders (what the agent would hold if everything filled).
fn potential_position(ctx: &Ctx<'_>, instrument: InstrumentId) -> Qty {
    let orders = ctx.orders();
    let open: Qty = orders
        .live()
        .filter(|(_, o)| o.instrument == instrument && !o.cancel_pending)
        .map(|(_, o)| if o.side == Side::Buy { o.leaves } else { -o.leaves })
        .sum();
    orders.position(instrument) + open
}

/// Trend follower on its own view of the mid.
#[derive(Clone, Debug)]
pub struct MomentumTrader {
    /// Instrument traded.
    pub instrument: InstrumentId,
    /// When the trader samples the mid and trades.
    pub schedule: Schedule,
    /// EWMA weight of the fast average.
    pub fast_alpha: f64,
    /// EWMA weight of the slow average.
    pub slow_alpha: f64,
    /// Enter when fast − slow exceeds this many ticks.
    pub entry_ticks: f64,
    /// Flatten when |fast − slow| falls below this many ticks.
    pub exit_ticks: f64,
    /// Target position size, in lots.
    pub position_lots: i64,
    /// Largest order per wake-up, in lots.
    pub clip_lots: i64,
    /// Samples before trading.
    pub warmup: u64,
    /// Marketable orders cross the opposite best by at most this many ticks.
    pub protection_ticks: i64,
    /// Strategy-side limits.
    pub limits: Limits,
    /// Loss kill switch (never trips by default).
    pub kill: KillSwitch,
    /// Stop acting after this time.
    pub stop_at: Option<Timestamp>,
    /// Current target, in lots.
    pub target_lots: i64,
    fast: Ewma,
    slow: Ewma,
}

impl MomentumTrader {
    /// Defaults: samples every 100 ms.
    pub fn new(instrument: InstrumentId) -> Self {
        MomentumTrader {
            instrument,
            schedule: Schedule::Periodic { period_ns: 100_000_000 },
            fast_alpha: 0.3,
            slow_alpha: 0.03,
            entry_ticks: 1.0,
            exit_ticks: 0.25,
            position_lots: 10,
            clip_lots: 5,
            warmup: 30,
            protection_ticks: 2,
            limits: Limits::default(),
            kill: KillSwitch::default(),
            stop_at: None,
            target_lots: 0,
            fast: Ewma::new(0.3),
            slow: Ewma::new(0.03),
        }
    }
}

impl Agent for MomentumTrader {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        self.fast = Ewma::new(self.fast_alpha);
        self.slow = Ewma::new(self.slow_alpha);
        let at = self.schedule.next(ctx.now(), ctx.rng());
        ctx.set_timer(at, 0);
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _token: u64) {
        let i = self.instrument;
        if self.stop_at.is_some_and(|s| ctx.now() > s) || !self.kill.check(ctx, &[i]) {
            return;
        }
        let sample = View::of(ctx, i).and_then(|v| Some((v.mid()?, v.spec.tick_size, v.spec.lot_size)));
        if let Some((mid, tick, lot)) = sample {
            self.fast.update(mid);
            self.slow.update(mid);
            if self.slow.count() >= self.warmup {
                let signal = (self.fast.mean().unwrap_or(mid) - self.slow.mean().unwrap_or(mid)) / tick as f64;
                self.target_lots =
                    hysteresis(signal, self.entry_ticks, self.exit_ticks, self.target_lots, self.position_lots);
                let diff = self.target_lots * lot - potential_position(ctx, i);
                let clip = self.clip_lots * lot;
                if diff > 0 {
                    send_marketable(ctx, i, Side::Buy, diff.min(clip), self.protection_ticks, &self.limits);
                } else if diff < 0 {
                    send_marketable(ctx, i, Side::Sell, (-diff).min(clip), self.protection_ticks, &self.limits);
                }
            }
        }
        let at = self.schedule.next(ctx.now(), ctx.rng());
        ctx.set_timer(at, 0);
    }
}

/// Contrarian liquidity provider on its own view of the mid.
#[derive(Clone, Debug)]
pub struct MeanReversionTrader {
    /// Instrument traded.
    pub instrument: InstrumentId,
    /// When the trader samples the mid and re-quotes.
    pub schedule: Schedule,
    /// EWMA weight of the mean and variance estimates.
    pub alpha: f64,
    /// Enter when the z-score exceeds this.
    pub z_entry: f64,
    /// Flatten when |z| falls below this.
    pub z_exit: f64,
    /// Target position size, in lots.
    pub position_lots: i64,
    /// Samples before trading.
    pub warmup: u64,
    /// Strategy-side limits.
    pub limits: Limits,
    /// Loss kill switch (never trips by default).
    pub kill: KillSwitch,
    /// Stop acting after this time.
    pub stop_at: Option<Timestamp>,
    /// Current target, in lots.
    pub target_lots: i64,
    stats: Ewma,
}

impl MeanReversionTrader {
    /// Defaults: re-quotes every 200 ms.
    pub fn new(instrument: InstrumentId) -> Self {
        MeanReversionTrader {
            instrument,
            schedule: Schedule::Periodic { period_ns: 200_000_000 },
            alpha: 0.02,
            z_entry: 1.5,
            z_exit: 0.3,
            position_lots: 10,
            warmup: 50,
            limits: Limits::default(),
            kill: KillSwitch::default(),
            stop_at: None,
            target_lots: 0,
            stats: Ewma::new(0.02),
        }
    }
}

impl Agent for MeanReversionTrader {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        self.stats = Ewma::new(self.alpha);
        let at = self.schedule.next(ctx.now(), ctx.rng());
        ctx.set_timer(at, 0);
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _token: u64) {
        let i = self.instrument;
        if self.stop_at.is_some_and(|s| ctx.now() > s) || !self.kill.check(ctx, &[i]) {
            return;
        }
        let sample = View::of(ctx, i).and_then(|v| {
            let tick = v.spec.tick_size;
            Some((v.mid()?, v.bid()?, v.ask()?, tick, v.spec.lot_size))
        });
        if let Some((mid, bid, ask, tick, lot)) = sample {
            self.stats.update(mid);
            if self.stats.count() >= self.warmup {
                let sd = self.stats.sd().max(tick as f64 / 2.0);
                let z = (mid - self.stats.mean().unwrap_or(mid)) / sd;
                // Contrarian: a high price is a reason to be short.
                self.target_lots = hysteresis(-z, self.z_entry, self.z_exit, self.target_lots, self.position_lots);
                cancel_all(ctx, i);
                let diff = self.target_lots * lot - ctx.orders().position(i);
                if diff > 0 {
                    let price = floor_tick(bid as f64, tick);
                    send_limit(ctx, i, Side::Buy, price, diff, TimeInForce::Gtc, &self.limits);
                } else if diff < 0 {
                    let price = ceil_tick(ask as f64, tick);
                    send_limit(ctx, i, Side::Sell, price, -diff, TimeInForce::Gtc, &self.limits);
                }
            }
        }
        let at = self.schedule.next(ctx.now(), ctx.rng());
        ctx.set_timer(at, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::hysteresis;

    #[test]
    fn hysteresis_enters_holds_and_exits() {
        assert_eq!(hysteresis(2.0, 1.0, 0.3, 0, 5), 5);
        assert_eq!(hysteresis(0.5, 1.0, 0.3, 5, 5), 5, "between thresholds: hold");
        assert_eq!(hysteresis(0.1, 1.0, 0.3, 5, 5), 0, "inside exit band: flat");
        assert_eq!(hysteresis(-1.5, 1.0, 0.3, 5, 5), -5);
    }
}
