//! The matching engine: a deterministic state machine `(timestamp, Command) -> events`.
//!
//! Rule numbers (R1, R2, ...) in comments refer to `docs/MATCHING_RULES.md`.

use crate::auction::{self, AuctionQuote};
use crate::book::{Book, NIL, Node};
use crate::event::{CancelReason, Event, EventKind, RejectReason, RequestKind};
use crate::instrument::{InstrumentSpec, SpecError};
use crate::types::{
    Command, LevelSnapshot, Liquidity, NewOrder, OrderId, OrderSnapshot, OwnerId, Phase, Price,
    Qty, SeqNo, Side, StpMode, TimeInForce, Timestamp, TradeId,
};

/// A command refused as a caller error. No event is emitted and no state changes.
/// (Business-level refusals, such as an invalid price, are events instead.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EngineError {
    /// Timestamps must be non-decreasing.
    NonMonotonicTime {
        /// Last processed timestamp.
        last: Timestamp,
        /// Offending timestamp.
        got: Timestamp,
    },
    /// The phase transition is not allowed from the current state (R8).
    InvalidPhaseTransition {
        /// Current phase.
        from: Phase,
        /// Requested phase.
        to: Phase,
    },
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::NonMonotonicTime { last, got } => {
                write!(f, "timestamp {got} precedes the last processed timestamp {last}")
            }
            EngineError::InvalidPhaseTransition { from, to } => {
                write!(f, "phase transition {from:?} -> {to:?} is not allowed in this state")
            }
        }
    }
}

impl std::error::Error for EngineError {}

/// State of an incoming (aggressive) order during a matching walk.
struct Taker {
    order_id: OrderId,
    owner: OwnerId,
    side: Side,
    limit: Option<Price>,
    leaves: Qty,
    filled: Qty,
    stp: StpMode,
}

/// Whether an incoming order on `side` with limit `limit` can trade with a resting
/// order at `resting`.
#[inline]
fn crosses(side: Side, limit: Price, resting: Price) -> bool {
    match side {
        Side::Buy => resting <= limit,
        Side::Sell => resting >= limit,
    }
}

/// Price-time-priority matching engine for one instrument.
pub struct MatchingEngine {
    spec: InstrumentSpec,
    phase: Phase,
    book: Book,
    next_order_id: OrderId,
    next_trade_id: TradeId,
    next_seq: SeqNo,
    last_ts: Timestamp,
    now: Timestamp,
    reference_price: Price,
}

impl MatchingEngine {
    /// Engine with an empty book in `phase`.
    pub fn new(spec: InstrumentSpec, phase: Phase) -> Result<Self, SpecError> {
        Self::with_capacity(spec, phase, 1 << 12)
    }

    /// Engine with room for `orders` live orders before reallocating.
    pub fn with_capacity(spec: InstrumentSpec, phase: Phase, orders: usize) -> Result<Self, SpecError> {
        spec.validate()?;
        Ok(MatchingEngine {
            spec,
            phase,
            book: Book::with_capacity(orders),
            next_order_id: 1,
            next_trade_id: 1,
            next_seq: 1,
            last_ts: Timestamp::MIN,
            now: Timestamp::MIN,
            reference_price: spec.reference_price,
        })
    }

    /// Process one command at time `ts`, appending its events to `out` (which is not
    /// cleared). Returns an error, with no events and no state change, for caller errors.
    pub fn process(&mut self, ts: Timestamp, cmd: &Command, out: &mut Vec<Event>) -> Result<(), EngineError> {
        if ts < self.last_ts {
            return Err(EngineError::NonMonotonicTime { last: self.last_ts, got: ts });
        }
        if let Command::SetPhase { phase } = *cmd {
            self.check_transition(phase)?;
        }
        self.last_ts = ts;
        self.now = ts;
        match *cmd {
            Command::New(ref o) => self.on_new(o, out),
            Command::Cancel { owner, order_id } => self.on_cancel(owner, order_id, out),
            Command::Modify { owner, order_id, price, qty } => {
                self.on_modify(owner, order_id, price, qty, out)
            }
            Command::CancelAll { owner } => self.on_cancel_all(owner, out),
            Command::SetPhase { phase } => self.on_set_phase(phase, out),
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------------
    // Queries
    // ------------------------------------------------------------------------------------

    /// Instrument rules.
    pub fn spec(&self) -> &InstrumentSpec {
        &self.spec
    }

    /// Current trading phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Reference price: last trade price, or the spec's initial reference before any trade.
    pub fn reference_price(&self) -> Price {
        self.reference_price
    }

    /// Timestamp of the last processed command.
    pub fn last_timestamp(&self) -> Option<Timestamp> {
        (self.last_ts != Timestamp::MIN).then_some(self.last_ts)
    }

    /// Id the next `New` command will receive.
    pub fn next_order_id(&self) -> OrderId {
        self.next_order_id
    }

    /// Best bid price and total quantity at that price.
    pub fn best_bid(&self) -> Option<(Price, Qty)> {
        self.book.levels[Side::Buy.idx()].last().map(|l| (l.price, l.qty))
    }

    /// Best ask price and total quantity at that price.
    pub fn best_ask(&self) -> Option<(Price, Qty)> {
        self.book.levels[Side::Sell.idx()].last().map(|l| (l.price, l.qty))
    }

    /// Best ask minus best bid (negative while an auction call phase has a crossed book).
    pub fn spread(&self) -> Option<Price> {
        Some(self.best_ask()?.0 - self.best_bid()?.0)
    }

    /// Mid price.
    pub fn mid(&self) -> Option<f64> {
        Some((self.best_ask()?.0 as f64 + self.best_bid()?.0 as f64) / 2.0)
    }

    /// Up to `levels` aggregated limit levels of `side`, best first.
    pub fn depth(&self, side: Side, levels: usize) -> Vec<LevelSnapshot> {
        self.book.levels[side.idx()]
            .iter()
            .rev()
            .take(levels)
            .map(|l| LevelSnapshot { price: l.price, qty: l.qty, count: l.count })
            .collect()
    }

    /// Number of price levels on `side`.
    pub fn level_count(&self, side: Side) -> usize {
        self.book.levels[side.idx()].len()
    }

    /// Number of live orders.
    pub fn live_orders(&self) -> usize {
        self.book.live()
    }

    /// One live order.
    pub fn order(&self, id: OrderId) -> Option<OrderSnapshot> {
        self.book.slot(id).map(|s| self.book.snap(s))
    }

    /// Quantity queued ahead of a live limit order at its price level.
    pub fn queue_ahead(&self, id: OrderId) -> Option<Qty> {
        let slot = self.book.slot(id)?;
        let mut ahead = 0;
        let mut s = self.book.nodes[slot as usize].prev;
        while s != NIL {
            let n = &self.book.nodes[s as usize];
            ahead += n.leaves;
            s = n.prev;
        }
        Some(ahead)
    }

    /// Every live order in priority order: buys then sells; within a side, resting market
    /// orders first (auction only), then price levels from best to worst, FIFO within each.
    pub fn snapshot(&self) -> Vec<OrderSnapshot> {
        let mut v = Vec::with_capacity(self.book.live());
        for side in [Side::Buy, Side::Sell] {
            let mut s = self.book.market[side.idx()].head;
            while s != NIL {
                v.push(self.book.snap(s));
                s = self.book.nodes[s as usize].next;
            }
            for lvl in self.book.levels[side.idx()].iter().rev() {
                let mut s = lvl.head;
                while s != NIL {
                    v.push(self.book.snap(s));
                    s = self.book.nodes[s as usize].next;
                }
            }
        }
        v
    }

    /// Price, volume and imbalance an uncross would produce now (R7). Meaningful during
    /// an auction call phase; `None` if nothing would trade.
    pub fn indicative_auction(&self) -> Option<AuctionQuote> {
        let bids: Vec<(Price, Qty)> =
            self.book.levels[Side::Buy.idx()].iter().map(|l| (l.price, l.qty)).collect();
        let asks: Vec<(Price, Qty)> =
            self.book.levels[Side::Sell.idx()].iter().rev().map(|l| (l.price, l.qty)).collect();
        auction::determine(
            &bids,
            &asks,
            self.book.market[Side::Buy.idx()].qty,
            self.book.market[Side::Sell.idx()].qty,
            self.spec.tick_size,
            self.reference_price,
        )
    }

    /// Verify every structural invariant of the book. Used by tests after each command;
    /// O(live orders).
    pub fn check_invariants(&self) -> Result<(), String> {
        let b = &self.book;
        let mut linked = 0usize;
        for side in [Side::Buy, Side::Sell] {
            let levels = &b.levels[side.idx()];
            for w in levels.windows(2) {
                let ordered = match side {
                    Side::Buy => w[0].price < w[1].price,
                    Side::Sell => w[0].price > w[1].price,
                };
                if !ordered {
                    return Err(format!("{side:?} levels out of order: {} then {}", w[0].price, w[1].price));
                }
            }
            for lvl in levels {
                let (qty, count) = self.check_queue(side, Some(lvl.price), lvl.head, lvl.tail)?;
                if count == 0 {
                    return Err(format!("empty {side:?} level at {}", lvl.price));
                }
                if qty != lvl.qty || count != lvl.count {
                    return Err(format!(
                        "{side:?} level {}: orders sum to {qty}/{count}, level says {}/{}",
                        lvl.price, lvl.qty, lvl.count
                    ));
                }
                linked += count as usize;
            }
            let q = &b.market[side.idx()];
            let (qty, count) = self.check_queue(side, None, q.head, q.tail)?;
            if qty != q.qty || count != q.count {
                return Err(format!("{side:?} market queue totals inconsistent"));
            }
            if count > 0 && matches!(self.phase, Phase::Continuous | Phase::Closed) {
                return Err(format!("market orders resting in {:?}", self.phase));
            }
            linked += count as usize;
        }
        if linked != b.index.len() {
            return Err(format!("{linked} linked orders but {} indexed", b.index.len()));
        }
        if b.nodes.len() != b.index.len() + b.free_len() {
            return Err("arena slots leaked".to_string());
        }
        if matches!(self.phase, Phase::Continuous | Phase::Closed) {
            if let (Some((bid, _)), Some((ask, _))) = (self.best_bid(), self.best_ask()) {
                if bid >= ask {
                    return Err(format!("book crossed in {:?}: bid {bid} >= ask {ask}", self.phase));
                }
            }
        }
        Ok(())
    }

    fn check_queue(&self, side: Side, price: Option<Price>, head: u32, tail: u32) -> Result<(Qty, u32), String> {
        let b = &self.book;
        let (mut qty, mut count, mut prev, mut s, mut last_prio) = (0, 0u32, NIL, head, 0u64);
        while s != NIL {
            let n = &b.nodes[s as usize];
            if n.prev != prev {
                return Err(format!("order {}: broken prev link", n.id));
            }
            if n.side != side {
                return Err(format!("order {} queued on the wrong side", n.id));
            }
            match price {
                Some(p) if n.is_market || n.price != p => {
                    return Err(format!("order {} queued at the wrong level", n.id));
                }
                None if !n.is_market => {
                    return Err(format!("limit order {} in the market queue", n.id));
                }
                _ => {}
            }
            if n.leaves <= 0 || n.leaves != n.order_qty - n.cum_qty {
                return Err(format!(
                    "order {}: leaves {} order_qty {} cum {}",
                    n.id, n.leaves, n.order_qty, n.cum_qty
                ));
            }
            if n.prio <= last_prio {
                return Err(format!("order {}: queue not in time priority", n.id));
            }
            if b.index.get(&n.id) != Some(&s) {
                return Err(format!("order {}: id index mismatch", n.id));
            }
            last_prio = n.prio;
            qty += n.leaves;
            count += 1;
            prev = s;
            s = n.next;
            if count as usize > b.nodes.len() {
                return Err("cycle in queue".to_string());
            }
        }
        if prev != tail {
            return Err(format!("{side:?} queue tail mismatch"));
        }
        Ok((qty, count))
    }

    // ------------------------------------------------------------------------------------
    // Command handlers
    // ------------------------------------------------------------------------------------

    #[inline]
    fn emit(&mut self, out: &mut Vec<Event>, kind: EventKind) {
        out.push(Event { seq: self.next_seq, ts: self.now, kind });
        self.next_seq += 1;
    }

    #[inline]
    fn take_trade_id(&mut self) -> TradeId {
        let t = self.next_trade_id;
        self.next_trade_id += 1;
        t
    }

    fn check_qty(&self, qty: Qty) -> Result<(), RejectReason> {
        if qty <= 0 {
            Err(RejectReason::InvalidQty)
        } else if qty % self.spec.lot_size != 0 {
            Err(RejectReason::LotSize)
        } else if qty > self.spec.max_order_qty {
            Err(RejectReason::MaxQty)
        } else {
            Ok(())
        }
    }

    fn check_price(&self, price: Price) -> Result<(), RejectReason> {
        if price % self.spec.tick_size != 0 {
            Err(RejectReason::TickSize)
        } else if price < self.spec.min_price || price > self.spec.max_price {
            Err(RejectReason::PriceBounds)
        } else {
            Ok(())
        }
    }

    /// Whether a limit order on `side` at `price` would execute against the book now.
    fn would_cross(&self, side: Side, price: Price) -> bool {
        match side {
            Side::Buy => self.best_ask().is_some_and(|(a, _)| price >= a),
            Side::Sell => self.best_bid().is_some_and(|(b, _)| price <= b),
        }
    }

    /// R2: validation order fixes which reason is reported when several apply.
    fn validate_new(&self, o: &NewOrder) -> Result<(), RejectReason> {
        match self.phase {
            Phase::Closed => return Err(RejectReason::MarketClosed),
            Phase::Halted => return Err(RejectReason::TradingHalted),
            Phase::Continuous | Phase::Auction => {}
        }
        self.check_qty(o.qty)?;
        if let Some(p) = o.price {
            self.check_price(p)?;
        }
        if o.post_only && (o.price.is_none() || o.tif != TimeInForce::Gtc) {
            return Err(RejectReason::InvalidPostOnly);
        }
        if self.phase == Phase::Auction {
            if o.tif != TimeInForce::Gtc {
                return Err(RejectReason::TifNotAllowedInPhase);
            }
            if o.post_only {
                return Err(RejectReason::InvalidPostOnly);
            }
        }
        if let (true, Some(p)) = (o.post_only, o.price) {
            if self.would_cross(o.side, p) {
                return Err(RejectReason::PostOnlyWouldCross);
            }
        }
        Ok(())
    }

    fn on_new(&mut self, o: &NewOrder, out: &mut Vec<Event>) {
        let order_id = self.next_order_id;
        self.next_order_id += 1;
        if let Err(reason) = self.validate_new(o) {
            self.emit(out, EventKind::Rejected { order_id, owner: o.owner, client_tag: o.client_tag, reason });
            return;
        }
        self.emit(
            out,
            EventKind::Accepted {
                order_id,
                owner: o.owner,
                client_tag: o.client_tag,
                side: o.side,
                price: o.price,
                qty: o.qty,
                tif: o.tif,
            },
        );
        if self.phase == Phase::Auction {
            // R6: call phase, rest without matching.
            self.rest(order_id, o, o.qty, 0, out);
            return;
        }
        // R3/R4: continuous trading.
        if o.tif == TimeInForce::Fok && self.fillable(o.side, o.price, o.owner, o.stp, o.qty) < o.qty {
            self.emit(
                out,
                EventKind::Cancelled { order_id, owner: o.owner, qty: o.qty, reason: CancelReason::FokUnfilled },
            );
            return;
        }
        let mut t = Taker {
            order_id,
            owner: o.owner,
            side: o.side,
            limit: o.price,
            leaves: o.qty,
            filled: 0,
            stp: o.stp,
        };
        self.match_taker(&mut t, out);
        if t.leaves > 0 {
            if o.price.is_some() && o.tif == TimeInForce::Gtc {
                self.rest(order_id, o, t.leaves, t.filled, out);
            } else {
                self.emit(
                    out,
                    EventKind::Cancelled {
                        order_id,
                        owner: o.owner,
                        qty: t.leaves,
                        reason: CancelReason::IocRemainder,
                    },
                );
            }
        }
    }

    fn rest(&mut self, order_id: OrderId, o: &NewOrder, leaves: Qty, cum: Qty, out: &mut Vec<Event>) {
        let slot = self.book.alloc(Node {
            id: order_id,
            price: o.price.unwrap_or(0),
            leaves,
            order_qty: o.qty,
            cum_qty: cum,
            prio: 0,
            owner: o.owner,
            prev: NIL,
            next: NIL,
            side: o.side,
            is_market: o.price.is_none(),
            post_only: o.post_only,
            stp: o.stp,
        });
        self.book.link(slot);
        self.emit(out, EventKind::BookAdd { order_id, side: o.side, price: o.price, qty: leaves });
    }

    /// Quantity a FOK order could execute right now, scanning in priority order and
    /// applying the same self-trade prevention the walk would (R4). Stops once `need`
    /// is reached.
    fn fillable(&self, side: Side, limit: Option<Price>, owner: OwnerId, stp: StpMode, need: Qty) -> Qty {
        let mut acc: Qty = 0;
        for lvl in self.book.levels[side.opposite().idx()].iter().rev() {
            if let Some(lim) = limit {
                if !crosses(side, lim, lvl.price) {
                    break;
                }
            }
            let mut s = lvl.head;
            while s != NIL {
                let n = &self.book.nodes[s as usize];
                if stp != StpMode::None && n.owner == owner {
                    if stp != StpMode::CancelResting {
                        // The walk would cancel the aggressor here.
                        return acc;
                    }
                    // CancelResting: this order would be cancelled, contributing nothing.
                } else {
                    acc += n.leaves;
                    if acc >= need {
                        return acc;
                    }
                }
                s = n.next;
            }
        }
        acc
    }

    /// R3: walk the opposite side in price-time priority. Trades print at the resting
    /// order's price. Self-trade prevention per R5.
    fn match_taker(&mut self, t: &mut Taker, out: &mut Vec<Event>) {
        let opp = t.side.opposite();
        let oi = opp.idx();
        while t.leaves > 0 {
            let (level_price, head) = match self.book.levels[oi].last() {
                Some(l) => (l.price, l.head),
                None => break,
            };
            if let Some(lim) = t.limit {
                if !crosses(t.side, lim, level_price) {
                    break;
                }
            }
            let (m_id, m_owner, m_leaves) = {
                let n = &self.book.nodes[head as usize];
                (n.id, n.owner, n.leaves)
            };
            if t.stp != StpMode::None && m_owner == t.owner {
                match t.stp {
                    StpMode::CancelResting => {
                        self.cancel_resting(head, CancelReason::SelfTrade, out);
                        continue;
                    }
                    StpMode::CancelAggressor | StpMode::CancelBoth => {
                        if t.stp == StpMode::CancelBoth {
                            self.cancel_resting(head, CancelReason::SelfTrade, out);
                        }
                        self.emit(
                            out,
                            EventKind::Cancelled {
                                order_id: t.order_id,
                                owner: t.owner,
                                qty: t.leaves,
                                reason: CancelReason::SelfTrade,
                            },
                        );
                        t.leaves = 0;
                        break;
                    }
                    StpMode::None => unreachable!(),
                }
            }
            let q = t.leaves.min(m_leaves);
            let trade_id = self.take_trade_id();
            let (buy_order_id, sell_order_id) = match t.side {
                Side::Buy => (t.order_id, m_id),
                Side::Sell => (m_id, t.order_id),
            };
            self.emit(
                out,
                EventKind::Trade {
                    trade_id,
                    price: level_price,
                    qty: q,
                    aggressor: Some(t.side),
                    buy_order_id,
                    sell_order_id,
                },
            );
            let m_left = m_leaves - q;
            {
                let n = &mut self.book.nodes[head as usize];
                n.leaves = m_left;
                n.cum_qty += q;
            }
            if let Some(l) = self.book.levels[oi].last_mut() {
                l.qty -= q;
            }
            self.emit(
                out,
                EventKind::Fill {
                    order_id: m_id,
                    owner: m_owner,
                    trade_id,
                    price: level_price,
                    qty: q,
                    leaves: m_left,
                    liquidity: Liquidity::Maker,
                },
            );
            t.leaves -= q;
            t.filled += q;
            self.emit(
                out,
                EventKind::Fill {
                    order_id: t.order_id,
                    owner: t.owner,
                    trade_id,
                    price: level_price,
                    qty: q,
                    leaves: t.leaves,
                    liquidity: Liquidity::Taker,
                },
            );
            self.reference_price = level_price;
            if m_left == 0 {
                let slot = self.book.pop_best_head(opp);
                self.book.release(slot);
            }
        }
    }

    /// Cancel a resting order's open quantity and remove it from the book.
    fn cancel_resting(&mut self, slot: u32, reason: CancelReason, out: &mut Vec<Event>) {
        let (order_id, owner, qty) = {
            let n = &self.book.nodes[slot as usize];
            (n.id, n.owner, n.leaves)
        };
        self.emit(out, EventKind::Cancelled { order_id, owner, qty, reason });
        self.emit(out, EventKind::BookRemove { order_id });
        self.book.unlink(slot);
        self.book.release(slot);
    }

    fn on_cancel(&mut self, owner: OwnerId, order_id: OrderId, out: &mut Vec<Event>) {
        let reason = match self.book.slot(order_id) {
            None => RejectReason::UnknownOrder,
            Some(slot) if self.book.nodes[slot as usize].owner != owner => RejectReason::NotOwner,
            Some(slot) => {
                self.cancel_resting(slot, CancelReason::Requested, out);
                return;
            }
        };
        self.emit(out, EventKind::RequestRejected { order_id, owner, request: RequestKind::Cancel, reason });
    }

    fn validate_modify(&self, owner: OwnerId, order_id: OrderId, price: Price, qty: Qty) -> Result<u32, RejectReason> {
        match self.phase {
            Phase::Closed => return Err(RejectReason::MarketClosed),
            Phase::Halted => return Err(RejectReason::TradingHalted),
            Phase::Continuous | Phase::Auction => {}
        }
        let slot = self.book.slot(order_id).ok_or(RejectReason::UnknownOrder)?;
        let n = &self.book.nodes[slot as usize];
        if n.owner != owner {
            return Err(RejectReason::NotOwner);
        }
        if n.is_market {
            return Err(RejectReason::MarketOrderNotModifiable);
        }
        self.check_qty(qty)?;
        self.check_price(price)?;
        Ok(slot)
    }

    /// R9: cancel/replace.
    fn on_modify(&mut self, owner: OwnerId, order_id: OrderId, price: Price, qty: Qty, out: &mut Vec<Event>) {
        let slot = match self.validate_modify(owner, order_id, price, qty) {
            Ok(slot) => slot,
            Err(reason) => {
                self.emit(out, EventKind::RequestRejected { order_id, owner, request: RequestKind::Modify, reason });
                return;
            }
        };
        let (side, old_price, old_leaves, cum, stp, post_only) = {
            let n = &self.book.nodes[slot as usize];
            (n.side, n.price, n.leaves, n.cum_qty, n.stp, n.post_only)
        };
        if qty <= cum {
            self.cancel_resting(slot, CancelReason::ModifiedBelowFilled, out);
            return;
        }
        let new_leaves = qty - cum;
        if post_only && self.phase == Phase::Continuous && self.would_cross(side, price) {
            self.emit(
                out,
                EventKind::RequestRejected {
                    order_id,
                    owner,
                    request: RequestKind::Modify,
                    reason: RejectReason::PostOnlyWouldCross,
                },
            );
            return;
        }
        if price == old_price && new_leaves <= old_leaves {
            // Same price and no size increase: keep time priority.
            self.book.reduce(slot, old_leaves - new_leaves);
            self.book.nodes[slot as usize].order_qty = qty;
            self.emit(out, EventKind::Replaced { order_id, owner, price, qty: new_leaves, priority_kept: true });
            self.emit(out, EventKind::BookUpdate { order_id, qty: new_leaves });
            return;
        }
        // Price change or size increase: leave the queue, re-enter as a new arrival.
        self.emit(out, EventKind::Replaced { order_id, owner, price, qty: new_leaves, priority_kept: false });
        self.emit(out, EventKind::BookRemove { order_id });
        self.book.unlink(slot);
        {
            let n = &mut self.book.nodes[slot as usize];
            n.price = price;
            n.order_qty = qty;
            n.leaves = new_leaves;
        }
        let mut leaves = new_leaves;
        if self.phase == Phase::Continuous {
            let mut t = Taker { order_id, owner, side, limit: Some(price), leaves: new_leaves, filled: 0, stp };
            self.match_taker(&mut t, out);
            leaves = t.leaves;
            let n = &mut self.book.nodes[slot as usize];
            n.leaves = t.leaves;
            n.cum_qty += t.filled;
        }
        if leaves > 0 {
            self.book.link(slot);
            self.emit(out, EventKind::BookAdd { order_id, side, price: Some(price), qty: leaves });
        } else {
            self.book.release(slot);
        }
    }

    /// R10: cancel every live order of `owner`, in ascending order id.
    fn on_cancel_all(&mut self, owner: OwnerId, out: &mut Vec<Event>) {
        let nodes = &self.book.nodes;
        let mut victims: Vec<(OrderId, u32)> = self
            .book
            .index
            .iter()
            .filter(|&(_, &s)| nodes[s as usize].owner == owner)
            .map(|(&id, &s)| (id, s))
            .collect();
        victims.sort_unstable_by_key(|&(id, _)| id);
        for (_, slot) in victims {
            self.cancel_resting(slot, CancelReason::MassCancel, out);
        }
    }

    /// No resting market orders and no crossed book.
    fn is_clean(&self) -> bool {
        let crossed = matches!((self.best_bid(), self.best_ask()), (Some((b, _)), Some((a, _))) if b >= a);
        self.book.market[0].count == 0 && self.book.market[1].count == 0 && !crossed
    }

    /// R8: phase transition table.
    fn check_transition(&self, to: Phase) -> Result<(), EngineError> {
        let from = self.phase;
        let allowed = from != to
            && match to {
                Phase::Auction | Phase::Halted => true,
                Phase::Continuous | Phase::Closed => match from {
                    Phase::Continuous | Phase::Auction => true,
                    Phase::Halted | Phase::Closed => self.is_clean(),
                },
            };
        if allowed { Ok(()) } else { Err(EngineError::InvalidPhaseTransition { from, to }) }
    }

    fn on_set_phase(&mut self, to: Phase, out: &mut Vec<Event>) {
        if self.phase == Phase::Auction && matches!(to, Phase::Continuous | Phase::Closed) {
            self.uncross(out);
        }
        self.phase = to;
        self.emit(out, EventKind::PhaseChanged { phase: to });
    }

    /// Slots eligible to trade at `price` in an uncross, in allocation priority:
    /// market orders (time priority), then limit orders by price, then time.
    fn eligible(&self, side: Side, price: Price) -> Vec<u32> {
        let mut v = Vec::new();
        let mut s = self.book.market[side.idx()].head;
        while s != NIL {
            v.push(s);
            s = self.book.nodes[s as usize].next;
        }
        for lvl in self.book.levels[side.idx()].iter().rev() {
            let ok = match side {
                Side::Buy => lvl.price >= price,
                Side::Sell => lvl.price <= price,
            };
            if !ok {
                break;
            }
            let mut s = lvl.head;
            while s != NIL {
                v.push(s);
                s = self.book.nodes[s as usize].next;
            }
        }
        v
    }

    /// R7: execute the call auction at a single price, then cancel unexecuted market orders.
    fn uncross(&mut self, out: &mut Vec<Event>) {
        if let Some(q) = self.indicative_auction() {
            self.emit(out, EventKind::AuctionUncross { price: Some(q.price), volume: q.volume });
            let buys = self.eligible(Side::Buy, q.price);
            let sells = self.eligible(Side::Sell, q.price);
            let (mut i, mut j, mut remaining) = (0usize, 0usize, q.volume);
            while remaining > 0 {
                let (bs, ss) = (buys[i], sells[j]);
                let (b_id, b_owner, b_leaves) = {
                    let n = &self.book.nodes[bs as usize];
                    (n.id, n.owner, n.leaves)
                };
                let (s_id, s_owner, s_leaves) = {
                    let n = &self.book.nodes[ss as usize];
                    (n.id, n.owner, n.leaves)
                };
                let x = b_leaves.min(s_leaves).min(remaining);
                let trade_id = self.take_trade_id();
                self.emit(
                    out,
                    EventKind::Trade {
                        trade_id,
                        price: q.price,
                        qty: x,
                        aggressor: None,
                        buy_order_id: b_id,
                        sell_order_id: s_id,
                    },
                );
                let b_left = self.book.reduce(bs, x);
                self.book.nodes[bs as usize].cum_qty += x;
                self.emit(
                    out,
                    EventKind::Fill {
                        order_id: b_id,
                        owner: b_owner,
                        trade_id,
                        price: q.price,
                        qty: x,
                        leaves: b_left,
                        liquidity: Liquidity::Auction,
                    },
                );
                let s_left = self.book.reduce(ss, x);
                self.book.nodes[ss as usize].cum_qty += x;
                self.emit(
                    out,
                    EventKind::Fill {
                        order_id: s_id,
                        owner: s_owner,
                        trade_id,
                        price: q.price,
                        qty: x,
                        leaves: s_left,
                        liquidity: Liquidity::Auction,
                    },
                );
                remaining -= x;
                if b_left == 0 {
                    self.book.unlink(bs);
                    self.book.release(bs);
                    i += 1;
                }
                if s_left == 0 {
                    self.book.unlink(ss);
                    self.book.release(ss);
                    j += 1;
                }
            }
            self.reference_price = q.price;
        } else {
            self.emit(out, EventKind::AuctionUncross { price: None, volume: 0 });
        }
        for side in [Side::Buy, Side::Sell] {
            loop {
                let head = self.book.market[side.idx()].head;
                if head == NIL {
                    break;
                }
                self.cancel_resting(head, CancelReason::AuctionMarketRemainder, out);
            }
        }
    }
}
