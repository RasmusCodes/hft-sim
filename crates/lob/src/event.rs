//! Engine output: private execution reports and the public market-by-order feed.
//!
//! One command produces zero or more events, in a fixed documented order
//! (`docs/MATCHING_RULES.md`, section "Event order"). Every event carries a sequence
//! number (contiguous from 1) and the timestamp of the command that caused it.

use std::hash::{Hash, Hasher};

use crate::types::{
    Liquidity, OrderId, OwnerId, Phase, Price, Qty, SeqNo, Side, TimeInForce, Timestamp, TradeId,
};

/// Why a new order, cancel or modify was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RejectReason {
    /// The market is closed.
    MarketClosed,
    /// Trading is halted.
    TradingHalted,
    /// Quantity is zero or negative.
    InvalidQty,
    /// Quantity is not a multiple of the lot size.
    LotSize,
    /// Quantity exceeds the maximum order quantity.
    MaxQty,
    /// Price is not a multiple of the tick size.
    TickSize,
    /// Price is outside the instrument's price bounds.
    PriceBounds,
    /// IOC and FOK orders are not accepted during an auction call phase.
    TifNotAllowedInPhase,
    /// Post-only is only valid on GTC limit orders in continuous trading.
    InvalidPostOnly,
    /// A post-only order (or modify) would execute on arrival.
    PostOnlyWouldCross,
    /// The order id is not live (never existed, already filled or cancelled).
    UnknownOrder,
    /// The requester does not own the order.
    NotOwner,
    /// Market orders cannot be modified.
    MarketOrderNotModifiable,
}

/// Why open quantity was cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CancelReason {
    /// Cancel requested by the owner.
    Requested,
    /// Cancelled by a `CancelAll` command.
    MassCancel,
    /// Unfilled remainder of an IOC or market order.
    IocRemainder,
    /// A fill-or-kill order could not be filled in full.
    FokUnfilled,
    /// Self-trade prevention.
    SelfTrade,
    /// A modify set the total quantity at or below the filled quantity.
    ModifiedBelowFilled,
    /// Market order left unexecuted by an auction uncross.
    AuctionMarketRemainder,
}

/// Request type of a [`EventKind::RequestRejected`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RequestKind {
    /// A `Cancel` command.
    Cancel,
    /// A `Modify` command.
    Modify,
}

/// Event payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventKind {
    // ---- private execution reports (routed to `owner`) -------------------------------
    /// A new order passed validation. Followed by fills, a `BookAdd` and/or a `Cancelled`.
    Accepted {
        /// Assigned order id.
        order_id: OrderId,
        /// Owner.
        owner: OwnerId,
        /// Echo of the client tag.
        client_tag: u64,
        /// Side.
        side: Side,
        /// Limit price, `None` for market orders.
        price: Option<Price>,
        /// Order quantity.
        qty: Qty,
        /// Time in force.
        tif: TimeInForce,
    },
    /// A new order failed validation. It never entered the book.
    Rejected {
        /// Assigned order id (ids are consumed by rejected orders too).
        order_id: OrderId,
        /// Owner.
        owner: OwnerId,
        /// Echo of the client tag.
        client_tag: u64,
        /// Reason.
        reason: RejectReason,
    },
    /// Execution of `qty` at `price` for one order.
    Fill {
        /// Order executed.
        order_id: OrderId,
        /// Owner.
        owner: OwnerId,
        /// Trade this fill belongs to.
        trade_id: TradeId,
        /// Execution price.
        price: Price,
        /// Executed quantity.
        qty: Qty,
        /// Open quantity after this fill.
        leaves: Qty,
        /// Maker, taker or auction.
        liquidity: Liquidity,
    },
    /// Open quantity cancelled; the order is done.
    Cancelled {
        /// Order cancelled.
        order_id: OrderId,
        /// Owner.
        owner: OwnerId,
        /// Open quantity that was cancelled.
        qty: Qty,
        /// Reason.
        reason: CancelReason,
    },
    /// A modify was applied.
    Replaced {
        /// Order modified.
        order_id: OrderId,
        /// Owner.
        owner: OwnerId,
        /// New limit price.
        price: Price,
        /// New open quantity, before any execution caused by the modify.
        qty: Qty,
        /// Whether the order kept its time priority.
        priority_kept: bool,
    },
    /// A cancel or modify was refused; the order (if any) is unchanged.
    RequestRejected {
        /// Order the request referred to.
        order_id: OrderId,
        /// Requester.
        owner: OwnerId,
        /// Cancel or modify.
        request: RequestKind,
        /// Reason.
        reason: RejectReason,
    },

    // ---- public market data: anonymous market-by-order feed ---------------------------
    /// An order starts resting in the book (at the back of its queue).
    BookAdd {
        /// Order id.
        order_id: OrderId,
        /// Side.
        side: Side,
        /// Limit price, `None` for a market order resting in an auction call phase.
        price: Option<Price>,
        /// Displayed open quantity.
        qty: Qty,
    },
    /// A resting order's open quantity was reduced in place (priority kept).
    BookUpdate {
        /// Order id.
        order_id: OrderId,
        /// New open quantity.
        qty: Qty,
    },
    /// A resting order left the book other than by execution.
    BookRemove {
        /// Order id.
        order_id: OrderId,
    },
    /// Trade print. Executions reduce resting orders implicitly: in continuous trading
    /// the resting (passive) order, in an auction both orders.
    Trade {
        /// Trade id.
        trade_id: TradeId,
        /// Execution price.
        price: Price,
        /// Executed quantity.
        qty: Qty,
        /// Side of the incoming order; `None` for auction executions.
        aggressor: Option<Side>,
        /// Buy order.
        buy_order_id: OrderId,
        /// Sell order.
        sell_order_id: OrderId,
    },
    /// Result of an auction uncross: price and executed volume (`None`/0 if no trade).
    AuctionUncross {
        /// Uncross price.
        price: Option<Price>,
        /// Executed volume.
        volume: Qty,
    },
    /// The trading phase changed.
    PhaseChanged {
        /// New phase.
        phase: Phase,
    },
}

impl EventKind {
    /// Whether the event belongs to the public (anonymous) market data feed.
    pub fn is_public(&self) -> bool {
        matches!(
            self,
            EventKind::BookAdd { .. }
                | EventKind::BookUpdate { .. }
                | EventKind::BookRemove { .. }
                | EventKind::Trade { .. }
                | EventKind::AuctionUncross { .. }
                | EventKind::PhaseChanged { .. }
        )
    }

    /// Participant a private execution report is routed to (`None` for public events).
    pub fn owner(&self) -> Option<OwnerId> {
        match *self {
            EventKind::Accepted { owner, .. }
            | EventKind::Rejected { owner, .. }
            | EventKind::Fill { owner, .. }
            | EventKind::Cancelled { owner, .. }
            | EventKind::Replaced { owner, .. }
            | EventKind::RequestRejected { owner, .. } => Some(owner),
            _ => None,
        }
    }
}

/// One engine output event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Event {
    /// Sequence number, contiguous from 1.
    pub seq: SeqNo,
    /// Timestamp of the command that produced the event.
    pub ts: Timestamp,
    /// Payload.
    pub kind: EventKind,
}

/// Running FNV-1a digest of an event stream, for determinism and regression checks:
/// two runs are identical if and only if (up to hash collisions) their digests match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Digest(u64);

impl Default for Digest {
    fn default() -> Self {
        Digest(0xcbf2_9ce4_8422_2325)
    }
}

impl Digest {
    /// Empty digest.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event into the digest.
    pub fn update(&mut self, event: &Event) {
        let mut h = Fnv(self.0);
        event.hash(&mut h);
        self.0 = h.0;
    }

    /// Current digest value.
    pub fn value(&self) -> u64 {
        self.0
    }
}

struct Fnv(u64);

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}
