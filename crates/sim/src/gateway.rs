//! Exchange gateway: one session per agent.
//!
//! The gateway sits between an agent's order-entry link and the matching engine. It
//! * throttles messages (token bucket) and applies pre-trade risk checks;
//! * maps the agent's client order ids to exchange order ids. Ids are assigned when the
//!   order is forwarded: the gateway → engine hop is a constant, so the engine receives
//!   messages in forwarding order and the ids agree with the engine's (checked by tests). A
//!   cancel can therefore follow its order immediately, before the acknowledgement returns;
//! * turns the engine's private events into execution reports keyed by client order id;
//! * keeps the exchange's ground-truth ledger of each participant's fills, positions and fees.

use lob::{
    Command, DetHashMap, Event, EventKind, NewOrder, OrderId, Qty, RejectReason, RequestKind, Side,
    Timestamp,
};

use crate::accounting::{FeeSchedule, Ledger};
use crate::messages::{
    AgentId, ClientOrderId, ExecReport, GatewayReject, InstrumentId, ReportKind, Request,
};

/// Message-rate limit: a token bucket of `burst` messages refilled at `per_second`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Throttle {
    /// Bucket size (messages that may be sent back to back).
    pub burst: u32,
    /// Sustained rate.
    pub per_second: u32,
}

/// Pre-trade risk limits of one session. Position and open-order limits apply per instrument.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RiskLimits {
    /// Largest quantity of one order (also checked on modify).
    pub max_order_qty: Qty,
    /// Most open orders (resting or in flight).
    pub max_open_orders: u32,
    /// Largest absolute position the session could reach if all its open orders filled.
    pub max_position: Qty,
    /// Optional message-rate limit (counts every message, including rejected ones).
    pub throttle: Option<Throttle>,
}

impl Default for RiskLimits {
    /// No limits.
    fn default() -> Self {
        RiskLimits { max_order_qty: Qty::MAX, max_open_orders: u32::MAX, max_position: Qty::MAX, throttle: None }
    }
}

const TOKEN: i128 = 1_000_000_000;

#[derive(Clone, Debug)]
struct Bucket {
    capacity: i128,
    tokens: i128,
    per_second: i128,
    last: Timestamp,
}

impl Bucket {
    fn new(t: Throttle) -> Self {
        let capacity = i128::from(t.burst) * TOKEN;
        Bucket { capacity, tokens: capacity, per_second: i128::from(t.per_second), last: 0 }
    }

    /// Integer token bucket in nano-tokens: refills `per_second` nano-tokens per nanosecond.
    fn take(&mut self, now: Timestamp) -> bool {
        let elapsed = i128::from(now.saturating_sub(self.last).max(0));
        self.tokens = (self.tokens + elapsed * self.per_second).min(self.capacity);
        self.last = now;
        if self.tokens >= TOKEN {
            self.tokens -= TOKEN;
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct GwOrder {
    instrument: InstrumentId,
    order_id: OrderId,
    side: Side,
    order_qty: Qty,
    leaves: Qty,
}

#[derive(Clone, Copy, Debug, Default)]
struct Exposure {
    position: Qty,
    open_buy: Qty,
    open_sell: Qty,
    open_orders: u32,
}

/// Where an engine job came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Origin {
    pub agent: AgentId,
    pub client_id: ClientOrderId,
    /// The exchange order the request creates or targets.
    pub order_id: Option<OrderId>,
}

/// Result of gateway processing.
pub(crate) enum Outcome {
    /// Pass to the matching engine.
    Forward(Command, Origin),
    /// Answer the agent directly (throttle, risk, or unknown order).
    Reply(ExecReport),
}

/// One agent's gateway session.
pub(crate) struct Session {
    agent: AgentId,
    limits: RiskLimits,
    bucket: Option<Bucket>,
    /// The session processes one message at a time; it is busy until `free_at`.
    pub free_at: Timestamp,
    orders: DetHashMap<ClientOrderId, GwOrder>,
    by_order: DetHashMap<(InstrumentId, OrderId), ClientOrderId>,
    exposure: Vec<Exposure>,
    /// Ground-truth ledger per instrument.
    pub ledgers: Vec<Ledger>,
}

impl Session {
    pub fn new(agent: AgentId, limits: RiskLimits, instruments: usize) -> Self {
        Session {
            agent,
            limits,
            bucket: limits.throttle.map(Bucket::new),
            free_at: Timestamp::MIN,
            orders: DetHashMap::default(),
            by_order: DetHashMap::default(),
            exposure: vec![Exposure::default(); instruments],
            ledgers: vec![Ledger::default(); instruments],
        }
    }

    /// Open orders on an instrument as the gateway sees them (including in-flight ones).
    pub fn open_orders(&self, instrument: InstrumentId) -> u32 {
        self.exposure[instrument as usize].open_orders
    }

    fn live(&self, client_id: ClientOrderId, instrument: InstrumentId) -> Option<GwOrder> {
        self.orders.get(&client_id).copied().filter(|g| g.instrument == instrument)
    }

    fn breaches_position(&self, ex: Exposure, side: Side, extra: i128) -> bool {
        let max = i128::from(self.limits.max_position);
        let pos = i128::from(ex.position);
        match side {
            Side::Buy => pos + i128::from(ex.open_buy) + extra > max,
            Side::Sell => -(pos - i128::from(ex.open_sell) - extra) > max,
        }
    }

    /// Process one request at time `now`. `next_ids[i]` is the id the engine of instrument
    /// `i` will give the next new order it receives.
    pub fn handle(&mut self, now: Timestamp, req: &Request, next_ids: &mut [OrderId]) -> Outcome {
        let instrument = req.instrument();
        let client_id = req.client_id();
        let reply = |kind| Outcome::Reply(ExecReport { instrument, client_id, order_id: None, exchange_ts: now, kind });
        let refuse = |reason| reply(ReportKind::GatewayRejected { request: req.kind(), reason });

        if let Some(bucket) = self.bucket.as_mut() {
            if !bucket.take(now) {
                return refuse(GatewayReject::Throttled);
            }
        }
        let i = instrument as usize;
        if i >= self.exposure.len() {
            return refuse(GatewayReject::UnknownInstrument);
        }
        let agent = self.agent;
        match *req {
            Request::New(o) => {
                if self.orders.contains_key(&o.client_id) {
                    return refuse(GatewayReject::DuplicateClientOrderId);
                }
                if o.qty > self.limits.max_order_qty {
                    return refuse(GatewayReject::MaxOrderQty);
                }
                let ex = self.exposure[i];
                if ex.open_orders >= self.limits.max_open_orders {
                    return refuse(GatewayReject::MaxOpenOrders);
                }
                if self.breaches_position(ex, o.side, i128::from(o.qty.max(0))) {
                    return refuse(GatewayReject::MaxPosition);
                }
                let order_id = next_ids[i];
                next_ids[i] += 1;
                self.orders.insert(
                    o.client_id,
                    GwOrder { instrument, order_id, side: o.side, order_qty: o.qty, leaves: o.qty },
                );
                self.by_order.insert((instrument, order_id), o.client_id);
                let ex = &mut self.exposure[i];
                ex.open_orders += 1;
                match o.side {
                    Side::Buy => ex.open_buy += o.qty,
                    Side::Sell => ex.open_sell += o.qty,
                }
                let cmd = Command::New(NewOrder {
                    owner: agent,
                    client_tag: o.client_id,
                    side: o.side,
                    price: o.price,
                    qty: o.qty,
                    tif: o.tif,
                    post_only: o.post_only,
                    stp: o.stp,
                });
                Outcome::Forward(cmd, Origin { agent, client_id: o.client_id, order_id: Some(order_id) })
            }
            Request::Cancel { client_id, .. } => match self.live(client_id, instrument) {
                // Already done, or never existed: the same answer the engine would give.
                None => reply(ReportKind::RequestRejected { request: RequestKind::Cancel, reason: RejectReason::UnknownOrder }),
                Some(g) => Outcome::Forward(
                    Command::Cancel { owner: agent, order_id: g.order_id },
                    Origin { agent, client_id, order_id: Some(g.order_id) },
                ),
            },
            Request::Modify { client_id, price, qty, .. } => {
                let Some(g) = self.live(client_id, instrument) else {
                    return reply(ReportKind::RequestRejected { request: RequestKind::Modify, reason: RejectReason::UnknownOrder });
                };
                if qty > self.limits.max_order_qty {
                    return refuse(GatewayReject::MaxOrderQty);
                }
                let filled = g.order_qty - g.leaves;
                let increase = i128::from(qty) - i128::from(filled) - i128::from(g.leaves);
                if increase > 0 && self.breaches_position(self.exposure[i], g.side, increase) {
                    return refuse(GatewayReject::MaxPosition);
                }
                Outcome::Forward(
                    Command::Modify { owner: agent, order_id: g.order_id, price, qty },
                    Origin { agent, client_id, order_id: Some(g.order_id) },
                )
            }
            Request::CancelAll { .. } => {
                Outcome::Forward(Command::CancelAll { owner: agent }, Origin { agent, client_id: 0, order_id: None })
            }
        }
    }

    fn forget(&mut self, client_id: ClientOrderId) {
        if let Some(g) = self.orders.remove(&client_id) {
            self.by_order.remove(&(g.instrument, g.order_id));
            let ex = &mut self.exposure[g.instrument as usize];
            ex.open_orders -= 1;
            match g.side {
                Side::Buy => ex.open_buy -= g.leaves,
                Side::Sell => ex.open_sell -= g.leaves,
            }
        }
    }

    /// Translate one private engine event about this session's orders into an execution
    /// report, updating exposure and the ledger. `None` if the event is not about this
    /// session's orders (which would be a kernel bug; the caller records a warning).
    pub fn on_engine_event(
        &mut self,
        instrument: InstrumentId,
        ev: &Event,
        origin: Option<&Origin>,
        fees: &FeeSchedule,
    ) -> Option<ExecReport> {
        let order_id = match ev.kind {
            EventKind::Accepted { order_id, .. }
            | EventKind::Rejected { order_id, .. }
            | EventKind::Fill { order_id, .. }
            | EventKind::Cancelled { order_id, .. }
            | EventKind::Replaced { order_id, .. }
            | EventKind::RequestRejected { order_id, .. } => order_id,
            _ => return None,
        };
        // Live orders are in the map. A request about an order that finished in the meantime
        // ("too late to cancel") is resolved through the job's origin.
        let client_id = match self.by_order.get(&(instrument, order_id)) {
            Some(&c) => c,
            None => match origin {
                Some(o) if o.agent == self.agent && o.order_id == Some(order_id) => o.client_id,
                _ => return None,
            },
        };
        let i = instrument as usize;
        let kind = match ev.kind {
            EventKind::Accepted { side, price, qty, tif, .. } => ReportKind::Accepted { side, price, qty, tif },
            EventKind::Rejected { reason, .. } => {
                self.forget(client_id);
                ReportKind::Rejected { reason }
            }
            EventKind::Fill { trade_id, price, qty, leaves, liquidity, .. } => {
                let g = self.orders.get_mut(&client_id)?;
                let side = g.side;
                g.leaves = leaves;
                let ex = &mut self.exposure[i];
                match side {
                    Side::Buy => {
                        ex.open_buy -= qty;
                        ex.position += qty;
                    }
                    Side::Sell => {
                        ex.open_sell -= qty;
                        ex.position -= qty;
                    }
                }
                self.ledgers[i].on_fill(side, price, qty, liquidity, fees);
                if leaves == 0 {
                    self.forget(client_id);
                }
                ReportKind::Fill { trade_id, side, price, qty, leaves, liquidity }
            }
            EventKind::Cancelled { qty, reason, .. } => {
                self.forget(client_id);
                ReportKind::Cancelled { qty, reason }
            }
            EventKind::Replaced { price, qty, priority_kept, .. } => {
                if let Some(g) = self.orders.get_mut(&client_id) {
                    let filled = g.order_qty - g.leaves;
                    let delta = qty - g.leaves;
                    g.leaves = qty;
                    g.order_qty = filled + qty;
                    let side = g.side;
                    let ex = &mut self.exposure[i];
                    match side {
                        Side::Buy => ex.open_buy += delta,
                        Side::Sell => ex.open_sell += delta,
                    }
                }
                ReportKind::Replaced { price, qty, priority_kept }
            }
            EventKind::RequestRejected { request, reason, .. } => ReportKind::RequestRejected { request, reason },
            _ => return None,
        };
        Some(ExecReport { instrument, client_id, order_id: Some(order_id), exchange_ts: ev.ts, kind })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_allows_bursts_then_refills() {
        let mut b = Bucket::new(Throttle { burst: 3, per_second: 1_000 });
        assert!(b.take(0) && b.take(0) && b.take(0));
        assert!(!b.take(0));
        assert!(!b.take(999_999)); // 0.999 of a token after 999,999 ns
        assert!(b.take(1_000_000)); // one token per millisecond
        assert!(!b.take(1_000_000));
    }
}
