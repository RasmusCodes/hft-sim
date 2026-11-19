//! The agent interface: what a trading strategy sees and can do.
//!
//! An agent never touches the exchange directly. It learns about the market only through
//! callbacks that arrive after their latency (its own execution reports, its delayed market
//! data feed, delayed signals, its own timers), and it acts only by sending requests, which
//! leave after its compute delay and travel its order-entry link to the gateway.

use std::any::Any;
use std::collections::BTreeMap;

use lob::rng::Rng;
use lob::{Event, InstrumentSpec, MboBook, OrderId, Price, Qty, RequestKind, Side, Timestamp};

use crate::feed::{Feed, FeedId};
use crate::latency::{LatencyModel, Link};
use crate::messages::{
    AgentId, ClientOrderId, ExecReport, GatewayReject, InstrumentId, ReportKind, Request, RequestType,
    SignalId,
};

/// A trading agent. All callbacks are optional.
pub trait Agent: Any {
    /// Once, at simulation time 0 (agents in id order).
    fn on_start(&mut self, _ctx: &mut Ctx<'_>) {}
    /// A market-data packet arrived. `ctx.book(instrument)` already includes it.
    fn on_market_data(&mut self, _ctx: &mut Ctx<'_>, _instrument: InstrumentId, _events: &[Event]) {}
    /// An execution report arrived. `ctx.orders()` already includes it.
    fn on_report(&mut self, _ctx: &mut Ctx<'_>, _report: &ExecReport) {}
    /// A timer set with [`Ctx::set_timer`] fired.
    fn on_timer(&mut self, _ctx: &mut Ctx<'_>, _token: u64) {}
    /// A signal value arrived.
    fn on_signal(&mut self, _ctx: &mut Ctx<'_>, _signal: SignalId, _value: i64) {}
}

/// One of the agent's orders as the agent knows it (from its own requests and reports).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MyOrder {
    /// Instrument.
    pub instrument: InstrumentId,
    /// Side.
    pub side: Side,
    /// Limit price (`None` for market orders).
    pub price: Option<Price>,
    /// Open quantity as last reported (or as sent, before the acknowledgement).
    pub leaves: Qty,
    /// Quantity filled so far.
    pub filled: Qty,
    /// Exchange order id, once acknowledged.
    pub order_id: Option<OrderId>,
    /// Whether the exchange acknowledged the order.
    pub acked: bool,
    /// A cancel was sent and has not been answered yet.
    pub cancel_pending: bool,
    /// When the agent sent the order (its own clock, before any latency).
    pub sent_at: Timestamp,
}

/// The agent's own, delayed view of its orders, positions and cash.
#[derive(Clone, Debug, Default)]
pub struct MyOrders {
    live: BTreeMap<ClientOrderId, MyOrder>,
    position: Vec<Qty>,
    cash: Vec<i128>,
    fills: u64,
}

impl MyOrders {
    pub(crate) fn new(instruments: usize) -> Self {
        MyOrders { live: BTreeMap::new(), position: vec![0; instruments], cash: vec![0; instruments], fills: 0 }
    }

    pub(crate) fn on_send(&mut self, req: &Request, now: Timestamp) {
        match *req {
            Request::New(o) => {
                self.live.entry(o.client_id).or_insert(MyOrder {
                    instrument: o.instrument,
                    side: o.side,
                    price: o.price,
                    leaves: o.qty,
                    filled: 0,
                    order_id: None,
                    acked: false,
                    cancel_pending: false,
                    sent_at: now,
                });
            }
            Request::Cancel { client_id, .. } => {
                if let Some(o) = self.live.get_mut(&client_id) {
                    o.cancel_pending = true;
                }
            }
            Request::CancelAll { instrument } => {
                for o in self.live.values_mut().filter(|o| o.instrument == instrument) {
                    o.cancel_pending = true;
                }
            }
            Request::Modify { .. } => {}
        }
    }

    pub(crate) fn on_report(&mut self, r: &ExecReport) {
        match r.kind {
            ReportKind::Accepted { .. } => {
                if let Some(o) = self.live.get_mut(&r.client_id) {
                    o.acked = true;
                    o.order_id = r.order_id;
                }
            }
            ReportKind::Rejected { .. } => {
                self.live.remove(&r.client_id);
            }
            ReportKind::GatewayRejected { request: RequestType::New, reason } => {
                // A duplicate id refers to an order that is still live: keep it.
                if reason != GatewayReject::DuplicateClientOrderId {
                    self.live.remove(&r.client_id);
                }
            }
            ReportKind::GatewayRejected { request: RequestType::Cancel, .. }
            | ReportKind::RequestRejected { request: RequestKind::Cancel, .. } => {
                if let Some(o) = self.live.get_mut(&r.client_id) {
                    o.cancel_pending = false;
                }
            }
            ReportKind::GatewayRejected { request: RequestType::CancelAll, .. } => {
                for o in self.live.values_mut().filter(|o| o.instrument == r.instrument) {
                    o.cancel_pending = false;
                }
            }
            ReportKind::GatewayRejected { .. } | ReportKind::RequestRejected { .. } => {}
            ReportKind::Fill { side, price, qty, leaves, .. } => {
                let i = r.instrument as usize;
                if let (Some(p), Some(c)) = (self.position.get_mut(i), self.cash.get_mut(i)) {
                    let notional = i128::from(price) * i128::from(qty);
                    match side {
                        Side::Buy => {
                            *p += qty;
                            *c -= notional;
                        }
                        Side::Sell => {
                            *p -= qty;
                            *c += notional;
                        }
                    }
                }
                self.fills += 1;
                if let Some(o) = self.live.get_mut(&r.client_id) {
                    o.leaves = leaves;
                    o.filled += qty;
                    if leaves == 0 {
                        self.live.remove(&r.client_id);
                    }
                }
            }
            ReportKind::Cancelled { .. } => {
                self.live.remove(&r.client_id);
            }
            ReportKind::Replaced { price, qty, .. } => {
                if let Some(o) = self.live.get_mut(&r.client_id) {
                    o.leaves = qty;
                    o.price = Some(price);
                }
            }
        }
    }

    /// One order.
    pub fn get(&self, client_id: ClientOrderId) -> Option<&MyOrder> {
        self.live.get(&client_id)
    }

    /// Live orders (sent and not known to be finished), in client-id order.
    pub fn live(&self) -> impl Iterator<Item = (ClientOrderId, &MyOrder)> {
        self.live.iter().map(|(&c, o)| (c, o))
    }

    /// Number of live orders on an instrument.
    pub fn live_count(&self, instrument: InstrumentId) -> usize {
        self.live.values().filter(|o| o.instrument == instrument).count()
    }

    /// Position on an instrument, from fills reported so far.
    pub fn position(&self, instrument: InstrumentId) -> Qty {
        self.position.get(instrument as usize).copied().unwrap_or(0)
    }

    /// Cash from trading on an instrument (price units × quantity), from fills reported so far.
    pub fn cash(&self, instrument: InstrumentId) -> i128 {
        self.cash.get(instrument as usize).copied().unwrap_or(0)
    }

    /// Number of fills reported so far.
    pub fn fills(&self) -> u64 {
        self.fills
    }
}

/// Kernel-owned per-agent state.
pub(crate) struct AgentState {
    pub id: AgentId,
    pub rng: Rng,
    pub compute_rng: Rng,
    pub compute: LatencyModel,
    pub order_out: Link,
    pub order_in: Link,
    /// Feed through which the agent sees each instrument (`None` if not subscribed).
    pub views: Vec<Option<FeedId>>,
    pub orders: MyOrders,
    pub outbox: Vec<Request>,
    pub timers: Vec<(Timestamp, u64)>,
    pub next_client_id: ClientOrderId,
}

/// What an agent can see and do inside a callback.
pub struct Ctx<'a> {
    pub(crate) now: Timestamp,
    pub(crate) state: &'a mut AgentState,
    pub(crate) specs: &'a [InstrumentSpec],
    pub(crate) feeds: &'a [Feed],
}

impl Ctx<'_> {
    /// Current simulation time (when the information arrived).
    pub fn now(&self) -> Timestamp {
        self.now
    }

    /// This agent's id.
    pub fn id(&self) -> AgentId {
        self.state.id
    }

    /// The agent's own random stream.
    pub fn rng(&mut self) -> &mut Rng {
        &mut self.state.rng
    }

    /// The agent's view of an instrument's book, built from its delayed feed (private or shared
    /// with its feed group); `None` if not subscribed.
    pub fn book(&self, instrument: InstrumentId) -> Option<&MboBook> {
        let feed = (*self.state.views.get(instrument as usize)?)?;
        Some(&self.feeds[feed as usize].book)
    }

    /// Trading rules of an instrument.
    pub fn spec(&self, instrument: InstrumentId) -> Option<&InstrumentSpec> {
        self.specs.get(instrument as usize)
    }

    /// The agent's view of its own orders and positions.
    pub fn orders(&self) -> &MyOrders {
        &self.state.orders
    }

    /// A fresh client order id (1, 2, 3, ...).
    pub fn next_client_id(&mut self) -> ClientOrderId {
        let c = self.state.next_client_id;
        self.state.next_client_id += 1;
        c
    }

    /// Send a request. It leaves after the agent's compute delay, in send order.
    pub fn send(&mut self, req: Request) {
        self.state.orders.on_send(&req, self.now);
        self.state.outbox.push(req);
    }

    /// Call `on_timer(token)` at `at` (or now, if `at` is in the past).
    pub fn set_timer(&mut self, at: Timestamp, token: u64) {
        self.state.timers.push((at, token));
    }
}
