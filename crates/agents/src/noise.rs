//! Noise trader: zero-intelligence order flow in the spirit of the Santa Fe model (limit orders,
//! market orders and cancellations arriving at random; Smith, Farmer, Gillemot & Krishnamurthy
//! 2003). It is the background liquidity of the simulated market.
//!
//! Passive orders: the distance in ticks is geometric, and a share of orders improves the quote
//! when the spread allows. Orders older than a maximum age are cancelled, as most real limit
//! orders are. Marketable orders are IOC limits with price protection rather than unprotected
//! market orders.
//!
//! Where the distance is measured from:
//! * pure zero intelligence (`anchor_memory_ns: None`): from the current same-side best quote.
//!   Liquidity then follows the price, which makes metaorder impact linear in size;
//! * anchored (`anchor_memory_ns: Some(τ)`): from an exponential moving average of the trader's own
//!   view of the mid with time constant τ, clamped so an order never crosses. When the price runs
//!   away from the anchor, orders pile up at the touch against the move. This is latent liquidity
//!   revealed by price moves (in the spirit of the latent order book of Tóth et al. 2011 and
//!   Donier et al. 2015): the mechanism behind concave and partly transient impact.

use lob::{Side, TimeInForce, Timestamp};
use sim::{Agent, ClientOrderId, Ctx, InstrumentId, Request, detmath};

use crate::common::{
    Limits, Schedule, View, cancel_older_than, ceil_tick, floor_tick, random_side, send_limit, send_marketable,
};

/// Zero-intelligence noise trader.
#[derive(Clone, Debug)]
pub struct NoiseTrader {
    /// Instrument traded.
    pub instrument: InstrumentId,
    /// When the trader acts.
    pub schedule: Schedule,
    /// Share of actions that send a marketable order.
    pub p_market: f64,
    /// Share of actions that cancel one random live order (no action when there is none).
    pub p_cancel: f64,
    /// Success probability of the geometric distance (in ticks) behind the same-side best quote.
    pub depth_decay: f64,
    /// Probability that a passive order improves the best quote by one tick when the spread allows.
    pub p_improve: f64,
    /// Order size range, in lots.
    pub lots: (i64, i64),
    /// Cancel any order older than this.
    pub max_order_age_ns: Option<i64>,
    /// Marketable orders cross the opposite best by at most this many ticks.
    pub protection_ticks: i64,
    /// Anchor passive orders to an exponential moving average of the mid with this time constant,
    /// in nanoseconds (`None`: measure from the current best quotes).
    pub anchor_memory_ns: Option<f64>,
    /// Strategy-side limits.
    pub limits: Limits,
    /// Stop acting after this time.
    pub stop_at: Option<Timestamp>,
    /// Actions taken.
    pub actions: u64,
    /// Current anchor price and when it was last updated.
    pub anchor: Option<(f64, Timestamp)>,
}

impl NoiseTrader {
    /// Defaults: one action per 10 ms on average, small orders near the touch.
    pub fn new(instrument: InstrumentId) -> Self {
        NoiseTrader {
            instrument,
            schedule: Schedule::Poisson { mean_ns: 10_000_000.0 },
            p_market: 0.05,
            p_cancel: 0.4,
            depth_decay: 0.3,
            p_improve: 0.1,
            lots: (1, 5),
            max_order_age_ns: Some(2_000_000_000),
            protection_ticks: 2,
            anchor_memory_ns: None,
            limits: Limits::default(),
            stop_at: None,
            actions: 0,
            anchor: None,
        }
    }

    fn schedule_next(&self, ctx: &mut Ctx<'_>) {
        let at = self.schedule.next(ctx.now(), ctx.rng());
        if self.stop_at.is_none_or(|s| at <= s) {
            ctx.set_timer(at, 0);
        }
    }

    fn act(&mut self, ctx: &mut Ctx<'_>) {
        let i = self.instrument;
        let Some(spec) = ctx.spec(i).copied() else { return };
        self.actions += 1;
        if let Some(age) = self.max_order_age_ns {
            cancel_older_than(ctx, i, age);
        }
        let u = ctx.rng().f64();
        if u < self.p_cancel {
            let live: Vec<ClientOrderId> = ctx
                .orders()
                .live()
                .filter(|(_, o)| o.instrument == i && !o.cancel_pending)
                .map(|(c, _)| c)
                .collect();
            if !live.is_empty() {
                let k = ctx.rng().index(live.len());
                ctx.send(Request::Cancel { instrument: i, client_id: live[k] });
            }
            // A cancel with nothing to cancel does nothing; it must not turn into another action.
            return;
        }
        let side = random_side(ctx.rng());
        let lots = ctx.rng().range_i64(self.lots.0, self.lots.1);
        let qty = lots * spec.lot_size;
        if u < self.p_cancel + self.p_market {
            send_marketable(ctx, i, side, qty, self.protection_ticks, &self.limits);
            return;
        }
        let tick = spec.tick_size;
        let behind = ctx.rng().geometric(self.depth_decay) as i64 * tick;
        let improve = ctx.rng().chance(self.p_improve);
        let price = {
            let Some(view) = View::of(ctx, i) else { return };
            let (bid, ask, reference) = (view.bid(), view.ask(), view.reference());
            let can_improve = view.spread_ticks().is_some_and(|s| s > 1);
            if let Some(tau) = self.anchor_memory_ns {
                let now = ctx.now();
                let a = match self.anchor {
                    None => reference,
                    Some((a, at)) => a + (1.0 - detmath::exp(-((now - at) as f64) / tau)) * (reference - a),
                };
                self.anchor = Some((a, now));
            }
            match (side, self.anchor_memory_ns.and(self.anchor.map(|x| x.0))) {
                (Side::Buy, None) => {
                    let base = bid.unwrap_or_else(|| floor_tick(reference, tick) - tick);
                    let p = if improve && can_improve { base + tick } else { base - behind };
                    ask.map_or(p, |a| p.min(a - tick))
                }
                (Side::Sell, None) => {
                    let base = ask.unwrap_or_else(|| ceil_tick(reference, tick) + tick);
                    let p = if improve && can_improve { base - tick } else { base + behind };
                    bid.map_or(p, |b| p.max(b + tick))
                }
                (Side::Buy, Some(anchor)) => {
                    let p = match bid {
                        Some(b) if improve && can_improve => b + tick,
                        _ => floor_tick(anchor, tick) - behind,
                    };
                    ask.map_or(p, |a| p.min(a - tick))
                }
                (Side::Sell, Some(anchor)) => {
                    let p = match ask {
                        Some(a) if improve && can_improve => a - tick,
                        _ => ceil_tick(anchor, tick) + behind,
                    };
                    bid.map_or(p, |b| p.max(b + tick))
                }
            }
        };
        send_limit(ctx, i, side, price, qty, TimeInForce::Gtc, &self.limits);
    }
}

impl Agent for NoiseTrader {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        self.schedule_next(ctx);
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _token: u64) {
        if self.stop_at.is_some_and(|s| ctx.now() > s) {
            return;
        }
        self.act(ctx);
        self.schedule_next(ctx);
    }
}
