//! Trading-session schedule: opening and closing auctions, halts, reopenings.
//!
//! Each event changes one instrument's phase. Its time can be fixed or drawn uniformly from a
//! window, so the end of an auction cannot be predicted to the nanosecond (Xetra volatility
//! auctions have a random end for exactly that reason). Draws come from a per-event random
//! stream and are fixed when the simulation is built.

use lob::{Phase, Timestamp};

use crate::messages::InstrumentId;

/// When a session event happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SessionTime {
    /// At this time.
    At(Timestamp),
    /// At a uniformly random nanosecond in `[from, to]`.
    Between(Timestamp, Timestamp),
}

/// A scheduled phase change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionEvent {
    /// Instrument.
    pub instrument: InstrumentId,
    /// Phase to switch to (leaving an auction for continuous or closed runs the uncross).
    pub phase: Phase,
    /// When.
    pub at: SessionTime,
}

impl SessionEvent {
    /// Phase change at a fixed time.
    pub fn at(instrument: InstrumentId, phase: Phase, t: Timestamp) -> Self {
        SessionEvent { instrument, phase, at: SessionTime::At(t) }
    }

    /// Phase change at a random time in `[from, to]`.
    pub fn between(instrument: InstrumentId, phase: Phase, from: Timestamp, to: Timestamp) -> Self {
        SessionEvent { instrument, phase, at: SessionTime::Between(from, to) }
    }
}
