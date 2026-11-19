//! A deliberately naive implementation of the matching rules, used only as a test oracle.
//!
//! Live orders sit in one unsorted `Vec`; every decision is a linear scan, with priority
//! computed from first principles as (market before limit, better price, earlier entry).
//! Fill-or-kill feasibility is decided by running the real matching walk on a scratch copy
//! of the whole engine. Auction prices are found by scoring every tick in the price range.
//! Nothing is shared with [`crate::MatchingEngine`] except the public value types, so a
//! bug would have to be made twice, independently, to slip through the differential tests.
//!
//! It is orders of magnitude slower than the engine. Never use it for simulation.

use std::cmp::Reverse;

use crate::auction::AuctionQuote;
use crate::engine::EngineError;
use crate::event::{CancelReason, Event, EventKind, RejectReason, RequestKind};
use crate::instrument::{InstrumentSpec, SpecError};
use crate::types::{
    Command, Liquidity, NewOrder, OrderId, OrderSnapshot, OwnerId, Phase, Price, Qty, SeqNo, Side,
    StpMode, TimeInForce, Timestamp, TradeId,
};

#[derive(Clone, Debug)]
struct ROrder {
    id: OrderId,
    owner: OwnerId,
    side: Side,
    price: Option<Price>,
    leaves: Qty,
    order_qty: Qty,
    cum_qty: Qty,
    prio: u64,
    post_only: bool,
    stp: StpMode,
}

/// Priority sort key (smaller = higher priority) within one side.
fn priority_key(o: &ROrder) -> (u8, i64, u64) {
    match o.price {
        None => (0, 0, o.prio),
        Some(p) => (1, if o.side == Side::Buy { -p } else { p }, o.prio),
    }
}

/// Result of a matching walk.
struct Walk {
    filled: Qty,
    left: Qty,
    /// Self-trade prevention cancelled the incoming order's remainder.
    killed: bool,
}

/// Naive reference implementation of the matching rules.
#[derive(Clone, Debug)]
pub struct ReferenceEngine {
    spec: InstrumentSpec,
    phase: Phase,
    orders: Vec<ROrder>,
    next_order_id: OrderId,
    next_trade_id: TradeId,
    next_seq: SeqNo,
    next_prio: u64,
    last_ts: Timestamp,
    now: Timestamp,
    reference_price: Price,
}

impl ReferenceEngine {
    /// Reference engine with an empty book in `phase`.
    pub fn new(spec: InstrumentSpec, phase: Phase) -> Result<Self, SpecError> {
        spec.validate()?;
        Ok(ReferenceEngine {
            spec,
            phase,
            orders: Vec::new(),
            next_order_id: 1,
            next_trade_id: 1,
            next_seq: 1,
            next_prio: 1,
            last_ts: Timestamp::MIN,
            now: Timestamp::MIN,
            reference_price: spec.reference_price,
        })
    }

    /// Same contract as [`crate::MatchingEngine::process`].
    pub fn process(&mut self, ts: Timestamp, cmd: &Command, out: &mut Vec<Event>) -> Result<(), EngineError> {
        if ts < self.last_ts {
            return Err(EngineError::NonMonotonicTime { last: self.last_ts, got: ts });
        }
        if let Command::SetPhase { phase } = *cmd {
            if !self.transition_allowed(phase) {
                return Err(EngineError::InvalidPhaseTransition { from: self.phase, to: phase });
            }
        }
        self.last_ts = ts;
        self.now = ts;
        match *cmd {
            Command::New(o) => self.on_new(&o, out),
            Command::Cancel { owner, order_id } => self.on_cancel(owner, order_id, out),
            Command::Modify { owner, order_id, price, qty } => self.on_modify(owner, order_id, price, qty, out),
            Command::CancelAll { owner } => self.on_cancel_all(owner, out),
            Command::SetPhase { phase } => self.on_set_phase(phase, out),
        }
        Ok(())
    }

    /// Current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Current reference price.
    pub fn reference_price(&self) -> Price {
        self.reference_price
    }

    /// Number of live orders.
    pub fn live_orders(&self) -> usize {
        self.orders.len()
    }

    /// Best bid price and total quantity.
    pub fn best_bid(&self) -> Option<(Price, Qty)> {
        self.best_level(Side::Buy)
    }

    /// Best ask price and total quantity.
    pub fn best_ask(&self) -> Option<(Price, Qty)> {
        self.best_level(Side::Sell)
    }

    fn best_level(&self, side: Side) -> Option<(Price, Qty)> {
        let p = self.best_limit(side)?;
        let qty = self.orders.iter().filter(|o| o.side == side && o.price == Some(p)).map(|o| o.leaves).sum();
        Some((p, qty))
    }

    /// Every live order in priority order (same layout as the engine's snapshot).
    pub fn snapshot(&self) -> Vec<OrderSnapshot> {
        let mut v: Vec<&ROrder> = self.orders.iter().collect();
        v.sort_by_key(|o| (o.side, priority_key(o)));
        v.into_iter()
            .map(|o| OrderSnapshot {
                order_id: o.id,
                owner: o.owner,
                side: o.side,
                price: o.price,
                leaves: o.leaves,
                order_qty: o.order_qty,
                cum_qty: o.cum_qty,
            })
            .collect()
    }

    /// Auction price determination by brute force over every tick (R7).
    pub fn indicative_auction(&self) -> Option<AuctionQuote> {
        let limits: Vec<Price> = self.orders.iter().filter_map(|o| o.price).collect();
        let candidates: Vec<Price> = match (limits.iter().min(), limits.iter().max()) {
            (Some(&lo), Some(&hi)) => (lo..=hi).step_by(self.spec.tick_size as usize).collect(),
            _ => vec![self.reference_price],
        };
        let rows: Vec<(Price, Qty, Qty)> = candidates
            .iter()
            .map(|&p| {
                let buy: Qty = self
                    .orders
                    .iter()
                    .filter(|o| o.side == Side::Buy && o.price.is_none_or(|lp| lp >= p))
                    .map(|o| o.leaves)
                    .sum();
                let sell: Qty = self
                    .orders
                    .iter()
                    .filter(|o| o.side == Side::Sell && o.price.is_none_or(|lp| lp <= p))
                    .map(|o| o.leaves)
                    .sum();
                (p, buy, sell)
            })
            .collect();
        let vmax = rows.iter().map(|&(_, b, s)| b.min(s)).max()?;
        if vmax <= 0 {
            return None;
        }
        let max_volume: Vec<&(Price, Qty, Qty)> = rows.iter().filter(|&&(_, b, s)| b.min(s) == vmax).collect();
        let smin = max_volume.iter().map(|&&(_, b, s)| (b - s).abs()).min()?;
        let best: Vec<&(Price, Qty, Qty)> =
            max_volume.into_iter().filter(|&&(_, b, s)| (b - s).abs() == smin).collect();
        let pick = if best.iter().all(|&&(_, b, s)| b > s) {
            best.iter().max_by_key(|r| r.0)
        } else if best.iter().all(|&&(_, b, s)| b < s) {
            best.iter().min_by_key(|r| r.0)
        } else {
            let reference = self.reference_price;
            best.iter().min_by_key(|r| ((r.0 - reference).abs(), Reverse(r.0)))
        }?;
        Some(AuctionQuote { price: pick.0, volume: vmax, imbalance: pick.1 - pick.2 })
    }

    // ------------------------------------------------------------------------------------

    fn emit(&mut self, out: &mut Vec<Event>, kind: EventKind) {
        out.push(Event { seq: self.next_seq, ts: self.now, kind });
        self.next_seq += 1;
    }

    fn position(&self, id: OrderId) -> Option<usize> {
        self.orders.iter().position(|o| o.id == id)
    }

    fn best_limit(&self, side: Side) -> Option<Price> {
        let prices = self.orders.iter().filter(|o| o.side == side).filter_map(|o| o.price);
        match side {
            Side::Buy => prices.max(),
            Side::Sell => prices.min(),
        }
    }

    fn would_cross(&self, side: Side, price: Price) -> bool {
        match side {
            Side::Buy => self.best_limit(Side::Sell).is_some_and(|a| price >= a),
            Side::Sell => self.best_limit(Side::Buy).is_some_and(|b| price <= b),
        }
    }

    fn qty_problem(&self, qty: Qty) -> Option<RejectReason> {
        if qty <= 0 {
            Some(RejectReason::InvalidQty)
        } else if qty % self.spec.lot_size != 0 {
            Some(RejectReason::LotSize)
        } else if qty > self.spec.max_order_qty {
            Some(RejectReason::MaxQty)
        } else {
            None
        }
    }

    fn price_problem(&self, price: Price) -> Option<RejectReason> {
        if price % self.spec.tick_size != 0 {
            Some(RejectReason::TickSize)
        } else if price < self.spec.min_price || price > self.spec.max_price {
            Some(RejectReason::PriceBounds)
        } else {
            None
        }
    }

    fn new_order_problem(&self, o: &NewOrder) -> Option<RejectReason> {
        if self.phase == Phase::Closed {
            return Some(RejectReason::MarketClosed);
        }
        if self.phase == Phase::Halted {
            return Some(RejectReason::TradingHalted);
        }
        if let Some(r) = self.qty_problem(o.qty) {
            return Some(r);
        }
        if let Some(r) = o.price.and_then(|p| self.price_problem(p)) {
            return Some(r);
        }
        if o.post_only && (o.price.is_none() || o.tif != TimeInForce::Gtc) {
            return Some(RejectReason::InvalidPostOnly);
        }
        if self.phase == Phase::Auction && o.tif != TimeInForce::Gtc {
            return Some(RejectReason::TifNotAllowedInPhase);
        }
        if self.phase == Phase::Auction && o.post_only {
            return Some(RejectReason::InvalidPostOnly);
        }
        if o.post_only && self.would_cross(o.side, o.price.expect("post-only orders are limit orders")) {
            return Some(RejectReason::PostOnlyWouldCross);
        }
        None
    }

    fn add_resting(&mut self, mut o: ROrder, out: &mut Vec<Event>) {
        o.prio = self.next_prio;
        self.next_prio += 1;
        self.emit(out, EventKind::BookAdd { order_id: o.id, side: o.side, price: o.price, qty: o.leaves });
        self.orders.push(o);
    }

    fn on_new(&mut self, o: &NewOrder, out: &mut Vec<Event>) {
        let id = self.next_order_id;
        self.next_order_id += 1;
        if let Some(reason) = self.new_order_problem(o) {
            self.emit(out, EventKind::Rejected { order_id: id, owner: o.owner, client_tag: o.client_tag, reason });
            return;
        }
        self.emit(
            out,
            EventKind::Accepted {
                order_id: id,
                owner: o.owner,
                client_tag: o.client_tag,
                side: o.side,
                price: o.price,
                qty: o.qty,
                tif: o.tif,
            },
        );
        let resting = ROrder {
            id,
            owner: o.owner,
            side: o.side,
            price: o.price,
            leaves: o.qty,
            order_qty: o.qty,
            cum_qty: 0,
            prio: 0,
            post_only: o.post_only,
            stp: o.stp,
        };
        if self.phase == Phase::Auction {
            self.add_resting(resting, out);
            return;
        }
        if o.tif == TimeInForce::Fok {
            // Fill-or-kill: try it on a throwaway copy of the whole exchange.
            let mut scratch = self.clone();
            let trial = scratch.walk(id, o.owner, o.side, o.price, o.qty, o.stp, &mut Vec::new());
            if trial.filled < o.qty {
                self.emit(
                    out,
                    EventKind::Cancelled { order_id: id, owner: o.owner, qty: o.qty, reason: CancelReason::FokUnfilled },
                );
                return;
            }
        }
        let w = self.walk(id, o.owner, o.side, o.price, o.qty, o.stp, out);
        if w.killed || w.left == 0 {
            return;
        }
        if o.price.is_some() && o.tif == TimeInForce::Gtc {
            self.add_resting(ROrder { leaves: w.left, cum_qty: w.filled, ..resting }, out);
        } else {
            self.emit(
                out,
                EventKind::Cancelled { order_id: id, owner: o.owner, qty: w.left, reason: CancelReason::IocRemainder },
            );
        }
    }

    /// Match an incoming order against resting orders of the other side.
    #[allow(clippy::too_many_arguments)]
    fn walk(
        &mut self,
        id: OrderId,
        owner: OwnerId,
        side: Side,
        limit: Option<Price>,
        qty: Qty,
        stp: StpMode,
        out: &mut Vec<Event>,
    ) -> Walk {
        let opp = side.opposite();
        let mut left = qty;
        while left > 0 {
            let best = self
                .orders
                .iter()
                .enumerate()
                .filter(|(_, r)| r.side == opp && r.price.is_some())
                .min_by_key(|(_, r)| priority_key(r))
                .map(|(i, _)| i);
            let Some(i) = best else { break };
            let price = self.orders[i].price.expect("limit order");
            let marketable = match (side, limit) {
                (_, None) => true,
                (Side::Buy, Some(l)) => price <= l,
                (Side::Sell, Some(l)) => price >= l,
            };
            if !marketable {
                break;
            }
            if stp != StpMode::None && self.orders[i].owner == owner {
                if matches!(stp, StpMode::CancelResting | StpMode::CancelBoth) {
                    let r = self.orders.remove(i);
                    self.emit(
                        out,
                        EventKind::Cancelled { order_id: r.id, owner: r.owner, qty: r.leaves, reason: CancelReason::SelfTrade },
                    );
                    self.emit(out, EventKind::BookRemove { order_id: r.id });
                }
                if matches!(stp, StpMode::CancelAggressor | StpMode::CancelBoth) {
                    self.emit(
                        out,
                        EventKind::Cancelled { order_id: id, owner, qty: left, reason: CancelReason::SelfTrade },
                    );
                    return Walk { filled: qty - left, left, killed: true };
                }
                continue;
            }
            let q = left.min(self.orders[i].leaves);
            let trade_id = self.next_trade_id;
            self.next_trade_id += 1;
            let maker_id = self.orders[i].id;
            let (buy_order_id, sell_order_id) = if side == Side::Buy { (id, maker_id) } else { (maker_id, id) };
            self.emit(
                out,
                EventKind::Trade { trade_id, price, qty: q, aggressor: Some(side), buy_order_id, sell_order_id },
            );
            let maker = &mut self.orders[i];
            maker.leaves -= q;
            maker.cum_qty += q;
            let (maker_owner, maker_left) = (maker.owner, maker.leaves);
            self.emit(
                out,
                EventKind::Fill {
                    order_id: maker_id,
                    owner: maker_owner,
                    trade_id,
                    price,
                    qty: q,
                    leaves: maker_left,
                    liquidity: Liquidity::Maker,
                },
            );
            left -= q;
            self.emit(
                out,
                EventKind::Fill { order_id: id, owner, trade_id, price, qty: q, leaves: left, liquidity: Liquidity::Taker },
            );
            self.reference_price = price;
            if maker_left == 0 {
                self.orders.remove(i);
            }
        }
        Walk { filled: qty - left, left, killed: false }
    }

    fn on_cancel(&mut self, owner: OwnerId, id: OrderId, out: &mut Vec<Event>) {
        let reason = match self.position(id) {
            None => Some(RejectReason::UnknownOrder),
            Some(i) if self.orders[i].owner != owner => Some(RejectReason::NotOwner),
            Some(i) => {
                let r = self.orders.remove(i);
                self.emit(out, EventKind::Cancelled { order_id: id, owner, qty: r.leaves, reason: CancelReason::Requested });
                self.emit(out, EventKind::BookRemove { order_id: id });
                None
            }
        };
        if let Some(reason) = reason {
            self.emit(out, EventKind::RequestRejected { order_id: id, owner, request: RequestKind::Cancel, reason });
        }
    }

    fn modify_problem(&self, owner: OwnerId, id: OrderId, price: Price, qty: Qty) -> Option<RejectReason> {
        if self.phase == Phase::Closed {
            return Some(RejectReason::MarketClosed);
        }
        if self.phase == Phase::Halted {
            return Some(RejectReason::TradingHalted);
        }
        let Some(i) = self.position(id) else { return Some(RejectReason::UnknownOrder) };
        if self.orders[i].owner != owner {
            return Some(RejectReason::NotOwner);
        }
        if self.orders[i].price.is_none() {
            return Some(RejectReason::MarketOrderNotModifiable);
        }
        self.qty_problem(qty).or_else(|| self.price_problem(price))
    }

    fn on_modify(&mut self, owner: OwnerId, id: OrderId, price: Price, qty: Qty, out: &mut Vec<Event>) {
        if let Some(reason) = self.modify_problem(owner, id, price, qty) {
            self.emit(out, EventKind::RequestRejected { order_id: id, owner, request: RequestKind::Modify, reason });
            return;
        }
        let i = self.position(id).expect("checked above");
        let current = self.orders[i].clone();
        if qty <= current.cum_qty {
            self.orders.remove(i);
            self.emit(
                out,
                EventKind::Cancelled { order_id: id, owner, qty: current.leaves, reason: CancelReason::ModifiedBelowFilled },
            );
            self.emit(out, EventKind::BookRemove { order_id: id });
            return;
        }
        let new_leaves = qty - current.cum_qty;
        if current.post_only && self.phase == Phase::Continuous && self.would_cross(current.side, price) {
            self.emit(
                out,
                EventKind::RequestRejected {
                    order_id: id,
                    owner,
                    request: RequestKind::Modify,
                    reason: RejectReason::PostOnlyWouldCross,
                },
            );
            return;
        }
        if current.price == Some(price) && new_leaves <= current.leaves {
            let r = &mut self.orders[i];
            r.leaves = new_leaves;
            r.order_qty = qty;
            self.emit(out, EventKind::Replaced { order_id: id, owner, price, qty: new_leaves, priority_kept: true });
            self.emit(out, EventKind::BookUpdate { order_id: id, qty: new_leaves });
            return;
        }
        self.emit(out, EventKind::Replaced { order_id: id, owner, price, qty: new_leaves, priority_kept: false });
        self.emit(out, EventKind::BookRemove { order_id: id });
        let mut r = self.orders.remove(i);
        r.price = Some(price);
        r.order_qty = qty;
        r.leaves = new_leaves;
        if self.phase == Phase::Continuous {
            let w = self.walk(id, r.owner, r.side, Some(price), new_leaves, r.stp, out);
            if w.killed {
                return;
            }
            r.cum_qty += w.filled;
            r.leaves = w.left;
        }
        if r.leaves > 0 {
            self.add_resting(r, out);
        }
    }

    fn on_cancel_all(&mut self, owner: OwnerId, out: &mut Vec<Event>) {
        let mut ids: Vec<OrderId> = self.orders.iter().filter(|o| o.owner == owner).map(|o| o.id).collect();
        ids.sort_unstable();
        for id in ids {
            let i = self.position(id).expect("live");
            let r = self.orders.remove(i);
            self.emit(out, EventKind::Cancelled { order_id: id, owner, qty: r.leaves, reason: CancelReason::MassCancel });
            self.emit(out, EventKind::BookRemove { order_id: id });
        }
    }

    /// Phase transition table, written out explicitly (R8).
    fn transition_allowed(&self, to: Phase) -> bool {
        let clean = !self.orders.iter().any(|o| o.price.is_none())
            && !matches!((self.best_limit(Side::Buy), self.best_limit(Side::Sell)), (Some(b), Some(a)) if b >= a);
        use Phase::*;
        match (self.phase, to) {
            (Continuous, Auction | Halted | Closed) => true,
            (Auction, Continuous | Halted | Closed) => true,
            (Halted, Auction) => true,
            (Halted, Continuous | Closed) => clean,
            (Closed, Auction | Halted) => true,
            (Closed, Continuous) => clean,
            _ => false,
        }
    }

    fn on_set_phase(&mut self, to: Phase, out: &mut Vec<Event>) {
        if self.phase == Phase::Auction && matches!(to, Phase::Continuous | Phase::Closed) {
            self.uncross(out);
        }
        self.phase = to;
        self.emit(out, EventKind::PhaseChanged { phase: to });
    }

    fn uncross(&mut self, out: &mut Vec<Event>) {
        match self.indicative_auction() {
            None => self.emit(out, EventKind::AuctionUncross { price: None, volume: 0 }),
            Some(q) => {
                self.emit(out, EventKind::AuctionUncross { price: Some(q.price), volume: q.volume });
                let eligible = |side: Side| -> Vec<OrderId> {
                    let mut v: Vec<&ROrder> = self
                        .orders
                        .iter()
                        .filter(|o| {
                            o.side == side
                                && match (side, o.price) {
                                    (_, None) => true,
                                    (Side::Buy, Some(p)) => p >= q.price,
                                    (Side::Sell, Some(p)) => p <= q.price,
                                }
                        })
                        .collect();
                    v.sort_by_key(|o| priority_key(o));
                    v.into_iter().map(|o| o.id).collect()
                };
                let buys = eligible(Side::Buy);
                let sells = eligible(Side::Sell);
                let (mut bi, mut si, mut remaining) = (0, 0, q.volume);
                while remaining > 0 {
                    let b = self.position(buys[bi]).expect("live");
                    let s = self.position(sells[si]).expect("live");
                    let x = self.orders[b].leaves.min(self.orders[s].leaves).min(remaining);
                    let trade_id = self.next_trade_id;
                    self.next_trade_id += 1;
                    self.emit(
                        out,
                        EventKind::Trade {
                            trade_id,
                            price: q.price,
                            qty: x,
                            aggressor: None,
                            buy_order_id: buys[bi],
                            sell_order_id: sells[si],
                        },
                    );
                    for k in [b, s] {
                        let o = &mut self.orders[k];
                        o.leaves -= x;
                        o.cum_qty += x;
                        let (order_id, owner, leaves) = (o.id, o.owner, o.leaves);
                        self.emit(
                            out,
                            EventKind::Fill {
                                order_id,
                                owner,
                                trade_id,
                                price: q.price,
                                qty: x,
                                leaves,
                                liquidity: Liquidity::Auction,
                            },
                        );
                    }
                    remaining -= x;
                    if self.orders[b].leaves == 0 {
                        bi += 1;
                    }
                    if self.orders[s].leaves == 0 {
                        si += 1;
                    }
                }
                self.orders.retain(|o| o.leaves > 0);
                self.reference_price = q.price;
            }
        }
        for side in [Side::Buy, Side::Sell] {
            let mut market: Vec<(u64, OrderId)> =
                self.orders.iter().filter(|o| o.side == side && o.price.is_none()).map(|o| (o.prio, o.id)).collect();
            market.sort_unstable();
            for (_, id) in market {
                let i = self.position(id).expect("live");
                let r = self.orders.remove(i);
                self.emit(
                    out,
                    EventKind::Cancelled {
                        order_id: id,
                        owner: r.owner,
                        qty: r.leaves,
                        reason: CancelReason::AuctionMarketRemainder,
                    },
                );
                self.emit(out, EventKind::BookRemove { order_id: id });
            }
        }
    }
}
