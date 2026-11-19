//! Fundamental trader: a patient value investor. It learns fundamental value through a (usually
//! slow and noisy) signal and sets a target position proportional to the part of the mispricing
//! beyond its own threshold: target = κ·sign(gap)·max(|gap| − θ, 0). It works toward the target
//! with passive orders at the touch, never paying more than its value estimate.
//!
//! A population of these anchors the price to value. With thresholds spread uniformly over
//! [0, θ_max], the population's total opposing supply grows like the square of the price's
//! displacement (Σ_j (x − θ_j)⁺ ≈ N·x²/(2θ_max)). That is the "latent liquidity" shape under which
//! a metaorder's impact grows like the square root of its size (Tóth et al. 2011).

use lob::{Side, TimeInForce, Timestamp};
use sim::{Agent, Ctx, InstrumentId, SignalId};

use crate::common::{Limits, Perception, Schedule, View, cancel_all, ceil_tick, floor_tick, send_limit};

/// Value investor working passive orders toward a mispricing-driven target.
#[derive(Clone, Debug)]
pub struct FundamentalTrader {
    /// Instrument traded.
    pub instrument: InstrumentId,
    /// Value signal observed.
    pub signal: SignalId,
    /// Observation noise.
    pub perception: Perception,
    /// When the trader re-quotes.
    pub schedule: Schedule,
    /// Target lots per tick of mispricing beyond the threshold.
    pub lots_per_tick: f64,
    /// Mispricing (ticks) the trader ignores.
    pub threshold_ticks: f64,
    /// Largest target position, in lots.
    pub max_lots: i64,
    /// Strategy-side limits.
    pub limits: Limits,
    /// Stop acting after this time.
    pub stop_at: Option<Timestamp>,
    /// Latest value estimate.
    pub estimate: Option<f64>,
}

impl FundamentalTrader {
    /// Defaults: re-quotes every 500 ms on average.
    pub fn new(instrument: InstrumentId, signal: SignalId) -> Self {
        FundamentalTrader {
            instrument,
            signal,
            perception: Perception::default(),
            schedule: Schedule::Poisson { mean_ns: 500_000_000.0 },
            lots_per_tick: 1.0,
            threshold_ticks: 0.0,
            max_lots: 20,
            limits: Limits::default(),
            stop_at: None,
            estimate: None,
        }
    }
}

impl Agent for FundamentalTrader {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        let at = self.schedule.next(ctx.now(), ctx.rng());
        ctx.set_timer(at, 0);
    }

    fn on_signal(&mut self, ctx: &mut Ctx<'_>, signal: SignalId, value: i64) {
        if signal == self.signal {
            self.estimate = Some(self.perception.observe(value, ctx.rng()));
        }
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _token: u64) {
        if self.stop_at.is_some_and(|s| ctx.now() > s) {
            return;
        }
        let i = self.instrument;
        if let Some(estimate) = self.estimate {
            let quote = View::of(ctx, i).map(|v| (v.reference(), v.bid(), v.ask(), v.spec.tick_size, v.spec.lot_size));
            if let Some((mid, bid, ask, tick, lot)) = quote {
                let gap_ticks = (estimate - mid) / tick as f64;
                let beyond = (gap_ticks.abs() - self.threshold_ticks).max(0.0) * gap_ticks.signum();
                let target_lots =
                    (self.lots_per_tick * beyond).round().clamp(-(self.max_lots as f64), self.max_lots as f64) as i64;
                cancel_all(ctx, i);
                let diff = target_lots * lot - ctx.orders().position(i);
                if diff > 0 {
                    // Join the bid, but never bid above the value estimate.
                    let cap = floor_tick(estimate, tick);
                    let price = bid.map_or(cap - tick, |b| b.min(cap));
                    send_limit(ctx, i, Side::Buy, price, diff, TimeInForce::Gtc, &self.limits);
                } else if diff < 0 {
                    let floor = ceil_tick(estimate, tick);
                    let price = ask.map_or(floor + tick, |a| a.max(floor));
                    send_limit(ctx, i, Side::Sell, price, -diff, TimeInForce::Gtc, &self.limits);
                }
            }
        }
        let at = self.schedule.next(ctx.now(), ctx.rng());
        if self.stop_at.is_none_or(|s| at <= s) {
            ctx.set_timer(at, 0);
        }
    }
}
