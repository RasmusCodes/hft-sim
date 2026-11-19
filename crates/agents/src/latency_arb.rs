//! Latency arbitrageur: races to pick off stale quotes when fair value jumps (Budish, Cramton &
//! Shim 2015; Aquilina, Budish & O'Neill 2022).
//!
//! It subscribes to a value signal (news or a leading market) with a fast path. When an update
//! shows a resting ask below value − threshold (or a bid above value + threshold) in its own
//! book view, it sends an IOC at exactly that stale price, hoping to arrive before the quote's
//! owner cancels. It unwinds inventory passively at the value price on a timer.

use lob::{Side, TimeInForce, Timestamp};
use sim::{Agent, Ctx, InstrumentId, SignalId};

use crate::common::{KillSwitch, Limits, Schedule, View, cancel_all, ceil_tick, floor_tick, send_limit};

/// Stale-quote sniper.
#[derive(Clone, Debug)]
pub struct LatencyArbitrageur {
    /// Instrument traded.
    pub instrument: InstrumentId,
    /// Value signal observed.
    pub signal: SignalId,
    /// Minimum gap between value and the stale quote, in ticks.
    pub threshold_ticks: i64,
    /// Largest order, in lots.
    pub clip_lots: i64,
    /// Strategy-side limits.
    pub limits: Limits,
    /// Loss kill switch (never trips by default). Once tripped the agent stops sniping and
    /// unwinding alike.
    pub kill: KillSwitch,
    /// When to re-post the unwinding order.
    pub unwind: Schedule,
    /// Stop acting after this time.
    pub stop_at: Option<Timestamp>,
    /// Latest value received.
    pub value: Option<i64>,
    /// Sniping orders sent.
    pub snipes: u64,
}

impl LatencyArbitrageur {
    /// Defaults: 2-tick threshold, unwinds every 50 ms.
    pub fn new(instrument: InstrumentId, signal: SignalId) -> Self {
        LatencyArbitrageur {
            instrument,
            signal,
            threshold_ticks: 2,
            clip_lots: 5,
            limits: Limits::default(),
            kill: KillSwitch::default(),
            unwind: Schedule::Periodic { period_ns: 50_000_000 },
            stop_at: None,
            value: None,
            snipes: 0,
        }
    }
}

impl Agent for LatencyArbitrageur {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        let at = self.unwind.next(ctx.now(), ctx.rng());
        ctx.set_timer(at, 0);
    }

    fn on_signal(&mut self, ctx: &mut Ctx<'_>, signal: SignalId, value: i64) {
        let i = self.instrument;
        if signal != self.signal || self.stop_at.is_some_and(|s| ctx.now() > s) {
            return;
        }
        self.value = Some(value);
        if !self.kill.check(ctx, &[i]) {
            return;
        }
        let target = {
            let Some(view) = View::of(ctx, i) else { return };
            let edge = self.threshold_ticks * view.spec.tick_size;
            let clip = self.clip_lots * view.spec.lot_size;
            match (view.book.best_bid(), view.book.best_ask()) {
                (_, Some((ask, qty))) if ask <= value - edge => Some((Side::Buy, ask, qty.min(clip))),
                (Some((bid, qty)), _) if bid >= value + edge => Some((Side::Sell, bid, qty.min(clip))),
                _ => None,
            }
        };
        if let Some((side, price, qty)) = target {
            if send_limit(ctx, i, side, price, qty, TimeInForce::Ioc, &self.limits).is_some() {
                self.snipes += 1;
            }
        }
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _token: u64) {
        let i = self.instrument;
        if self.stop_at.is_some_and(|s| ctx.now() > s) || !self.kill.check(ctx, &[i]) {
            return;
        }
        if let (Some(value), Some(tick)) = (self.value, ctx.spec(i).map(|s| s.tick_size)) {
            let position = ctx.orders().position(i);
            cancel_all(ctx, i);
            if position > 0 {
                send_limit(ctx, i, Side::Sell, ceil_tick(value as f64, tick), position, TimeInForce::Gtc, &self.limits);
            } else if position < 0 {
                send_limit(ctx, i, Side::Buy, floor_tick(value as f64, tick), -position, TimeInForce::Gtc, &self.limits);
            }
        }
        let at = self.unwind.next(ctx.now(), ctx.rng());
        ctx.set_timer(at, 0);
    }
}
