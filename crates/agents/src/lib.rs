//! # agents: trading agents for the hft-sim market simulator
//!
//! Every agent implements [`sim::Agent`]. It sees only its own delayed book view, its own
//! execution reports and its own delayed (optionally noisy) signals, and acts only by sending
//! requests through its latency path and gateway. The catalogue (docs/AGENTS.md):
//!
//! | Agent | Behaviour |
//! |---|---|
//! | [`NoiseTrader`] | zero-intelligence limit orders, protected marketable orders and cancels |
//! | [`InformedTrader`] | takes liquidity when its value estimate beats the quotes |
//! | [`MomentumTrader`] | follows fast-versus-slow moving-average trends, marketable orders |
//! | [`MeanReversionTrader`] | fades z-score deviations with passive quotes |
//! | [`FundamentalTrader`] | patient value investor, passive orders toward a mispricing target |
//! | [`PairsTrader`] | statistical arbitrage on the spread between two instruments |
//! | [`ExecutionAgent`] | institutional parent orders: TWAP, VWAP, Almgren–Chriss, POV |
//! | [`LatencyArbitrageur`] | snipes stale quotes after value jumps |
//!
//! Configurable everywhere: capital and risk limits ([`Limits`], plus the gateway's own limits),
//! trading frequency ([`Schedule`]), latency (the agent's `LatencyProfile`), information quality
//! (signal latency plus [`Perception`] noise), execution style, and inventory limits. A
//! [`KillSwitch`] stops an agent after a loss.
//!
//! [`add_background`] with [`BackgroundConfig`] adds the background market calibrated to stylised
//! facts, and [`calibrated_market`] builds a whole market around it (docs/STYLIZED_FACTS.md).
//!
//! [`mm`] holds the market makers: models (GLFT, Avellaneda–Stoikov), estimators, the ten-step
//! quoting ladder and the [`mm::MarketMaker`] agent (docs/MARKET_MAKING.md).

#![forbid(unsafe_code)]

pub mod common;
pub mod execution;
pub mod fundamental;
pub mod informed;
pub mod latency_arb;
pub mod mm;
pub mod noise;
pub mod pairs;
pub mod presets;
pub mod probe;
pub mod technical;

pub use common::{Ewma, KillSwitch, Limits, Perception, Schedule, View};
pub use execution::{Algo, ExecutionAgent, MetaorderSource, ParentOrder, ParentReport};
pub use fundamental::FundamentalTrader;
pub use informed::InformedTrader;
pub use latency_arb::LatencyArbitrageur;
pub use noise::NoiseTrader;
pub use pairs::PairsTrader;
pub use presets::{
    Background, BackgroundConfig, CalibratedMarket, add_background, calibrated_market, calibrated_value, location_of,
    location_profiles,
};
pub use probe::ProbeTrader;
pub use technical::{MeanReversionTrader, MomentumTrader};
