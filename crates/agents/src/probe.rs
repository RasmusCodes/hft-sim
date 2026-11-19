//! Measurement probe: rests one small limit order at the touch at a time and cancels it after a
//! fixed life. It is an instrument, not a strategy. The exchange's own log then shows what really
//! happened to each resting order: the ground truth against which replay fill models are checked
//! (phase 5, docs/HISTORICAL_DATA.md §4).

use lob::{Side, TimeInForce, Timestamp};
use sim::{Agent, Ctx, InstrumentId, Request};

use crate::common::{Limits, Schedule, View, send_limit};

/// Rests one order at a time at its view of the best bid or best ask, alternating sides.
#[derive(Clone, Debug)]
pub struct ProbeTrader {
    /// Instrument.
    pub instrument: InstrumentId,
    /// When to try to place the next probe; skipped while one is live.
    pub schedule: Schedule,
    /// A probe is cancelled this long after it was sent, if still live.
    pub lifetime_ns: i64,
    /// Probe size, in lots.
    pub lots: i64,
    /// Limits (generous: probes accumulate a small random-walk position).
    pub limits: Limits,
    /// Stop placing probes after this time.
    pub stop_at: Option<Timestamp>,
    /// Probes sent.
    pub placed: u64,
    next_side: Side,
}

impl ProbeTrader {
    /// Defaults: a placement attempt every second on average, a 5-second life, one lot.
    pub fn new(instrument: InstrumentId) -> Self {
        ProbeTrader {
            instrument,
            schedule: Schedule::Poisson { mean_ns: 1e9 },
            lifetime_ns: 5_000_000_000,
            lots: 1,
            limits: Limits { max_position: 1_000_000, max_order_qty: 1_000, max_open_orders: 2, capital: i128::MAX },
            stop_at: None,
            placed: 0,
            next_side: Side::Buy,
        }
    }
}

const PLACE: u64 = 0;

impl Agent for ProbeTrader {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        let at = self.schedule.next(ctx.now(), ctx.rng());
        ctx.set_timer(at, PLACE);
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, token: u64) {
        let i = self.instrument;
        if token != PLACE {
            // Expiry of the probe with client id token − 1, if it is still resting.
            let cid = token - 1;
            if ctx.orders().live().any(|(c, o)| c == cid && !o.cancel_pending) {
                ctx.send(Request::Cancel { instrument: i, client_id: cid });
            }
            return;
        }
        if self.stop_at.is_some_and(|s| ctx.now() > s) {
            return;
        }
        if ctx.orders().live_count(i) == 0 {
            let side = self.next_side;
            let price = View::of(ctx, i).and_then(|v| match side {
                Side::Buy => v.bid(),
                Side::Sell => v.ask(),
            });
            let lot = ctx.spec(i).map_or(1, |s| s.lot_size);
            if let Some(p) = price {
                if let Some(cid) = send_limit(ctx, i, side, p, self.lots * lot, TimeInForce::Gtc, &self.limits) {
                    self.placed += 1;
                    self.next_side = side.opposite();
                    ctx.set_timer(ctx.now() + self.lifetime_ns, cid + 1);
                }
            }
        }
        let at = self.schedule.next(ctx.now(), ctx.rng());
        ctx.set_timer(at, PLACE);
    }
}
