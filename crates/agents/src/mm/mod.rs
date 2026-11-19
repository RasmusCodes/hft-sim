//! Market making (phase 3): models, estimators, the quoting-strategy ladder and the market-maker
//! agent. docs/MARKET_MAKING.md gives the sources and verification status of every formula.

pub mod agent;
pub mod estimators;
pub mod linalg;
pub mod models;
pub mod strategy;

pub use agent::{MarketMaker, MmStats};
pub use estimators::{IntensityEstimator, Markouts, MicroPrice, OfiTracker, Top, VolEstimator};
pub use models::{Depths, GlftSolution, MmModel};
pub use strategy::{
    Center, FlowSkew, IntensitySource, LADDER, LadderParams, Level, Levels, Pricing, QueueRule, QuotingStrategy, SigmaSource,
    Snapshot, ToxicityGuard, keep_resting,
};
