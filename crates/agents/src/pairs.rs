//! Pairs trader (statistical arbitrage) on two instruments.
//!
//! It watches the spread s = mid_B − β·mid_A in its own views of both books and keeps
//! exponentially weighted estimates of the spread's mean and standard deviation. When the z-score
//! leaves ±`z_entry` it sells the rich leg and buys the cheap one in ratio β (marketable
//! orders on both legs); inside ±`z_exit` it flattens. Legs are sent back to back, so legging
//! risk (one leg filling, the other not) is real, as in practice.

use lob::{Qty, Side, Timestamp};
use sim::{Agent, Ctx, InstrumentId};

use crate::common::{Ewma, KillSwitch, Limits, Schedule, View, send_marketable};

/// Spread mean-reversion trader on instruments A and B.
#[derive(Clone, Debug)]
pub struct PairsTrader {
    /// First leg.
    pub a: InstrumentId,
    /// Second leg.
    pub b: InstrumentId,
    /// Hedge ratio: units of A per unit of B; spread = mid_B − β·mid_A.
    pub beta: f64,
    /// When the trader samples and trades.
    pub schedule: Schedule,
    /// EWMA weight of the spread mean and variance.
    pub alpha: f64,
    /// Samples before trading.
    pub warmup: u64,
    /// Enter when |z| exceeds this.
    pub z_entry: f64,
    /// Flatten when |z| falls below this.
    pub z_exit: f64,
    /// Position size of leg B, in lots (leg A follows by β).
    pub size_lots: i64,
    /// Largest order per leg per wake-up, in lots.
    pub clip_lots: i64,
    /// Marketable orders cross the opposite best by at most this many ticks.
    pub protection_ticks: i64,
    /// Limits for leg A.
    pub limits_a: Limits,
    /// Limits for leg B.
    pub limits_b: Limits,
    /// Loss kill switch over both legs (never trips by default).
    pub kill: KillSwitch,
    /// Stop acting after this time.
    pub stop_at: Option<Timestamp>,
    /// Current direction: +1 long spread (long B, short A), −1 short spread, 0 flat.
    pub direction: i64,
    spread: Ewma,
}

impl PairsTrader {
    /// Defaults: β = 1, samples every 100 ms, enters at |z| > 2, exits at |z| < 0.5.
    pub fn new(a: InstrumentId, b: InstrumentId) -> Self {
        PairsTrader {
            a,
            b,
            beta: 1.0,
            schedule: Schedule::Periodic { period_ns: 100_000_000 },
            alpha: 0.01,
            warmup: 100,
            z_entry: 2.0,
            z_exit: 0.5,
            size_lots: 10,
            clip_lots: 5,
            protection_ticks: 2,
            limits_a: Limits::default(),
            limits_b: Limits::default(),
            kill: KillSwitch::default(),
            stop_at: None,
            direction: 0,
            spread: Ewma::new(0.01),
        }
    }

    fn rebalance(&self, ctx: &mut Ctx<'_>, instrument: InstrumentId, desired: Qty, clip: Qty, limits: &Limits) {
        let orders = ctx.orders();
        let open: Qty = orders
            .live()
            .filter(|(_, o)| o.instrument == instrument && !o.cancel_pending)
            .map(|(_, o)| if o.side == Side::Buy { o.leaves } else { -o.leaves })
            .sum();
        let diff = desired - (orders.position(instrument) + open);
        if diff > 0 {
            send_marketable(ctx, instrument, Side::Buy, diff.min(clip), self.protection_ticks, limits);
        } else if diff < 0 {
            send_marketable(ctx, instrument, Side::Sell, (-diff).min(clip), self.protection_ticks, limits);
        }
    }
}

impl Agent for PairsTrader {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        self.spread = Ewma::new(self.alpha);
        let at = self.schedule.next(ctx.now(), ctx.rng());
        ctx.set_timer(at, 0);
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _token: u64) {
        if self.stop_at.is_some_and(|s| ctx.now() > s) || !self.kill.check(ctx, &[self.a, self.b]) {
            return;
        }
        let sample = (|| {
            let va = View::of(ctx, self.a)?;
            let vb = View::of(ctx, self.b)?;
            Some((va.mid()?, vb.mid()?, va.spec.lot_size, vb.spec.lot_size, vb.spec.tick_size))
        })();
        if let Some((mid_a, mid_b, lot_a, lot_b, tick_b)) = sample {
            let s = mid_b - self.beta * mid_a;
            self.spread.update(s);
            if self.spread.count() >= self.warmup {
                let sd = self.spread.sd().max(tick_b as f64 / 2.0);
                let z = (s - self.spread.mean().unwrap_or(s)) / sd;
                if z > self.z_entry {
                    self.direction = -1;
                } else if z < -self.z_entry {
                    self.direction = 1;
                } else if z.abs() < self.z_exit {
                    self.direction = 0;
                }
                let desired_b = self.direction * self.size_lots * lot_b;
                let desired_a = -((self.beta * desired_b as f64 / lot_a as f64).round() as Qty) * lot_a;
                let (la, lb) = (self.limits_a, self.limits_b);
                self.rebalance(ctx, self.b, desired_b, self.clip_lots * lot_b, &lb);
                self.rebalance(ctx, self.a, desired_a, self.clip_lots * lot_a, &la);
            }
        }
        let at = self.schedule.next(ctx.now(), ctx.rng());
        ctx.set_timer(at, 0);
    }
}
