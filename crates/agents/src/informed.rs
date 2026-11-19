//! Informed trader: trades on private information about fundamental value, the adverse-selection
//! risk that market makers price into the spread (Glosten & Milgrom 1985; Kyle 1985).
//!
//! The trader subscribes to a value signal. Information quality is its signal latency (how late
//! it learns) plus Gaussian observation noise. When its estimate exceeds the best ask (or falls
//! below the best bid) of its own book view by a threshold, it takes liquidity with an IOC limit
//! that never pays more than estimate − threshold.

use lob::{Side, TimeInForce, Timestamp};
use sim::{Agent, Ctx, InstrumentId, SignalId};

use crate::common::{KillSwitch, Limits, Perception, Schedule, View, ceil_tick, floor_tick, send_limit};

/// Informed (value-driven, liquidity-taking) trader.
#[derive(Clone, Debug)]
pub struct InformedTrader {
    /// Instrument traded.
    pub instrument: InstrumentId,
    /// Value signal observed.
    pub signal: SignalId,
    /// Observation noise.
    pub perception: Perception,
    /// Minimum edge, in ticks, between the estimate and the price paid.
    pub threshold_ticks: i64,
    /// Order size, in lots.
    pub clip_lots: i64,
    /// Strategy-side limits.
    pub limits: Limits,
    /// Loss kill switch (never trips by default).
    pub kill: KillSwitch,
    /// Optional periodic re-check (the book can move while the estimate stays put).
    pub recheck: Option<Schedule>,
    /// Stop acting after this time.
    pub stop_at: Option<Timestamp>,
    /// Latest value estimate.
    pub estimate: Option<f64>,
    /// Orders sent.
    pub orders_sent: u64,
}

impl InformedTrader {
    /// Defaults: perfect information, 2-tick threshold, one lot per order.
    pub fn new(instrument: InstrumentId, signal: SignalId) -> Self {
        InformedTrader {
            instrument,
            signal,
            perception: Perception::default(),
            threshold_ticks: 2,
            clip_lots: 1,
            limits: Limits::default(),
            kill: KillSwitch::default(),
            recheck: Some(Schedule::Poisson { mean_ns: 50_000_000.0 }),
            stop_at: None,
            estimate: None,
            orders_sent: 0,
        }
    }

    fn act(&mut self, ctx: &mut Ctx<'_>) {
        let i = self.instrument;
        if self.stop_at.is_some_and(|s| ctx.now() > s) || !self.kill.check(ctx, &[i]) {
            return;
        }
        let Some(estimate) = self.estimate else { return };
        let order = {
            let Some(view) = View::of(ctx, i) else { return };
            let tick = view.spec.tick_size;
            let edge = (self.threshold_ticks * tick) as f64;
            match (view.bid(), view.ask()) {
                (_, Some(ask)) if estimate - ask as f64 >= edge => Some((Side::Buy, floor_tick(estimate - edge, tick))),
                (Some(bid), _) if bid as f64 - estimate >= edge => Some((Side::Sell, ceil_tick(estimate + edge, tick))),
                _ => None,
            }
        };
        if let Some((side, price)) = order {
            let qty = self.clip_lots * ctx.spec(i).map_or(1, |s| s.lot_size);
            if send_limit(ctx, i, side, price, qty, TimeInForce::Ioc, &self.limits).is_some() {
                self.orders_sent += 1;
            }
        }
    }
}

impl Agent for InformedTrader {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(s) = self.recheck {
            let at = s.next(ctx.now(), ctx.rng());
            ctx.set_timer(at, 0);
        }
    }

    fn on_signal(&mut self, ctx: &mut Ctx<'_>, signal: SignalId, value: i64) {
        if signal == self.signal {
            self.estimate = Some(self.perception.observe(value, ctx.rng()));
            self.act(ctx);
        }
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _token: u64) {
        self.act(ctx);
        if self.kill.tripped() {
            return;
        }
        if let Some(s) = self.recheck {
            let at = s.next(ctx.now(), ctx.rng());
            if self.stop_at.is_none_or(|stop| at <= stop) {
                ctx.set_timer(at, 0);
            }
        }
    }
}
