//! Shared test harness. Every command is run through the optimised engine AND the naive
//! reference model; after each command the harness checks that:
//!
//! * both produce identical results and identical event streams;
//! * both hold the same book (every order, in priority order);
//! * the book rebuilt from the public market-by-order feed alone equals the engine's book;
//! * the engine's structural invariants hold;
//! * private execution reports are self-consistent: sequence numbers are contiguous, every
//!   fill/cancel/replace refers to an open order, open quantity is conserved, every trade
//!   is followed by its two fills, and the orders open according to the reports are
//!   exactly the orders resting in the book with the same open quantity.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use lob::{
    Command, EngineError, Event, EventKind, InstrumentSpec, L3Entry, Liquidity, MatchingEngine,
    MboBook, NewOrder, OrderId, OrderSnapshot, OwnerId, Phase, Price, Qty, ReferenceEngine, Side,
    Timestamp,
};

macro_rules! ensure {
    ($cond:expr, $($fmt:tt)*) => {
        if !$cond {
            return Err(format!($($fmt)*));
        }
    };
}

/// Plain instrument for scenario tests: tick 1, lot 1, reference price 100.
pub fn spec() -> InstrumentSpec {
    InstrumentSpec {
        tick_size: 1,
        lot_size: 1,
        min_price: 1,
        max_price: 100_000,
        max_order_qty: 1_000_000,
        reference_price: 100,
    }
}

/// Event-sourced view of every order's open quantity, rebuilt from private reports only.
#[derive(Default)]
pub struct Tracker {
    open: HashMap<OrderId, Qty>,
    done: HashSet<OrderId>,
    last_seq: u64,
}

impl Tracker {
    pub fn apply(&mut self, events: &[Event]) -> Result<(), String> {
        for (i, e) in events.iter().enumerate() {
            ensure!(e.seq == self.last_seq + 1, "sequence gap before {e:?}");
            self.last_seq = e.seq;
            match e.kind {
                EventKind::Accepted { order_id, qty, .. } => {
                    ensure!(
                        !self.open.contains_key(&order_id) && !self.done.contains(&order_id),
                        "order id {order_id} accepted twice"
                    );
                    self.open.insert(order_id, qty);
                }
                EventKind::Rejected { order_id, .. } => {
                    ensure!(
                        !self.open.contains_key(&order_id) && !self.done.contains(&order_id),
                        "order id {order_id} reused"
                    );
                    self.done.insert(order_id);
                }
                EventKind::Fill { order_id, qty, leaves, .. } => {
                    let open = self
                        .open
                        .get_mut(&order_id)
                        .ok_or_else(|| format!("fill for order {order_id}, which is not open"))?;
                    ensure!(qty > 0 && *open - qty == leaves, "fill {e:?} inconsistent with open qty {open}");
                    *open = leaves;
                    if leaves == 0 {
                        self.open.remove(&order_id);
                        self.done.insert(order_id);
                    }
                }
                EventKind::Cancelled { order_id, qty, .. } => {
                    let open = self
                        .open
                        .remove(&order_id)
                        .ok_or_else(|| format!("cancel of order {order_id}, which is not open"))?;
                    ensure!(open == qty, "cancel {e:?} but open qty was {open}");
                    self.done.insert(order_id);
                }
                EventKind::Replaced { order_id, qty, .. } => {
                    let open = self
                        .open
                        .get_mut(&order_id)
                        .ok_or_else(|| format!("replace of order {order_id}, which is not open"))?;
                    ensure!(qty > 0, "replace {e:?} to a non-positive open qty");
                    *open = qty;
                }
                EventKind::Trade { trade_id, price, qty, aggressor, buy_order_id, sell_order_id } => {
                    ensure!(qty > 0, "empty trade {e:?}");
                    let (first, second, liq) = match aggressor {
                        Some(Side::Buy) => (sell_order_id, buy_order_id, [Liquidity::Maker, Liquidity::Taker]),
                        Some(Side::Sell) => (buy_order_id, sell_order_id, [Liquidity::Maker, Liquidity::Taker]),
                        None => (buy_order_id, sell_order_id, [Liquidity::Auction, Liquidity::Auction]),
                    };
                    for (k, id) in [(1usize, first), (2, second)] {
                        match events.get(i + k).map(|e| e.kind) {
                            Some(EventKind::Fill { order_id, trade_id: t, price: p, qty: q, liquidity, .. })
                                if order_id == id && t == trade_id && p == price && q == qty && liquidity == liq[k - 1] => {}
                            other => return Err(format!("trade {trade_id} not followed by its fill #{k}: {other:?}")),
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn check(&self, snapshot: &[OrderSnapshot]) -> Result<(), String> {
        ensure!(
            self.open.len() == snapshot.len(),
            "{} orders open according to execution reports, {} resting in the book",
            self.open.len(),
            snapshot.len()
        );
        for o in snapshot {
            ensure!(
                self.open.get(&o.order_id) == Some(&o.leaves),
                "order {}: reports say {:?} open, book says {}",
                o.order_id,
                self.open.get(&o.order_id),
                o.leaves
            );
        }
        Ok(())
    }
}

/// Every cross-check, after one command.
pub fn check_step(
    eng: &MatchingEngine,
    refm: &ReferenceEngine,
    mbo: &mut MboBook,
    tracker: &mut Tracker,
    events: &[Event],
    compare_auction: bool,
) -> Result<(), String> {
    for e in events {
        mbo.apply(e).map_err(|err| format!("public feed inconsistent at {e:?}: {err:?}"))?;
    }
    tracker.apply(events)?;
    eng.check_invariants()?;
    let snap = eng.snapshot();
    let rsnap = refm.snapshot();
    ensure!(snap == rsnap, "book differs from reference\n engine:    {snap:?}\n reference: {rsnap:?}");
    let projected: Vec<L3Entry> = snap
        .iter()
        .map(|o| L3Entry { order_id: o.order_id, side: o.side, price: o.price, qty: o.leaves })
        .collect();
    let rebuilt = mbo.l3();
    ensure!(
        projected == rebuilt,
        "book rebuilt from the public feed differs\n engine: {projected:?}\n feed:   {rebuilt:?}"
    );
    ensure!(
        eng.best_bid() == mbo.best_bid() && eng.best_ask() == mbo.best_ask(),
        "best bid/ask differ between engine and feed"
    );
    ensure!(
        eng.best_bid() == refm.best_bid() && eng.best_ask() == refm.best_ask(),
        "best bid/ask differ between engine and reference"
    );
    ensure!(eng.phase() == refm.phase(), "phase differs");
    ensure!(eng.reference_price() == refm.reference_price(), "reference price differs");
    if compare_auction {
        let (a, b) = (eng.indicative_auction(), refm.indicative_auction());
        ensure!(a == b, "indicative auction differs: engine {a:?}, reference {b:?}");
    }
    tracker.check(&snap)
}

/// Scenario-test harness.
pub struct H {
    pub eng: MatchingEngine,
    pub refm: ReferenceEngine,
    pub mbo: MboBook,
    pub tracker: Tracker,
    pub ts: Timestamp,
}

impl H {
    pub fn new() -> Self {
        Self::with(spec(), Phase::Continuous)
    }

    pub fn with(spec: InstrumentSpec, phase: Phase) -> Self {
        H {
            eng: MatchingEngine::new(spec, phase).expect("valid spec"),
            refm: ReferenceEngine::new(spec, phase).expect("valid spec"),
            mbo: MboBook::new(),
            tracker: Tracker::default(),
            ts: 0,
        }
    }

    /// Run a command at the next timestamp; panics if the engine refuses it.
    pub fn run(&mut self, cmd: Command) -> Vec<EventKind> {
        self.ts += 10;
        let ts = self.ts;
        self.try_run(ts, cmd).unwrap_or_else(|e| panic!("{cmd:?} refused: {e}"))
    }

    /// Run a command at `ts` through both implementations and every cross-check.
    pub fn try_run(&mut self, ts: Timestamp, cmd: Command) -> Result<Vec<EventKind>, EngineError> {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let ra = self.eng.process(ts, &cmd, &mut a);
        let rb = self.refm.process(ts, &cmd, &mut b);
        assert_eq!(ra, rb, "engine and reference disagree on the result of {cmd:?}");
        assert_eq!(a, b, "engine and reference disagree on the events of {cmd:?}");
        check_step(&self.eng, &self.refm, &mut self.mbo, &mut self.tracker, &a, true)
            .unwrap_or_else(|e| panic!("after {cmd:?}: {e}"));
        ra.map(|()| a.into_iter().map(|e| e.kind).collect())
    }

    pub fn submit(&mut self, o: NewOrder) -> (OrderId, Vec<EventKind>) {
        let id = self.eng.next_order_id();
        (id, self.run(Command::New(o)))
    }

    pub fn buy(&mut self, owner: OwnerId, price: Price, qty: Qty) -> OrderId {
        self.submit(NewOrder::limit(owner, Side::Buy, price, qty)).0
    }

    pub fn sell(&mut self, owner: OwnerId, price: Price, qty: Qty) -> OrderId {
        self.submit(NewOrder::limit(owner, Side::Sell, price, qty)).0
    }

    pub fn cancel(&mut self, owner: OwnerId, order_id: OrderId) -> Vec<EventKind> {
        self.run(Command::Cancel { owner, order_id })
    }

    pub fn modify(&mut self, owner: OwnerId, order_id: OrderId, price: Price, qty: Qty) -> Vec<EventKind> {
        self.run(Command::Modify { owner, order_id, price, qty })
    }

    pub fn phase(&mut self, phase: Phase) -> Vec<EventKind> {
        self.run(Command::SetPhase { phase })
    }
}

/// `(price, qty, buy order, sell order)` of every trade print.
pub fn trades(events: &[EventKind]) -> Vec<(Price, Qty, OrderId, OrderId)> {
    events
        .iter()
        .filter_map(|e| match *e {
            EventKind::Trade { price, qty, buy_order_id, sell_order_id, .. } => {
                Some((price, qty, buy_order_id, sell_order_id))
            }
            _ => None,
        })
        .collect()
}
