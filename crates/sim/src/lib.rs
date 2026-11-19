//! # sim: deterministic discrete-event market simulator
//!
//! The simulation kernel of `hft-sim`, built on the `lob` matching engine. Every message
//! between a trading agent and the exchange takes an explicit, sampled amount of time, so
//! **no agent ever sees the true order book**. Agents only see their own execution reports and a
//! delayed market-data feed, from which the kernel maintains each agent's own book view.
//!
//! * One scheduler ordered by `(time, insertion sequence)`: simultaneous events always resolve
//!   the same way.
//! * Per-path latency models ([`LatencyModel`]: constant, uniform, log-normal, Pareto,
//!   spikes) on order-preserving [`Link`]s, each with its own random stream.
//! * Gateway sessions with throttles and pre-trade risk checks ([`RiskLimits`]) that map client
//!   order ids to exchange ids; matching engines that are FIFO servers with service times, so
//!   bursts queue.
//! * Exchange-side ground-truth accounting in integers ([`Ledger`]).
//! * Session schedules with random auction ends ([`SessionEvent`]) and signal sources
//!   ([`SignalProcess`]) that reach each subscriber with its own delay.
//! * Bit-reproducible: all randomness comes from per-entity streams of one master seed, and
//!   distributions are sampled through [`detmath`] (no platform math library), so a seed gives
//!   the same run on any machine. [`Simulation::digest`] fingerprints a run.
//!
//! ```
//! use lob::{InstrumentSpec, Side};
//! use sim::{AgentConfig, InstrumentConfig, LatencyProfile, OrderSpec, Request, SimBuilder};
//! use sim::agents::Scripted;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut b = SimBuilder::new(42);
//! let i = b.add_instrument(InstrumentConfig::new(InstrumentSpec::example()));
//! let order = Request::New(OrderSpec::limit(i, 1, Side::Buy, 9_999, 100));
//! let trader = b.add_agent(Scripted::new(vec![(1_000, order)]), AgentConfig::new(LatencyProfile::colocated()));
//! let mut sim = b.build()?;
//! sim.run_until(1_000_000);
//! assert_eq!(sim.engine(i).best_bid(), Some((9_999, 100)));
//! let acked_at = sim.agent::<Scripted>(trader).unwrap().reports[0].0;
//! assert!(acked_at > 1_000); // the acknowledgement took a round trip
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod accounting;
pub mod agent;
pub mod agents;
pub mod detmath;
pub mod feed;
pub mod gateway;
pub mod latency;
pub mod messages;
mod queue;
pub mod session;
pub mod signal;
mod simulation;

pub use accounting::{FEE_SCALE, FeeSchedule, Ledger};
pub use agent::{Agent, Ctx, MyOrder, MyOrders};
pub use feed::{FeedGroupId, FeedMode, Subscription};
pub use gateway::{RiskLimits, Throttle};
pub use latency::{LatencyModel, LatencyProfile, Link};
pub use messages::{
    AgentId, ClientOrderId, ExecReport, GatewayReject, InstrumentId, OrderSpec, ReportKind, Request,
    RequestType, SignalId,
};
pub use session::{SessionEvent, SessionTime};
pub use signal::SignalProcess;
pub use simulation::{
    AgentConfig, BuildError, ExchangeTiming, GatewayRecord, InstrumentConfig, SimBuilder, SimStats, Simulation,
};
