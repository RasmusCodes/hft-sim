//! Ready-made agent populations.
//!
//! [`add_background`] adds the background market used to check stylised facts:
//! * noise traders (liquidity and random flow);
//! * fundamental traders that anchor price to value;
//! * execution agents working random parent orders with power-law sizes, the source of
//!   long-memory order flow.
//!
//! Agents are spread over three locations (co-located, proximity, remote). Each location shares
//! one market-data feed group in snapshot mode, since none of these agents needs a callback on
//! every packet.

use lob::{Price, Timestamp};
use sim::{AgentConfig, AgentId, FeedGroupId, FeedMode, InstrumentId, LatencyProfile, SignalId, SignalProcess, SimBuilder, detmath};

use crate::common::{Limits, Perception, Schedule};
use crate::execution::{ExecutionAgent, MetaorderSource};
use crate::fundamental::FundamentalTrader;
use crate::noise::NoiseTrader;

/// Illustrative latency profiles of the three locations: co-located, proximity, remote.
pub fn location_profiles() -> [LatencyProfile; 3] {
    [LatencyProfile::colocated(), LatencyProfile::proximity(), LatencyProfile::remote()]
}

/// Location of the k-th background agent: 10% co-located, 60% proximity, 30% remote.
pub fn location_of(k: usize) -> usize {
    match k % 10 {
        0 => 0,
        1..=6 => 1,
        _ => 2,
    }
}

/// The fundamental-value process the background defaults were calibrated against, for an
/// instrument with tick `tick` starting at `start`: one-tick moves every `mean_interval_s`
/// seconds on average, with a 1% chance that a move is a 10-tick jump (news).
pub fn calibrated_value(start: Price, tick: Price, mean_interval_s: f64) -> SignalProcess {
    SignalProcess::JumpDiffusion {
        start,
        step: tick,
        mean_interval_ns: mean_interval_s * 1e9,
        jump_probability: 0.01,
        jump: 10 * tick,
    }
}

/// Parameters of the background market.
#[derive(Clone, Debug)]
pub struct BackgroundConfig {
    /// Number of noise traders.
    pub noise: usize,
    /// Template for noise traders (its instrument is overwritten).
    pub noise_trader: NoiseTrader,
    /// Spread the noise traders' anchor memories log-uniformly over this range of seconds,
    /// overriding the template's `anchor_memory_ns`: liquidity providers then remember prices over
    /// different horizons.
    pub noise_anchor_memory_s: Option<(f64, f64)>,
    /// Number of fundamental traders.
    pub fundamental: usize,
    /// Template for fundamental traders (instrument, signal and threshold are overwritten).
    pub fundamental_trader: FundamentalTrader,
    /// Thresholds of the fundamental traders are spread uniformly over [0, this] ticks.
    pub fundamental_max_threshold_ticks: f64,
    /// Number of execution agents producing random parent orders.
    pub executors: usize,
    /// Their parent-order process.
    pub metaorders: MetaorderSource,
    /// Stop every background agent after this time.
    pub stop_at: Option<Timestamp>,
}

impl Default for BackgroundConfig {
    /// The calibrated defaults (see docs/STYLIZED_FACTS.md).
    fn default() -> Self {
        let mut noise_trader = NoiseTrader::new(0);
        noise_trader.p_market = 0.005;
        noise_trader.p_cancel = 0.35;
        noise_trader.depth_decay = 0.02;
        noise_trader.max_order_age_ns = Some(1_000_000_000);
        let mut fundamental_trader = FundamentalTrader::new(0, 0);
        fundamental_trader.perception = Perception { noise_sd: 1.0 };
        fundamental_trader.schedule = Schedule::Poisson { mean_ns: 50_000_000.0 };
        fundamental_trader.lots_per_tick = 100.0;
        fundamental_trader.max_lots = 2_000;
        fundamental_trader.limits = Limits { max_position: 2_000, max_order_qty: 2_000, max_open_orders: 10, capital: i128::MAX };
        BackgroundConfig {
            noise: 100,
            noise_trader,
            noise_anchor_memory_s: Some((0.8, 8.0)),
            fundamental: 20,
            fundamental_trader,
            fundamental_max_threshold_ticks: 6.0,
            executors: 4,
            metaorders: MetaorderSource {
                mean_gap_ns: 1_000_000_000.0,
                min_children: 10,
                max_children: 5_000,
                tail_alpha: 1.5,
                child_lots: 2,
                child_interval_ns: 10_000_000,
            },
            stop_at: None,
        }
    }
}

impl BackgroundConfig {
    /// The same market with pure zero-intelligence noise liquidity (no price anchor). Returns are
    /// closer to uncorrelated at 1-10 s, but metaorder impact is only marginally concave (see
    /// docs/STYLIZED_FACTS.md). Results of a strategy should be checked on both backgrounds.
    pub fn zero_intelligence() -> Self {
        let mut cfg = Self::default();
        cfg.noise_anchor_memory_s = None;
        cfg.noise_trader.anchor_memory_ns = None;
        cfg
    }
}

/// A one-instrument market holding the background, ready for more agents.
pub struct CalibratedMarket {
    /// Builder with the instrument, the value signal and the background added.
    pub builder: SimBuilder,
    /// The instrument: `InstrumentSpec::example()` (cents, tick 1, reference $100.00).
    pub instrument: InstrumentId,
    /// Fundamental value: [`calibrated_value`] with moves every `value_interval_s` seconds.
    pub value: SignalId,
    /// The background agents.
    pub background: Background,
}

/// The market the background was calibrated in (with `value_interval_s = 1.0` and the default
/// config). Add agents to `builder`, then build.
pub fn calibrated_market(seed: u64, value_interval_s: f64, cfg: &BackgroundConfig) -> CalibratedMarket {
    let spec = lob::InstrumentSpec::example();
    let mut builder = SimBuilder::new(seed);
    let instrument = builder.add_instrument(sim::InstrumentConfig::new(spec));
    let value = builder.add_signal(calibrated_value(spec.reference_price, spec.tick_size, value_interval_s));
    let background = add_background(&mut builder, instrument, value, cfg);
    CalibratedMarket { builder, instrument, value, background }
}

/// Ids of the agents added by [`add_background`].
#[derive(Clone, Debug, Default)]
pub struct Background {
    /// Feed groups of the three locations.
    pub groups: Vec<FeedGroupId>,
    /// Noise traders.
    pub noise: Vec<AgentId>,
    /// Fundamental traders.
    pub fundamental: Vec<AgentId>,
    /// Execution agents.
    pub executors: Vec<AgentId>,
}

/// Add the background market for `instrument`, whose fundamental value is signal `value`.
pub fn add_background(b: &mut SimBuilder, instrument: InstrumentId, value: SignalId, cfg: &BackgroundConfig) -> Background {
    let profiles = location_profiles();
    let groups: Vec<FeedGroupId> = profiles.iter().map(|p| b.add_feed_group(p.feed.clone())).collect();
    let mut out = Background { groups: groups.clone(), ..Background::default() };
    let mut k = 0usize;
    let config = |k: &mut usize| {
        let loc = location_of(*k);
        *k += 1;
        AgentConfig::new(profiles[loc].clone()).subscribe_group(instrument, groups[loc], FeedMode::Snapshot)
    };
    for j in 0..cfg.noise {
        let mut a = cfg.noise_trader.clone();
        a.instrument = instrument;
        if let Some((lo, hi)) = cfg.noise_anchor_memory_s {
            let t = (j as f64 + 0.5) / cfg.noise as f64;
            a.anchor_memory_ns = Some(1e9 * detmath::exp(detmath::ln(lo) + t * (detmath::ln(hi) - detmath::ln(lo))));
        }
        a.stop_at = cfg.stop_at.or(a.stop_at);
        let c = config(&mut k);
        out.noise.push(b.add_agent(a, c));
    }
    for j in 0..cfg.fundamental {
        let mut a = cfg.fundamental_trader.clone();
        a.instrument = instrument;
        a.signal = value;
        a.threshold_ticks = cfg.fundamental_max_threshold_ticks * (j as f64 + 0.5) / cfg.fundamental as f64;
        a.stop_at = cfg.stop_at.or(a.stop_at);
        let c = config(&mut k).with_signal(value);
        out.fundamental.push(b.add_agent(a, c));
    }
    for _ in 0..cfg.executors {
        let mut a = ExecutionAgent::random(instrument, cfg.metaorders);
        a.stop_at = cfg.stop_at;
        let c = config(&mut k);
        out.executors.push(b.add_agent(a, c));
    }
    out
}
