//! The market-maker agent. It keeps its estimators current from its own feed and fills, asks its
//! [`QuotingStrategy`] for levels and reconciles them with its live orders:
//! * a resting order at a wanted price is kept, and cut down with a priority-keeping modify if
//!   it is larger than wanted;
//! * an order the queue rule says to keep is kept;
//! * any other order is cancelled, and missing levels get new orders.
//!
//! Every new order is post-only by default and passes the strategy-side `Limits` (position
//! assuming open orders fill). It requotes after each market-data packet (at most every
//! `min_requote_ns`), after each own fill, and on a heartbeat timer. A loss `KillSwitch` cancels
//! everything and stops the agent for good.

use lob::{EventKind, OrderId, Price, Qty, RejectReason, Side, StpMode, TimeInForce, Timestamp};
use sim::{Agent, ClientOrderId, Ctx, ExecReport, InstrumentId, OrderSpec, ReportKind, Request};

use super::estimators::{IntensityEstimator, Markouts, MicroPrice, OfiTracker, Top, VolEstimator};
use super::strategy::{Level, QuotingStrategy, Snapshot, keep_resting};
use crate::common::{KillSwitch, Limits};

/// Counters kept by a market maker about itself.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MmStats {
    /// Requote decisions.
    pub requotes: u64,
    /// New orders sent.
    pub orders_sent: u64,
    /// Cancels sent.
    pub cancels_sent: u64,
    /// Size-reducing modifies sent.
    pub modifies_sent: u64,
    /// Post-only orders rejected because they would have crossed (the view was stale).
    pub post_only_rejects: u64,
    /// Fills.
    pub fills: u64,
    /// Quantity bought.
    pub bought: Qty,
    /// Quantity sold.
    pub sold: Qty,
    /// Spread captured against the mid of the agent's view at each fill: Σ (mid − price)·qty
    /// for buys and (price − mid)·qty for sells, in price units × quantity.
    pub spread_capture: f64,
    /// Largest absolute position reached.
    pub max_abs_position: Qty,
    /// Time with live orders on both sides, nanoseconds (sampled at each requote and heartbeat).
    pub two_sided_ns: i64,
    /// Time observed, nanoseconds.
    pub observed_ns: i64,
}

impl MmStats {
    /// Share of observed time with quotes on both sides.
    pub fn two_sided_share(&self) -> f64 {
        if self.observed_ns > 0 { self.two_sided_ns as f64 / self.observed_ns as f64 } else { 0.0 }
    }
}

/// A market maker running one quoting strategy on one instrument.
#[derive(Clone, Debug)]
pub struct MarketMaker {
    /// Instrument.
    pub instrument: InstrumentId,
    /// Quoting rule.
    pub strategy: QuotingStrategy,
    /// Lots per quote unit.
    pub unit_lots: i64,
    /// Strategy-side limits (applied to every new order).
    pub limits: Limits,
    /// Loss kill switch.
    pub kill: KillSwitch,
    /// Send post-only orders.
    pub post_only: bool,
    /// Self-trade prevention instruction on every order.
    pub stp: StpMode,
    /// Minimum time between requotes triggered by market data.
    pub min_requote_ns: i64,
    /// Heartbeat period.
    pub heartbeat_ns: i64,
    /// Stop quoting (and cancel everything) after this time.
    pub stop_at: Option<Timestamp>,
    /// Volatility estimator.
    pub vol: VolEstimator,
    /// Order-flow imbalance.
    pub ofi: OfiTracker,
    /// Fill-intensity estimator.
    pub intensity: IntensityEstimator,
    /// Mark-outs of own fills.
    pub markouts: Markouts,
    /// Micro-price estimator.
    pub micro: MicroPrice,
    /// Counters.
    pub stats: MmStats,
    last_top: Option<Top>,
    last_requote: Option<Timestamp>,
    last_sample: Option<(Timestamp, bool)>,
    stopped: bool,
}

impl MarketMaker {
    /// Market maker with default machinery: 5 lots per unit, post-only, requotes at most every
    /// 0.5 ms on market data, heartbeat 100 ms. Limits follow the strategy's inventory bound.
    pub fn new(instrument: InstrumentId, strategy: QuotingStrategy) -> Self {
        let unit_lots = 5;
        let per_level = strategy.levels.units * unit_lots;
        let limits = Limits {
            max_position: strategy.max_inventory * unit_lots,
            max_order_qty: per_level,
            max_open_orders: 2 * strategy.levels.count + 4,
            capital: i128::MAX,
        };
        MarketMaker {
            instrument,
            strategy,
            unit_lots,
            limits,
            kill: KillSwitch::default(),
            post_only: true,
            stp: StpMode::None,
            min_requote_ns: 500_000,
            heartbeat_ns: 100_000_000,
            stop_at: None,
            vol: VolEstimator::new(60.0, 1.0),
            ofi: OfiTracker::new(1.0),
            intensity: IntensityEstimator::new(120.0),
            markouts: Markouts::new(1_000_000_000, 0.1),
            micro: MicroPrice::new(10, 2),
            stats: MmStats::default(),
            last_top: None,
            last_requote: None,
            last_sample: None,
            stopped: false,
        }
    }

    /// Set lots per quote unit and rescale the limits to match.
    pub fn with_unit_lots(mut self, unit_lots: i64) -> Self {
        self.unit_lots = unit_lots;
        self.limits.max_position = self.strategy.max_inventory * unit_lots;
        self.limits.max_order_qty = self.strategy.levels.units * unit_lots;
        self
    }

    fn top(ctx: &Ctx<'_>, i: InstrumentId) -> Option<Top> {
        let book = ctx.book(i)?;
        let (bid, bid_qty) = book.best_bid()?;
        let (ask, ask_qty) = book.best_ask()?;
        Some(Top { bid, bid_qty, ask, ask_qty })
    }

    /// Called before each decision: credits the time since the previous sample to "two-sided" if
    /// the agent had a live order (acknowledged or in flight, not being cancelled) on each side.
    fn sample_uptime(&mut self, ctx: &Ctx<'_>) {
        let now = ctx.now();
        let i = self.instrument;
        let live = |s: Side| ctx.orders().live().any(|(_, o)| o.instrument == i && o.side == s && !o.cancel_pending);
        let both = live(Side::Buy) && live(Side::Sell);
        if let Some((t, was_both)) = self.last_sample {
            self.stats.observed_ns += now - t;
            if was_both {
                self.stats.two_sided_ns += now - t;
            }
        }
        self.last_sample = Some((now, both));
    }

    fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if !self.stopped {
            self.stopped = true;
            crate::common::cancel_all(ctx, self.instrument);
        }
    }

    /// Decide and send. Public so tests and custom drivers can force a decision.
    pub fn requote(&mut self, ctx: &mut Ctx<'_>) {
        let i = self.instrument;
        let now = ctx.now();
        self.last_requote = Some(now);
        if self.stopped {
            return;
        }
        self.sample_uptime(ctx);
        if self.stop_at.is_some_and(|s| now > s) {
            self.stop(ctx);
            return;
        }
        if !self.kill.check(ctx, &[i]) {
            self.stopped = true;
            return;
        }
        let (Some(top), Some(spec)) = (self.last_top, ctx.spec(i).copied()) else { return };
        let unit = self.unit_lots * spec.lot_size;
        let tick = spec.tick_size;
        let position = ctx.orders().position(i);
        let snapshot = Snapshot {
            now,
            tick,
            top,
            inventory: position as f64 / unit as f64,
            micro_price: self.micro.price_ticks(&top, tick),
            sigma: self.vol.sigma(),
            intensity: self.intensity.fit(),
            ofi: self.ofi.normalized(),
            markout: self.markouts.recent(),
        };
        let levels = self.strategy.quotes(&snapshot);
        self.reconcile(ctx, &levels, unit, tick);
        self.stats.requotes += 1;
    }

    fn reconcile(&mut self, ctx: &mut Ctx<'_>, levels: &[Level], unit: Qty, tick: Price) {
        let i = self.instrument;
        let mut wanted: Vec<(Side, Price, Qty)> = levels.iter().map(|l| (l.side, l.price, l.units * unit)).collect();
        let live: Vec<(ClientOrderId, Side, Price, Qty, Qty, Option<OrderId>)> = ctx
            .orders()
            .live()
            .filter(|(_, o)| o.instrument == i && !o.cancel_pending)
            .filter_map(|(c, o)| Some((c, o.side, o.price?, o.leaves, o.filled, o.order_id)))
            .collect();
        let mut requests = Vec::new();
        for (cid, side, price, leaves, filled, order_id) in live {
            if let Some(w) = wanted.iter_mut().find(|w| w.0 == side && w.1 == price && w.2 > 0) {
                if leaves > w.2 {
                    // Larger than wanted: cut it down (keeps time priority).
                    requests.push(Request::Modify { instrument: i, client_id: cid, price, qty: filled + w.2 });
                    self.stats.modifies_sent += 1;
                    w.2 = 0;
                } else {
                    w.2 -= leaves;
                }
                continue;
            }
            let nearest = wanted.iter_mut().filter(|w| w.0 == side && w.2 >= leaves).min_by_key(|w| (w.1 - price).abs());
            if let Some(w) = nearest {
                let book = ctx.book(i);
                let ahead = order_id.and_then(|id| book.and_then(|b| b.queue_ahead(id)));
                let level_qty = book.map_or(0, |b| b.depth(side, 64).iter().find(|l| l.price == price).map_or(0, |l| l.qty));
                if keep_resting(self.strategy.queue, price, w.1, tick, ahead, level_qty) {
                    w.2 -= leaves;
                    continue;
                }
            }
            requests.push(Request::Cancel { instrument: i, client_id: cid });
            self.stats.cancels_sent += 1;
        }
        for r in requests {
            ctx.send(r);
        }
        let lot = ctx.spec(i).map_or(1, |s| s.lot_size);
        for (side, price, qty) in wanted {
            let qty = qty.min(self.limits.room(ctx, i, side, price)) / lot * lot;
            if qty <= 0 {
                continue;
            }
            let cid = ctx.next_client_id();
            let mut spec = OrderSpec::limit(i, cid, side, price, qty).with_tif(TimeInForce::Gtc).with_stp(self.stp);
            if self.post_only {
                spec = spec.with_post_only();
            }
            ctx.send(Request::New(spec));
            self.stats.orders_sent += 1;
        }
    }
}

impl Agent for MarketMaker {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        let at = ctx.now() + self.heartbeat_ns.max(1);
        ctx.set_timer(at, 0);
    }

    fn on_market_data(&mut self, ctx: &mut Ctx<'_>, instrument: InstrumentId, events: &[lob::Event]) {
        if instrument != self.instrument {
            return;
        }
        let now = ctx.now();
        let Some(tick) = ctx.spec(instrument).map(|s| s.tick_size) else { return };
        // Aggressive orders in this packet, each at its deepest execution relative to the mid
        // before the packet.
        self.intensity.advance(now);
        if let Some(prev) = self.last_top {
            let mid = prev.mid();
            let mut current: Option<(OrderId, f64)> = None;
            for e in events {
                if let EventKind::Trade { price, aggressor: Some(side), buy_order_id, sell_order_id, .. } = e.kind {
                    let taker = if side == Side::Buy { buy_order_id } else { sell_order_id };
                    let depth = (price as f64 - mid).abs() / tick as f64;
                    current = match current {
                        Some((t, d)) if t == taker => Some((t, d.max(depth))),
                        Some((_, d)) => {
                            self.intensity.record(d);
                            Some((taker, depth))
                        }
                        None => Some((taker, depth)),
                    };
                }
            }
            if let Some((_, d)) = current {
                self.intensity.record(d);
            }
        }
        let Some(top) = Self::top(ctx, instrument) else { return };
        let mid_ticks = top.mid() / tick as f64;
        self.vol.update(now, mid_ticks);
        if self.last_top != Some(top) {
            self.ofi.update(now, top);
            self.micro.observe(&top, tick);
        }
        self.markouts.update(now, mid_ticks);
        self.last_top = Some(top);
        if self.last_requote.is_none_or(|t| now - t >= self.min_requote_ns) {
            self.requote(ctx);
        }
    }

    fn on_report(&mut self, ctx: &mut Ctx<'_>, report: &ExecReport) {
        if report.instrument != self.instrument {
            return;
        }
        match report.kind {
            ReportKind::Fill { side, price, qty, .. } => {
                let now = ctx.now();
                let tick = ctx.spec(self.instrument).map_or(1, |s| s.tick_size);
                let unit = self.unit_lots * ctx.spec(self.instrument).map_or(1, |s| s.lot_size);
                self.stats.fills += 1;
                match side {
                    Side::Buy => self.stats.bought += qty,
                    Side::Sell => self.stats.sold += qty,
                }
                let mid = self.last_top.map_or(price as f64, |t| t.mid());
                let edge = match side {
                    Side::Buy => mid - price as f64,
                    Side::Sell => price as f64 - mid,
                };
                self.stats.spread_capture += edge * qty as f64;
                self.markouts.record_fill(now, side, price as f64 / tick as f64, qty as f64 / unit as f64);
                let position = ctx.orders().position(self.instrument);
                self.stats.max_abs_position = self.stats.max_abs_position.max(position.abs());
                self.requote(ctx);
            }
            ReportKind::Rejected { reason: RejectReason::PostOnlyWouldCross } => self.stats.post_only_rejects += 1,
            _ => {}
        }
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _token: u64) {
        if let Some(top) = self.last_top {
            let tick = ctx.spec(self.instrument).map_or(1, |s| s.tick_size);
            self.markouts.update(ctx.now(), top.mid() / tick as f64);
        }
        self.requote(ctx);
        if !self.stopped {
            let at = ctx.now() + self.heartbeat_ns.max(1);
            ctx.set_timer(at, 0);
        }
    }
}
