//! Order book reconstruction from the public market-by-order (MBO) feed.
//!
//! This is what a participant's feed handler does with an ITCH-style feed, and what
//! historical MBO data will be replayed through. It sees only public events
//! ([`EventKind::is_public`]) and never touches engine internals, so a test that compares
//! its book with the engine's after every command proves the public feed is complete.
//!
//! In the simulator each agent will own an `MboBook` fed through its own latency
//! profile, so its view of the book is stale by construction.

use std::collections::BTreeMap;

use crate::book::IdMap;
use crate::event::{Event, EventKind};
use crate::types::{LevelSnapshot, OrderId, Phase, Price, Qty, Side};

/// Inconsistency found while applying the feed (a gap or a corrupted stream).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MboError {
    /// `BookAdd` for an id that is already in the book.
    DuplicateOrder(OrderId),
    /// Update, removal or execution of an id that is not in the book.
    UnknownOrder(OrderId),
    /// Execution larger than the order's open quantity.
    Overfill {
        /// Order executed.
        order_id: OrderId,
        /// Executed quantity.
        qty: Qty,
        /// Open quantity before the execution.
        leaves: Qty,
    },
    /// `BookUpdate` that does not reduce the open quantity to a positive value.
    InvalidUpdate {
        /// Order updated.
        order_id: OrderId,
        /// Proposed open quantity.
        qty: Qty,
    },
}

/// One order in a level-3 snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct L3Entry {
    /// Order id.
    pub order_id: OrderId,
    /// Side.
    pub side: Side,
    /// Limit price, `None` for a market order resting in an auction call phase.
    pub price: Option<Price>,
    /// Open quantity.
    pub qty: Qty,
}

#[derive(Clone, Copy, Debug)]
struct MOrder {
    side: Side,
    price: Option<Price>,
    qty: Qty,
    arrival: u64,
}

/// Book rebuilt from the public feed.
#[derive(Clone, Debug, Default)]
pub struct MboBook {
    orders: IdMap<MOrder>,
    levels: [BTreeMap<Price, (Qty, u32)>; 2],
    market_qty: [Qty; 2],
    arrival: u64,
    phase: Option<Phase>,
    last_trade: Option<(Price, Qty)>,
    trades: u64,
    volume: Qty,
}

impl MboBook {
    /// Empty book.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one event. Private execution reports are ignored.
    pub fn apply(&mut self, e: &Event) -> Result<(), MboError> {
        match e.kind {
            EventKind::BookAdd { order_id, side, price, qty } => self.add(order_id, side, price, qty),
            EventKind::BookUpdate { order_id, qty } => self.update(order_id, qty),
            EventKind::BookRemove { order_id } => self.remove(order_id),
            EventKind::Trade { price, qty, aggressor, buy_order_id, sell_order_id, .. } => {
                match aggressor {
                    Some(Side::Buy) => self.execute(sell_order_id, qty)?,
                    Some(Side::Sell) => self.execute(buy_order_id, qty)?,
                    None => {
                        self.execute(buy_order_id, qty)?;
                        self.execute(sell_order_id, qty)?;
                    }
                }
                self.last_trade = Some((price, qty));
                self.trades += 1;
                self.volume += qty;
                Ok(())
            }
            EventKind::PhaseChanged { phase } => {
                self.phase = Some(phase);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn level_delta(&mut self, side: Side, price: Option<Price>, dq: Qty, dc: i64) {
        match price {
            None => self.market_qty[side.idx()] += dq,
            Some(p) => {
                let lvl = self.levels[side.idx()].entry(p).or_insert((0, 0));
                lvl.0 += dq;
                lvl.1 = (i64::from(lvl.1) + dc) as u32;
                if lvl.1 == 0 {
                    self.levels[side.idx()].remove(&p);
                }
            }
        }
    }

    fn add(&mut self, id: OrderId, side: Side, price: Option<Price>, qty: Qty) -> Result<(), MboError> {
        if self.orders.contains_key(&id) {
            return Err(MboError::DuplicateOrder(id));
        }
        self.arrival += 1;
        self.orders.insert(id, MOrder { side, price, qty, arrival: self.arrival });
        self.level_delta(side, price, qty, 1);
        Ok(())
    }

    fn update(&mut self, id: OrderId, qty: Qty) -> Result<(), MboError> {
        let o = self.orders.get_mut(&id).ok_or(MboError::UnknownOrder(id))?;
        if qty <= 0 || qty > o.qty {
            return Err(MboError::InvalidUpdate { order_id: id, qty });
        }
        let (side, price, dq) = (o.side, o.price, qty - o.qty);
        o.qty = qty;
        self.level_delta(side, price, dq, 0);
        Ok(())
    }

    fn remove(&mut self, id: OrderId) -> Result<(), MboError> {
        let o = self.orders.remove(&id).ok_or(MboError::UnknownOrder(id))?;
        self.level_delta(o.side, o.price, -o.qty, -1);
        Ok(())
    }

    fn execute(&mut self, id: OrderId, qty: Qty) -> Result<(), MboError> {
        let o = self.orders.get_mut(&id).ok_or(MboError::UnknownOrder(id))?;
        if qty > o.qty {
            return Err(MboError::Overfill { order_id: id, qty, leaves: o.qty });
        }
        o.qty -= qty;
        let (side, price, done) = (o.side, o.price, o.qty == 0);
        if done {
            self.orders.remove(&id);
        }
        self.level_delta(side, price, -qty, if done { -1 } else { 0 });
        Ok(())
    }

    /// Best bid and its total quantity.
    pub fn best_bid(&self) -> Option<(Price, Qty)> {
        self.levels[Side::Buy.idx()].iter().next_back().map(|(&p, &(q, _))| (p, q))
    }

    /// Best ask and its total quantity.
    pub fn best_ask(&self) -> Option<(Price, Qty)> {
        self.levels[Side::Sell.idx()].iter().next().map(|(&p, &(q, _))| (p, q))
    }

    /// Best ask minus best bid.
    pub fn spread(&self) -> Option<Price> {
        Some(self.best_ask()?.0 - self.best_bid()?.0)
    }

    /// Up to `n` aggregated levels of `side`, best first.
    pub fn depth(&self, side: Side, n: usize) -> Vec<LevelSnapshot> {
        let to_snap = |(&price, &(qty, count)): (&Price, &(Qty, u32))| LevelSnapshot { price, qty, count };
        match side {
            Side::Buy => self.levels[0].iter().rev().take(n).map(to_snap).collect(),
            Side::Sell => self.levels[1].iter().take(n).map(to_snap).collect(),
        }
    }

    /// Market-order quantity resting on `side` (auction call phase only).
    pub fn market_qty(&self, side: Side) -> Qty {
        self.market_qty[side.idx()]
    }

    /// Quantity queued ahead of `id` at its price level (by feed arrival order).
    pub fn queue_ahead(&self, id: OrderId) -> Option<Qty> {
        let me = self.orders.get(&id)?;
        Some(
            self.orders
                .values()
                .filter(|o| o.side == me.side && o.price == me.price && o.arrival < me.arrival)
                .map(|o| o.qty)
                .sum(),
        )
    }

    /// Every resting order in priority order (buys then sells; market orders first, then
    /// better prices, then earlier arrival).
    pub fn l3(&self) -> Vec<L3Entry> {
        let mut v: Vec<(OrderId, MOrder)> = self.orders.iter().map(|(&id, &o)| (id, o)).collect();
        v.sort_by_key(|&(_, o)| {
            let price_key = match (o.side, o.price) {
                (_, None) => 0,
                (Side::Buy, Some(p)) => -p,
                (Side::Sell, Some(p)) => p,
            };
            (o.side, o.price.is_some(), price_key, o.arrival)
        });
        v.into_iter()
            .map(|(order_id, o)| L3Entry { order_id, side: o.side, price: o.price, qty: o.qty })
            .collect()
    }

    /// Number of resting orders.
    pub fn order_count(&self) -> usize {
        self.orders.len()
    }

    /// Last phase announced on the feed.
    pub fn phase(&self) -> Option<Phase> {
        self.phase
    }

    /// Last trade price and quantity.
    pub fn last_trade(&self) -> Option<(Price, Qty)> {
        self.last_trade
    }

    /// Number of trades and total volume seen on the feed.
    pub fn trade_stats(&self) -> (u64, Qty) {
        (self.trades, self.volume)
    }
}
