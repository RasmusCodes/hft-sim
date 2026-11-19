//! Value types shared by the matching engine, the reference model and downstream
//! consumers (feed handlers, agents, research code).
//!
//! Prices and quantities are integers. Floating point never enters the matching
//! engine, so every result is exact and reproducible.

/// Price in integer price units of the instrument (for example 1 unit = 0.01 USD).
/// Order prices must be multiples of [`crate::InstrumentSpec::tick_size`]. Negative
/// prices are representable; the instrument's price bounds decide whether they are valid.
pub type Price = i64;

/// Quantity in integer units (shares, contracts). Order quantities must be positive
/// multiples of [`crate::InstrumentSpec::lot_size`].
pub type Qty = i64;

/// Exchange-assigned order id: 1, 2, 3, ... in the order `New` commands are processed,
/// whether they are accepted or rejected.
pub type OrderId = u64;

/// Participant (account) id. Routes execution reports, authorises cancels and
/// modifies, and drives self-trade prevention.
pub type OwnerId = u32;

/// Exchange-assigned trade id: 1, 2, 3, ...
pub type TradeId = u64;

/// Engine output sequence number: 1, 2, 3, ... across all events of one engine.
pub type SeqNo = u64;

/// Nanoseconds on the simulation clock. The engine never reads a wall clock: every
/// command carries the timestamp at which the engine processes it.
pub type Timestamp = i64;

/// Order side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    /// Bid.
    Buy = 0,
    /// Offer.
    Sell = 1,
}

impl Side {
    /// The other side.
    #[inline]
    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    #[inline]
    pub(crate) fn idx(self) -> usize {
        self as usize
    }
}

/// Time in force.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimeInForce {
    /// Good-till-cancelled: the unfilled remainder of a limit order rests in the book.
    Gtc,
    /// Immediate-or-cancel: execute what is possible on arrival, cancel the rest.
    Ioc,
    /// Fill-or-kill: execute the full quantity on arrival, or nothing.
    Fok,
}

/// Self-trade prevention instruction. It is carried by the order and applies when that
/// order is the aggressor in continuous trading; it has no effect while the order rests
/// and none in auctions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StpMode {
    /// Self-trades are allowed.
    None,
    /// Cancel the resting order and keep matching ("cancel oldest").
    CancelResting,
    /// Cancel the aggressor's remaining quantity ("cancel newest").
    CancelAggressor,
    /// Cancel the resting order and the aggressor's remaining quantity.
    CancelBoth,
}

/// Trading phase of the instrument.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Continuous matching in price-time priority.
    Continuous,
    /// Call phase of a call auction: orders rest without matching and the book may cross.
    Auction,
    /// Trading halted: only cancels are accepted.
    Halted,
    /// Market closed: only cancels are accepted.
    Closed,
}

/// Liquidity role of a fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Liquidity {
    /// Resting order that was executed (provided liquidity).
    Maker,
    /// Incoming order that executed against the book (took liquidity).
    Taker,
    /// Execution in an auction uncross (no maker/taker distinction).
    Auction,
}

/// A new order request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NewOrder {
    /// Submitting participant.
    pub owner: OwnerId,
    /// Opaque client reference, echoed in `Accepted` / `Rejected`.
    pub client_tag: u64,
    /// Buy or sell.
    pub side: Side,
    /// `Some(p)`: limit order at `p`. `None`: market order.
    pub price: Option<Price>,
    /// Order quantity.
    pub qty: Qty,
    /// Time in force.
    pub tif: TimeInForce,
    /// Maker-only: reject the order if it would execute on arrival.
    pub post_only: bool,
    /// Self-trade prevention instruction.
    pub stp: StpMode,
}

impl NewOrder {
    /// GTC limit order without post-only or self-trade prevention.
    pub fn limit(owner: OwnerId, side: Side, price: Price, qty: Qty) -> Self {
        NewOrder {
            owner,
            client_tag: 0,
            side,
            price: Some(price),
            qty,
            tif: TimeInForce::Gtc,
            post_only: false,
            stp: StpMode::None,
        }
    }

    /// Market order (IOC by default; use GTC to enter one during an auction call phase).
    pub fn market(owner: OwnerId, side: Side, qty: Qty) -> Self {
        NewOrder {
            owner,
            client_tag: 0,
            side,
            price: None,
            qty,
            tif: TimeInForce::Ioc,
            post_only: false,
            stp: StpMode::None,
        }
    }

    /// Set the time in force.
    pub fn with_tif(mut self, tif: TimeInForce) -> Self {
        self.tif = tif;
        self
    }

    /// Make the order post-only.
    pub fn with_post_only(mut self) -> Self {
        self.post_only = true;
        self
    }

    /// Set the self-trade prevention instruction.
    pub fn with_stp(mut self, stp: StpMode) -> Self {
        self.stp = stp;
        self
    }

    /// Set the client tag.
    pub fn with_tag(mut self, tag: u64) -> Self {
        self.client_tag = tag;
        self
    }
}

/// Input to the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    /// Submit a new order.
    New(NewOrder),
    /// Cancel a live order.
    Cancel {
        /// Requesting participant (must own the order).
        owner: OwnerId,
        /// Order to cancel.
        order_id: OrderId,
    },
    /// Cancel/replace a live limit order. `qty` is the new *total* order quantity
    /// including what has already been filled (FIX `OrderQty` semantics), so a modify
    /// that races a fill cannot increase the participant's intended exposure.
    Modify {
        /// Requesting participant (must own the order).
        owner: OwnerId,
        /// Order to modify.
        order_id: OrderId,
        /// New limit price.
        price: Price,
        /// New total order quantity (filled + open).
        qty: Qty,
    },
    /// Cancel every live order of `owner` (kill switch).
    CancelAll {
        /// Participant whose orders are cancelled.
        owner: OwnerId,
    },
    /// Administrative phase change (open, halt, call auction, close).
    SetPhase {
        /// Target phase.
        phase: Phase,
    },
}

/// One aggregated price level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LevelSnapshot {
    /// Level price.
    pub price: Price,
    /// Total open quantity at the level.
    pub qty: Qty,
    /// Number of orders at the level.
    pub count: u32,
}

/// One live order, as returned by snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OrderSnapshot {
    /// Exchange order id.
    pub order_id: OrderId,
    /// Owning participant.
    pub owner: OwnerId,
    /// Side.
    pub side: Side,
    /// Limit price (`None` for a market order resting in an auction call phase).
    pub price: Option<Price>,
    /// Open (unfilled) quantity.
    pub leaves: Qty,
    /// Total order quantity, as last set by the order or a modify.
    pub order_qty: Qty,
    /// Quantity filled so far.
    pub cum_qty: Qty,
}
