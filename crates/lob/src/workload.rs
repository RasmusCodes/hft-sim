//! Command-stream generators.
//!
//! * [`FuzzGen`]: adversarial streams for differential testing. Narrow price band (many
//!   crosses), few owners (many self-trade situations), every order type, invalid fields,
//!   phase changes (including invalid ones) and occasional backwards timestamps.
//! * [`FlowGen`]: a crude zero-intelligence order flow for benchmarks. Passive orders
//!   cluster near the touch, most orders are cancelled, and a few percent of messages
//!   take liquidity. It drives a shadow engine so cancels and modifies target live orders.
//!   It is a load generator, not a market model: nothing about price formation should be
//!   inferred from it.
//!
//! Both are deterministic functions of their seed.

use crate::book::IdMap;
use crate::engine::MatchingEngine;
use crate::event::{Event, EventKind};
use crate::instrument::InstrumentSpec;
use crate::rng::Rng;
use crate::types::{
    Command, Liquidity, NewOrder, OrderId, OrderSnapshot, OwnerId, Phase, Price, Side, StpMode,
    TimeInForce, Timestamp,
};

/// Parameters of [`FuzzGen`].
#[derive(Clone, Debug)]
pub struct FuzzConfig {
    /// Number of participants (small, to provoke self-trade prevention).
    pub owners: u32,
    /// Half-width of the price band around the drifting centre, in ticks.
    pub band_ticks: i64,
    /// Largest order size, in lots.
    pub max_lots: i64,
    /// Relative weight of new orders.
    pub w_new: u64,
    /// Relative weight of cancels.
    pub w_cancel: u64,
    /// Relative weight of modifies.
    pub w_modify: u64,
    /// Relative weight of mass cancels.
    pub w_cancel_all: u64,
    /// Relative weight of phase changes.
    pub w_phase: u64,
    /// Probability that a new order or modify carries an invalid field.
    pub p_invalid: f64,
    /// Probability of a timestamp earlier than the previous one.
    pub p_backwards_ts: f64,
}

impl Default for FuzzConfig {
    fn default() -> Self {
        FuzzConfig {
            owners: 3,
            band_ticks: 8,
            max_lots: 6,
            w_new: 56,
            w_cancel: 17,
            w_modify: 18,
            w_cancel_all: 1,
            w_phase: 4,
            p_invalid: 0.03,
            p_backwards_ts: 0.005,
        }
    }
}

/// Adversarial command generator for differential testing.
pub struct FuzzGen {
    rng: Rng,
    spec: InstrumentSpec,
    cfg: FuzzConfig,
    ts: Timestamp,
    center: Price,
    issued: u64,
}

impl FuzzGen {
    /// Generator for `spec` seeded with `seed`.
    pub fn new(seed: u64, spec: InstrumentSpec, cfg: FuzzConfig) -> Self {
        FuzzGen { rng: Rng::new(seed), spec, cfg, ts: 0, center: spec.reference_price, issued: 0 }
    }

    /// Next `(timestamp, command)`. `live` is the current set of live orders, used to aim
    /// most cancels and modifies at real orders.
    pub fn next(&mut self, live: &[OrderSnapshot]) -> (Timestamp, Command) {
        let ts = if self.rng.chance(self.cfg.p_backwards_ts) {
            self.ts - 1 - self.rng.below(3) as i64
        } else {
            self.ts += self.rng.below(4) as i64;
            self.ts
        };
        if self.rng.chance(0.05) {
            let tick = self.spec.tick_size;
            let lo = self.spec.min_price + self.cfg.band_ticks * tick;
            let hi = self.spec.max_price - self.cfg.band_ticks * tick;
            if lo <= hi {
                self.center = (self.center + tick * self.rng.range_i64(-1, 1)).clamp(lo, hi);
            }
        }
        let (w_new, w_cancel, w_modify, w_cancel_all, w_phase) = (
            self.cfg.w_new,
            self.cfg.w_cancel,
            self.cfg.w_modify,
            self.cfg.w_cancel_all,
            self.cfg.w_phase,
        );
        let mut r = self.rng.below(w_new + w_cancel + w_modify + w_cancel_all + w_phase);
        let cmd = if r < w_new {
            self.new_order()
        } else {
            r -= w_new;
            if r < w_cancel {
                let (owner, order_id, _) = self.target(live);
                Command::Cancel { owner, order_id }
            } else if r < w_cancel + w_modify {
                self.modify(live)
            } else if r < w_cancel + w_modify + w_cancel_all {
                Command::CancelAll { owner: self.random_owner() }
            } else {
                let phase = match self.rng.below(20) {
                    0..=7 => Phase::Continuous,
                    8..=13 => Phase::Auction,
                    14..=16 => Phase::Halted,
                    _ => Phase::Closed,
                };
                Command::SetPhase { phase }
            }
        };
        (ts, cmd)
    }

    fn random_owner(&mut self) -> OwnerId {
        1 + self.rng.below(u64::from(self.cfg.owners)) as OwnerId
    }

    fn random_price(&mut self) -> Price {
        self.center + self.spec.tick_size * self.rng.range_i64(-self.cfg.band_ticks, self.cfg.band_ticks)
    }

    fn new_order(&mut self) -> Command {
        let (tick, lot) = (self.spec.tick_size, self.spec.lot_size);
        let side = if self.rng.chance(0.5) { Side::Buy } else { Side::Sell };
        let mut price = self.random_price();
        let mut qty = lot * self.rng.range_i64(1, self.cfg.max_lots);
        if self.rng.chance(self.cfg.p_invalid) {
            match self.rng.below(6) {
                0 => qty = 0,
                1 => qty = -lot,
                2 => qty += 1,
                3 => qty = self.spec.max_order_qty + lot,
                4 => price += 1,
                _ => price = self.spec.max_price + tick,
            }
        }
        let tif = match self.rng.below(20) {
            0..=13 => TimeInForce::Gtc,
            14..=16 => TimeInForce::Ioc,
            _ => TimeInForce::Fok,
        };
        let post_only = self.rng.chance(0.1);
        let stp = match self.rng.below(5) {
            0 | 1 => StpMode::None,
            2 => StpMode::CancelResting,
            3 => StpMode::CancelAggressor,
            _ => StpMode::CancelBoth,
        };
        let owner = self.random_owner();
        let market = self.rng.chance(0.1);
        self.issued += 1;
        Command::New(NewOrder {
            owner,
            client_tag: self.issued,
            side,
            price: if market { None } else { Some(price) },
            qty,
            tif,
            post_only,
            stp,
        })
    }

    /// Pick a cancel/modify target: usually a live order (usually by its owner),
    /// sometimes an arbitrary id that may be dead or never issued.
    fn target(&mut self, live: &[OrderSnapshot]) -> (OwnerId, OrderId, Option<OrderSnapshot>) {
        if !live.is_empty() && self.rng.chance(0.85) {
            let o = live[self.rng.index(live.len())];
            let owner = if self.rng.chance(0.9) { o.owner } else { self.random_owner() };
            (owner, o.order_id, Some(o))
        } else {
            let owner = self.random_owner();
            (owner, 1 + self.rng.below(self.issued + 3), None)
        }
    }

    fn modify(&mut self, live: &[OrderSnapshot]) -> Command {
        let (owner, order_id, target) = self.target(live);
        let lot = self.spec.lot_size;
        let max_lots = self.cfg.max_lots;
        let random_price = self.random_price();
        let (mut price, mut qty) = match target {
            None => (random_price, lot * self.rng.range_i64(1, max_lots)),
            Some(o) => {
                let p = o.price.unwrap_or(random_price);
                match self.rng.below(10) {
                    // size down (priority kept unless it reaches the filled quantity)
                    0..=2 => (p, o.order_qty - lot * self.rng.range_i64(1, 2)),
                    // size up (priority lost)
                    3 | 4 => (p, o.order_qty + lot * self.rng.range_i64(1, 3)),
                    // reprice (priority lost, may cross)
                    5..=8 => (random_price, o.cum_qty + lot * self.rng.range_i64(1, max_lots)),
                    // at or below the filled quantity
                    _ => (p, o.cum_qty - lot * self.rng.range_i64(0, 1)),
                }
            }
        };
        if self.rng.chance(self.cfg.p_invalid) {
            if self.rng.chance(0.5) {
                price += 1;
            } else {
                qty += 1;
            }
        }
        Command::Modify { owner, order_id, price, qty }
    }
}

/// Parameters of [`FlowGen`].
#[derive(Clone, Debug)]
pub struct FlowConfig {
    /// Number of live orders around which the book size mean-reverts.
    pub target_live: usize,
    /// Number of participants.
    pub owners: u32,
    /// Cancel probability per message when the book is at `target_live`.
    pub p_cancel: f64,
    /// Probability of a modify (70% size-down keeping priority, 30% reprice by one tick).
    pub p_modify: f64,
    /// Probability of a liquidity-taking order.
    pub p_aggressive: f64,
    /// Share of aggressive orders sent as market orders (the rest are IOC limits).
    pub p_market: f64,
    /// Probability that a passive order improves the touch by one tick.
    pub p_improve: f64,
    /// Success probability of the geometric distance (ticks) of passive orders behind the touch.
    pub depth_decay: f64,
    /// Success probability of the geometric number of extra lots per order.
    pub size_decay: f64,
}

impl Default for FlowConfig {
    fn default() -> Self {
        FlowConfig {
            target_live: 2_000,
            owners: 64,
            p_cancel: 0.45,
            p_modify: 0.05,
            p_aggressive: 0.05,
            p_market: 0.2,
            p_improve: 0.05,
            depth_decay: 0.25,
            size_decay: 0.35,
        }
    }
}

/// Zero-intelligence order-flow generator (load generator for benchmarks).
pub struct FlowGen {
    rng: Rng,
    spec: InstrumentSpec,
    cfg: FlowConfig,
    shadow: MatchingEngine,
    live: Vec<OrderId>,
    pos: IdMap<usize>,
    ts: Timestamp,
    events: Vec<Event>,
    tag: u64,
}

impl FlowGen {
    /// Generator for `spec` seeded with `seed`, starting from an empty continuous book.
    pub fn new(seed: u64, spec: InstrumentSpec, cfg: FlowConfig) -> Self {
        let shadow = MatchingEngine::with_capacity(spec, Phase::Continuous, cfg.target_live * 2)
            .expect("valid instrument spec");
        FlowGen {
            rng: Rng::new(seed),
            spec,
            cfg,
            shadow,
            live: Vec::new(),
            pos: IdMap::default(),
            ts: 0,
            events: Vec::new(),
            tag: 0,
        }
    }

    /// `n` commands from a fresh generator.
    pub fn generate(seed: u64, spec: InstrumentSpec, cfg: FlowConfig, n: usize) -> Vec<(Timestamp, Command)> {
        let mut g = FlowGen::new(seed, spec, cfg);
        (0..n).map(|_| g.step()).collect()
    }

    /// Number of live orders in the shadow book.
    pub fn live_orders(&self) -> usize {
        self.live.len()
    }

    /// Next `(timestamp, command)`.
    pub fn step(&mut self) -> (Timestamp, Command) {
        self.ts += 1 + self.rng.below(1_000) as i64;
        let fullness = self.live.len() as f64 / self.cfg.target_live.max(1) as f64;
        let p_cancel = (self.cfg.p_cancel * fullness).min(0.95);
        let u = self.rng.f64();
        let cmd = if !self.live.is_empty() && u < p_cancel {
            self.cancel()
        } else if !self.live.is_empty() && u < p_cancel + self.cfg.p_modify {
            self.modify()
        } else if u < p_cancel + self.cfg.p_modify + self.cfg.p_aggressive {
            self.aggressive()
        } else {
            self.passive()
        };
        self.events.clear();
        self.shadow
            .process(self.ts, &cmd, &mut self.events)
            .expect("generator timestamps increase");
        self.track();
        (self.ts, cmd)
    }

    fn track(&mut self) {
        for i in 0..self.events.len() {
            let kind = self.events[i].kind;
            match kind {
                EventKind::BookAdd { order_id, .. } => {
                    self.pos.insert(order_id, self.live.len());
                    self.live.push(order_id);
                }
                EventKind::BookRemove { order_id } => self.forget(order_id),
                EventKind::Fill { order_id, leaves: 0, liquidity: Liquidity::Maker | Liquidity::Auction, .. } => {
                    self.forget(order_id)
                }
                _ => {}
            }
        }
    }

    fn forget(&mut self, id: OrderId) {
        if let Some(i) = self.pos.remove(&id) {
            let last = self.live.pop().expect("tracked set not empty");
            if i < self.live.len() {
                self.live[i] = last;
                self.pos.insert(last, i);
            }
        }
    }

    fn owner(&mut self) -> OwnerId {
        1 + self.rng.below(u64::from(self.cfg.owners)) as OwnerId
    }

    fn lots(&mut self) -> i64 {
        self.spec.lot_size * (1 + self.rng.geometric(self.cfg.size_decay) as i64)
    }

    fn random_side(&mut self) -> Side {
        if self.rng.chance(0.5) { Side::Buy } else { Side::Sell }
    }

    fn cancel(&mut self) -> Command {
        let id = self.live[self.rng.index(self.live.len())];
        let owner = self.shadow.order(id).expect("tracked order is live").owner;
        Command::Cancel { owner, order_id: id }
    }

    fn modify(&mut self) -> Command {
        let id = self.live[self.rng.index(self.live.len())];
        let o = self.shadow.order(id).expect("tracked order is live");
        let price = o.price.expect("flow orders are limit orders");
        let lot = self.spec.lot_size;
        if o.leaves > lot && self.rng.chance(0.7) {
            Command::Modify { owner: o.owner, order_id: id, price, qty: o.order_qty - lot }
        } else {
            let step = if self.rng.chance(0.5) { self.spec.tick_size } else { -self.spec.tick_size };
            let price = (price + step).clamp(self.spec.min_price, self.spec.max_price);
            Command::Modify { owner: o.owner, order_id: id, price, qty: o.order_qty }
        }
    }

    fn aggressive(&mut self) -> Command {
        let side = self.random_side();
        let qty = self.lots();
        let owner = self.owner();
        self.tag += 1;
        if self.rng.chance(self.cfg.p_market) {
            return Command::New(NewOrder::market(owner, side, qty).with_tag(self.tag));
        }
        let opposite = match side {
            Side::Buy => self.shadow.best_ask(),
            Side::Sell => self.shadow.best_bid(),
        };
        match opposite {
            None => self.passive(),
            Some((p, _)) => {
                let through = self.spec.tick_size * self.rng.below(3) as i64;
                let limit = match side {
                    Side::Buy => p + through,
                    Side::Sell => p - through,
                }
                .clamp(self.spec.min_price, self.spec.max_price);
                Command::New(
                    NewOrder::limit(owner, side, limit, qty).with_tif(TimeInForce::Ioc).with_tag(self.tag),
                )
            }
        }
    }

    fn passive(&mut self) -> Command {
        let tick = self.spec.tick_size;
        let side = self.random_side();
        let bid = self.shadow.best_bid().map(|x| x.0);
        let ask = self.shadow.best_ask().map(|x| x.0);
        let reference = self.shadow.reference_price();
        let behind = self.rng.geometric(self.cfg.depth_decay) as i64 * tick;
        let improve = self.rng.chance(self.cfg.p_improve);
        let price = match side {
            Side::Buy => {
                let anchor = bid.unwrap_or_else(|| ask.map_or(reference, |a| a - tick));
                let p = if improve { anchor + tick } else { anchor - behind };
                ask.map_or(p, |a| p.min(a - tick))
            }
            Side::Sell => {
                let anchor = ask.unwrap_or_else(|| bid.map_or(reference, |b| b + tick));
                let p = if improve { anchor - tick } else { anchor + behind };
                bid.map_or(p, |b| p.max(b + tick))
            }
        }
        .clamp(self.spec.min_price, self.spec.max_price);
        let qty = self.lots();
        let owner = self.owner();
        self.tag += 1;
        Command::New(NewOrder::limit(owner, side, price, qty).with_tag(self.tag))
    }
}
