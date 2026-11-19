//! Agents for tests, demos and load generation. Real strategy agents arrive in phase 2.

use lob::{Event, Side, TimeInForce, Timestamp};

use crate::agent::{Agent, Ctx};
use crate::detmath;
use crate::messages::{ClientOrderId, ExecReport, InstrumentId, OrderSpec, Request, SignalId};

/// Sends a fixed list of requests at fixed times and records everything it receives
/// (with arrival times). For tests and examples.
#[derive(Clone, Debug, Default)]
pub struct Scripted {
    script: Vec<(Timestamp, Request)>,
    /// Execution reports received: `(arrival time, report)`.
    pub reports: Vec<(Timestamp, ExecReport)>,
    /// Market-data packets received: `(arrival time, instrument, events)`.
    pub market_data: Vec<(Timestamp, InstrumentId, Vec<Event>)>,
    /// Signal values received: `(arrival time, signal, value)`.
    pub signals: Vec<(Timestamp, SignalId, i64)>,
}

impl Scripted {
    /// Agent that sends `script` (time, request) entries; same-time entries go in list order.
    pub fn new(script: Vec<(Timestamp, Request)>) -> Self {
        Scripted { script, ..Self::default() }
    }

    /// Agent that sends nothing and only records.
    pub fn silent() -> Self {
        Self::default()
    }
}

impl Agent for Scripted {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        for (k, (t, _)) in self.script.iter().enumerate() {
            ctx.set_timer(*t, k as u64);
        }
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, token: u64) {
        let req = self.script[token as usize].1;
        ctx.send(req);
    }

    fn on_report(&mut self, ctx: &mut Ctx<'_>, report: &ExecReport) {
        self.reports.push((ctx.now(), *report));
    }

    fn on_market_data(&mut self, ctx: &mut Ctx<'_>, instrument: InstrumentId, events: &[Event]) {
        self.market_data.push((ctx.now(), instrument, events.to_vec()));
    }

    fn on_signal(&mut self, ctx: &mut Ctx<'_>, signal: SignalId, value: i64) {
        self.signals.push((ctx.now(), signal, value));
    }
}

/// Zero-intelligence trader used as load and background noise. At exponentially distributed
/// intervals it cancels one of its orders, sends a marketable IOC order, or places a passive
/// limit order around the mid of its own (stale) view of the book. Not calibrated to any
/// market; phase 2 replaces it with calibrated agents.
#[derive(Clone, Debug)]
pub struct ZeroIntelligence {
    /// Instrument traded.
    pub instrument: InstrumentId,
    /// Mean time between actions, in nanoseconds.
    pub mean_interval_ns: f64,
    /// Probability that an action is a cancel (when the agent has live orders).
    pub p_cancel: f64,
    /// Probability that an action is a marketable order.
    pub p_take: f64,
    /// Most live orders kept.
    pub max_open: usize,
    /// Largest order, in lots.
    pub max_lots: i64,
    /// Passive orders go 1..=this many ticks away from the agent's mid.
    pub max_distance_ticks: i64,
    /// Stop acting after this time (lets a test reach a quiet state).
    pub stop_at: Option<Timestamp>,
    /// Actions taken.
    pub actions: u64,
}

impl ZeroIntelligence {
    /// Defaults: one action per millisecond on average.
    pub fn new(instrument: InstrumentId) -> Self {
        ZeroIntelligence {
            instrument,
            mean_interval_ns: 1_000_000.0,
            p_cancel: 0.35,
            p_take: 0.1,
            max_open: 10,
            max_lots: 5,
            max_distance_ticks: 10,
            stop_at: None,
            actions: 0,
        }
    }

    fn schedule_next(&self, ctx: &mut Ctx<'_>) {
        let dt = (detmath::exponential(self.mean_interval_ns, ctx.rng()).round() as i64).max(1);
        let at = ctx.now() + dt;
        if self.stop_at.is_none_or(|s| at <= s) {
            ctx.set_timer(at, 0);
        }
    }

    fn act(&mut self, ctx: &mut Ctx<'_>) {
        let i = self.instrument;
        let Some(spec) = ctx.spec(i).copied() else { return };
        let (tick, lot) = (spec.tick_size, spec.lot_size);
        let (bid, ask, last) = match ctx.book(i) {
            Some(b) => (b.best_bid().map(|x| x.0), b.best_ask().map(|x| x.0), b.last_trade().map(|x| x.0)),
            None => (None, None, None),
        };
        let mid = match (bid, ask) {
            (Some(b), Some(a)) => (b + a) / 2 / tick * tick,
            (Some(b), None) => b,
            (None, Some(a)) => a,
            (None, None) => last.unwrap_or(spec.reference_price),
        };
        let clamp = |p: i64| p.clamp(spec.min_price, spec.max_price);
        self.actions += 1;

        let u = ctx.rng().f64();
        let live: Vec<ClientOrderId> = ctx
            .orders()
            .live()
            .filter(|(_, o)| o.instrument == i && !o.cancel_pending)
            .map(|(c, _)| c)
            .collect();
        if u < self.p_cancel && !live.is_empty() {
            let k = ctx.rng().index(live.len());
            ctx.send(Request::Cancel { instrument: i, client_id: live[k] });
            return;
        }
        let side = if ctx.rng().chance(0.5) { Side::Buy } else { Side::Sell };
        let qty = lot * ctx.rng().range_i64(1, self.max_lots);
        if u < self.p_cancel + self.p_take {
            let through = tick * ctx.rng().range_i64(0, 2);
            let price = match side {
                Side::Buy => ask.unwrap_or(mid) + through,
                Side::Sell => bid.unwrap_or(mid) - through,
            };
            let cid = ctx.next_client_id();
            ctx.send(Request::New(OrderSpec::limit(i, cid, side, clamp(price), qty).with_tif(TimeInForce::Ioc)));
        } else if ctx.orders().live_count(i) < self.max_open {
            let d = tick * ctx.rng().range_i64(1, self.max_distance_ticks);
            let price = match side {
                Side::Buy => mid - d,
                Side::Sell => mid + d,
            };
            let cid = ctx.next_client_id();
            ctx.send(Request::New(OrderSpec::limit(i, cid, side, clamp(price), qty)));
        }
    }
}

impl Agent for ZeroIntelligence {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        self.schedule_next(ctx);
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _token: u64) {
        self.act(ctx);
        self.schedule_next(ctx);
    }
}
