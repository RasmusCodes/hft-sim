//! Messages between agents and the exchange.
//!
//! Agents name their orders with their own client order ids. The exchange order id is only
//! known once the acknowledgement arrives, possibly after the agent already wants to cancel,
//! so every request uses client ids and the gateway translates (as FIX `ClOrdID` does).

use lob::{
    CancelReason, Liquidity, OrderId, Price, Qty, RejectReason, RequestKind, Side, StpMode,
    TimeInForce, Timestamp, TradeId,
};

/// Index of an agent. Also its owner id at the exchange.
pub type AgentId = u32;
/// Index of an instrument.
pub type InstrumentId = u32;
/// Index of a signal source.
pub type SignalId = u32;
/// Order id chosen by the agent; must be unique per agent across all instruments.
pub type ClientOrderId = u64;

/// A new order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OrderSpec {
    /// Instrument.
    pub instrument: InstrumentId,
    /// The agent's id for this order.
    pub client_id: ClientOrderId,
    /// Side.
    pub side: Side,
    /// Limit price; `None` for a market order.
    pub price: Option<Price>,
    /// Quantity.
    pub qty: Qty,
    /// Time in force.
    pub tif: TimeInForce,
    /// Post-only (maker-only).
    pub post_only: bool,
    /// Self-trade prevention instruction.
    pub stp: StpMode,
}

impl OrderSpec {
    /// GTC limit order.
    pub fn limit(instrument: InstrumentId, client_id: ClientOrderId, side: Side, price: Price, qty: Qty) -> Self {
        OrderSpec {
            instrument,
            client_id,
            side,
            price: Some(price),
            qty,
            tif: TimeInForce::Gtc,
            post_only: false,
            stp: StpMode::None,
        }
    }

    /// Market order (IOC).
    pub fn market(instrument: InstrumentId, client_id: ClientOrderId, side: Side, qty: Qty) -> Self {
        OrderSpec {
            instrument,
            client_id,
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
}

/// Agent → exchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Request {
    /// Submit a new order.
    New(OrderSpec),
    /// Cancel an order.
    Cancel {
        /// Instrument of the order.
        instrument: InstrumentId,
        /// The agent's id of the order.
        client_id: ClientOrderId,
    },
    /// Cancel/replace (new price and new total quantity, FIX semantics; see MATCHING_RULES R9).
    Modify {
        /// Instrument of the order.
        instrument: InstrumentId,
        /// The agent's id of the order.
        client_id: ClientOrderId,
        /// New limit price.
        price: Price,
        /// New total order quantity (filled + open).
        qty: Qty,
    },
    /// Cancel all of the agent's orders on one instrument.
    CancelAll {
        /// Instrument.
        instrument: InstrumentId,
    },
}

impl Request {
    /// Instrument the request is for.
    pub fn instrument(&self) -> InstrumentId {
        match *self {
            Request::New(o) => o.instrument,
            Request::Cancel { instrument, .. }
            | Request::Modify { instrument, .. }
            | Request::CancelAll { instrument } => instrument,
        }
    }

    /// Request type.
    pub fn kind(&self) -> RequestType {
        match self {
            Request::New(_) => RequestType::New,
            Request::Cancel { .. } => RequestType::Cancel,
            Request::Modify { .. } => RequestType::Modify,
            Request::CancelAll { .. } => RequestType::CancelAll,
        }
    }

    /// Client order id the request refers to (0 for `CancelAll`).
    pub fn client_id(&self) -> ClientOrderId {
        match *self {
            Request::New(o) => o.client_id,
            Request::Cancel { client_id, .. } | Request::Modify { client_id, .. } => client_id,
            Request::CancelAll { .. } => 0,
        }
    }
}

/// Type of a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RequestType {
    /// New order.
    New,
    /// Cancel.
    Cancel,
    /// Cancel/replace.
    Modify,
    /// Cancel all.
    CancelAll,
}

/// Why the gateway refused a request before it reached the matching engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GatewayReject {
    /// Message rate above the session's throttle.
    Throttled,
    /// No such instrument.
    UnknownInstrument,
    /// The client order id is already in use by a live order.
    DuplicateClientOrderId,
    /// Order quantity above the session's limit.
    MaxOrderQty,
    /// Too many open orders on the instrument.
    MaxOpenOrders,
    /// The order could take the position beyond the session's limit if everything filled.
    MaxPosition,
}

/// Content of an execution report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReportKind {
    /// The order passed validation and has an exchange order id.
    Accepted {
        /// Side.
        side: Side,
        /// Limit price (`None` for market orders).
        price: Option<Price>,
        /// Quantity.
        qty: Qty,
        /// Time in force.
        tif: TimeInForce,
    },
    /// The matching engine rejected the order.
    Rejected {
        /// Reason.
        reason: RejectReason,
    },
    /// An execution.
    Fill {
        /// Trade id.
        trade_id: TradeId,
        /// Side of the agent's order.
        side: Side,
        /// Price.
        price: Price,
        /// Quantity.
        qty: Qty,
        /// Open quantity left.
        leaves: Qty,
        /// Maker, taker or auction.
        liquidity: Liquidity,
    },
    /// Open quantity cancelled; the order is done.
    Cancelled {
        /// Quantity cancelled.
        qty: Qty,
        /// Reason.
        reason: CancelReason,
    },
    /// A modify was applied.
    Replaced {
        /// New price.
        price: Price,
        /// New open quantity.
        qty: Qty,
        /// Whether time priority was kept.
        priority_kept: bool,
    },
    /// A cancel or modify was refused. `UnknownOrder` also means "too late": the order was
    /// already filled or cancelled, by the engine or (if the gateway already knew) by the gateway.
    RequestRejected {
        /// Cancel or modify.
        request: RequestKind,
        /// Reason.
        reason: RejectReason,
    },
    /// The gateway refused the request (throttle or pre-trade risk); the engine never saw it.
    GatewayRejected {
        /// Request type.
        request: RequestType,
        /// Reason.
        reason: GatewayReject,
    },
}

/// Exchange → agent, about one of the agent's orders.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExecReport {
    /// Instrument.
    pub instrument: InstrumentId,
    /// The agent's id of the order (0 for reports about a `CancelAll` request itself).
    pub client_id: ClientOrderId,
    /// Exchange order id, once assigned.
    pub order_id: Option<OrderId>,
    /// Exchange time of the event (engine or gateway processing time), not arrival time.
    pub exchange_ts: Timestamp,
    /// Content.
    pub kind: ReportKind,
}
