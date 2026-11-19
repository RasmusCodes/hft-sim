//! # lob: deterministic limit order book and matching engine
//!
//! The exchange core of `hft-sim`. A [`MatchingEngine`] is a pure state machine
//! `(timestamp, Command) -> events`: it performs no I/O, reads no clock and uses no
//! randomness, so a given command sequence always produces the same event sequence,
//! bit for bit.
//!
//! Implemented (rulebook: `docs/MATCHING_RULES.md`):
//! * continuous matching in price-time priority: limit and market orders, GTC / IOC /
//!   FOK, post-only, partial fills, multi-level sweeps, trade prints;
//! * cancel, cancel/replace with exchange-style priority rules (FIX total-quantity
//!   semantics), mass cancel;
//! * tick-size, lot-size, price-bound and maximum-quantity validation;
//! * trading phases: continuous, call auction with single-price uncross, halt, closed;
//! * self-trade prevention (cancel resting / cancel aggressor / cancel both);
//! * an anonymous market-by-order feed in the event stream, from which
//!   [`MboBook`] rebuilds the book exactly as a participant's feed handler would.
//!
//! [`ReferenceEngine`] is a deliberately naive second implementation of the same rules,
//! used only as a test oracle.
//!
//! ```
//! use lob::{Command, EventKind, InstrumentSpec, MatchingEngine, NewOrder, Phase, Side, TimeInForce};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut eng = MatchingEngine::new(InstrumentSpec::example(), Phase::Continuous)?;
//! let mut events = Vec::new();
//! eng.process(1_000, &Command::New(NewOrder::limit(1, Side::Sell, 10_001, 300)), &mut events)?;
//! let ioc_buy = NewOrder::limit(2, Side::Buy, 10_002, 100).with_tif(TimeInForce::Ioc);
//! eng.process(2_000, &Command::New(ioc_buy), &mut events)?;
//! // Accepted, BookAdd, Accepted, Trade, Fill (maker), Fill (taker)
//! assert_eq!(events.len(), 6);
//! assert!(matches!(events[3].kind, EventKind::Trade { price: 10_001, qty: 100, .. }));
//! assert_eq!(eng.best_ask(), Some((10_001, 200)));
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod auction;
mod book;
pub mod engine;
pub mod event;
pub mod instrument;
pub mod mbo;
pub mod reference;
pub mod rng;
pub mod types;
pub mod workload;

pub use auction::AuctionQuote;
pub use book::IdHasher;
pub use engine::{EngineError, MatchingEngine};

/// `HashMap` with the deterministic [`IdHasher`] (no per-process random seed).
pub type DetHashMap<K, V> = std::collections::HashMap<K, V, std::hash::BuildHasherDefault<IdHasher>>;
pub use event::{CancelReason, Digest, Event, EventKind, RejectReason, RequestKind};
pub use instrument::{InstrumentSpec, SpecError};
pub use mbo::{L3Entry, MboBook, MboError};
pub use reference::ReferenceEngine;
pub use types::{
    Command, LevelSnapshot, Liquidity, NewOrder, OrderId, OrderSnapshot, OwnerId, Phase, Price,
    Qty, SeqNo, Side, StpMode, TimeInForce, Timestamp, TradeId,
};
