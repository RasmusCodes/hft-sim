//! Quoting strategies: from what the market maker sees to the price levels it wants to quote.
//!
//! A [`QuotingStrategy`] is a pure decision rule over a [`Snapshot`]:
//! * a reference price ([`Center`]), optionally shifted by order flow ([`FlowSkew`]);
//! * a depth on each side ([`Pricing`]), widened when recent fills were toxic ([`ToxicityGuard`]);
//! * one or more levels ([`Levels`]) within a hard inventory bound.
//!
//! The market-maker agent turns the levels into orders; [`keep_resting`] is its rule for keeping an
//! order that holds a good queue position.
//!
//! [`QuotingStrategy::ladder`] builds the ten-step ladder of ARCHITECTURE §6. Each step adds one
//! idea to the previous one, so neighbouring steps can be compared:
//!
//! | Step | Name | Adds |
//! |---|---|---|
//! | 1 | fixed spread | quotes at mid ± a fixed half-spread |
//! | 2 | inventory-aware | skew against inventory (linear), hard inventory limit |
//! | 3 | micro-price | reference price = micro-price instead of mid |
//! | 4 | Avellaneda–Stoikov | reservation price and spread from the A–S formulas (rolling horizon) |
//! | 5 | GLFT | asymptotic optimal quotes with the inventory bound (exact, not the closed form) |
//! | 6 | volatility-adjusted | σ estimated online instead of fixed |
//! | 7 | order-flow-aware | reference shifted by order-flow imbalance and top-of-book imbalance |
//! | 8 | adverse-selection-aware | both sides widened after adverse mark-outs |
//! | 9 | queue-position-aware | resting orders with good queue positions kept when the target moves slightly |
//! | 10 | multi-level | three levels per side |
//!
//! Default parameters are illustrative, not fitted for performance.

use lob::{Price, Qty, Side, Timestamp};

use super::estimators::Top;
use super::models::{Depths, GlftSolution, MmModel};

/// What a strategy sees at a decision time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snapshot {
    /// Decision time.
    pub now: Timestamp,
    /// Tick size.
    pub tick: Price,
    /// Best quotes of the agent's own (delayed) book view.
    pub top: Top,
    /// Inventory in quote units (position ÷ quote size), signed.
    pub inventory: f64,
    /// Micro-price, ticks.
    pub micro_price: f64,
    /// Online volatility estimate, ticks/√s.
    pub sigma: Option<f64>,
    /// Online intensity estimate (A per second, k per tick).
    pub intensity: Option<(f64, f64)>,
    /// Normalised order-flow imbalance (OFI ÷ average top-of-book depth).
    pub ofi: f64,
    /// Recent average mark-out of own fills, ticks (negative = adverse).
    pub markout: Option<f64>,
}

/// Reference price the quotes are built around.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Center {
    /// Mid of the best quotes.
    Mid,
    /// Size-weighted mid: bid·(1 − I) + ask·I with I the bid share of top-of-book quantity.
    WeightedMid,
    /// The estimated micro-price (falls back to the size-weighted mid before it is fitted).
    MicroPrice,
}

/// How far from the reference each side is quoted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pricing {
    /// The same half-spread on both sides; inventory is ignored.
    Fixed {
        /// Half-spread, ticks.
        half_spread_ticks: f64,
    },
    /// Half-spread plus a linear skew: the bid moves away and the ask moves in by
    /// `skew_ticks_per_unit` per unit of long inventory (and the reverse when short).
    LinearSkew {
        /// Half-spread at zero inventory, ticks.
        half_spread_ticks: f64,
        /// Skew per unit of inventory, ticks.
        skew_ticks_per_unit: f64,
    },
    /// Avellaneda–Stoikov with a rolling horizon τ: δᵇ = qγσ²τ + ψ/2, δᵃ = −qγσ²τ + ψ/2,
    /// ψ = γσ²τ + (2/γ)ln(1 + γ/k).
    AvellanedaStoikov {
        /// Risk aversion, per tick·unit.
        gamma: f64,
        /// Time to the horizon, seconds (constant: the horizon rolls forward).
        tau_s: f64,
    },
    /// GLFT asymptotic optimal quotes with the strategy's inventory bound as Q.
    Glft {
        /// Risk aversion, per tick·unit.
        gamma: f64,
    },
}

/// Where σ comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SigmaSource {
    /// A fixed value, ticks/√s.
    Fixed(f64),
    /// The online estimate, with a fallback before it exists and a floor.
    Online {
        /// Used until the estimator has enough data.
        fallback: f64,
        /// Lower bound.
        floor: f64,
    },
}

/// Where the fill intensity λ(δ) = A·e^(−kδ) comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IntensitySource {
    /// Fixed values.
    Fixed {
        /// Fills per second at the reference price.
        a: f64,
        /// Decay per tick.
        k: f64,
    },
    /// The online estimate, with fallbacks and a floor on k.
    Online {
        /// A before the estimator has enough data.
        fallback_a: f64,
        /// k before the estimator has enough data.
        fallback_k: f64,
        /// Lower bound on k.
        k_floor: f64,
    },
}

/// Shift of the reference price toward the side order flow is pushing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FlowSkew {
    /// Ticks per unit of normalised OFI (clamped to [−1, 1]).
    pub ofi_ticks: f64,
    /// Ticks per unit of top-of-book imbalance, 2I − 1 ∈ [−1, 1].
    pub imbalance_ticks: f64,
}

/// Widen both sides after adverse fills.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ToxicityGuard {
    /// Widen when the recent average mark-out is below −threshold, ticks.
    pub threshold_ticks: f64,
    /// Extra depth on both sides while toxic, ticks.
    pub widen_ticks: f64,
}

/// Keep a resting order instead of replacing it, when it is close to the target and holds a good
/// place in its queue (most of its level's quantity is behind it, so it is likely to fill first).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QueueRule {
    /// Keep orders within this many ticks of the target price.
    pub tolerance_ticks: i64,
    /// … whose quantity ahead is at most this share of the level.
    pub max_ahead_share: f64,
}

/// Levels per side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Levels {
    /// Number of levels on each side.
    pub count: usize,
    /// Ticks between levels.
    pub spacing_ticks: i64,
    /// Units at each level.
    pub units: i64,
}

/// One desired quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    /// Side.
    pub side: Side,
    /// Price, on the tick grid.
    pub price: Price,
    /// Size in quote units.
    pub units: i64,
}

/// Shared parameters of the ladder presets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LadderParams {
    /// Hard inventory bound, units (also GLFT's Q).
    pub max_inventory: i64,
    /// Half-spread of steps 1–3, ticks.
    pub half_spread_ticks: f64,
    /// Linear skew of steps 2–3, ticks per unit.
    pub skew_ticks_per_unit: f64,
    /// Risk aversion of steps 4+, per tick·unit.
    pub gamma: f64,
    /// Rolling horizon of the Avellaneda–Stoikov step, seconds.
    pub as_horizon_s: f64,
    /// σ for steps 4–5 and the fallback after, ticks/√s.
    pub sigma: f64,
    /// A for steps 4–5 and the fallback after, per second.
    pub a: f64,
    /// k for steps 4–5 and the fallback after, per tick.
    pub k: f64,
}

impl Default for LadderParams {
    /// Defaults for the calibrated background market (docs/MARKET_MAKING.md §4):
    /// * σ = 1 tick/√s, the background's own 1-second volatility;
    /// * k = 5 per tick, measured by the intensity estimator;
    /// * A = 130 units/s, a market maker's own fill rate at the touch in a probe run, converted to
    ///   δ = 0.
    ///
    /// γ = 3.5 and the A–S horizon of 0.03 s then give the model steps the same inventory skew
    /// near zero inventory as the linear step, 0.1 tick per unit. Neighbouring steps therefore
    /// differ in the shape of their response, not in risk appetite. Illustrative, not optimised.
    fn default() -> Self {
        LadderParams {
            max_inventory: 10,
            half_spread_ticks: 0.5,
            skew_ticks_per_unit: 0.1,
            gamma: 3.5,
            as_horizon_s: 0.03,
            sigma: 1.0,
            a: 130.0,
            k: 5.0,
        }
    }
}

/// A quoting rule (see the module documentation).
#[derive(Clone, Debug, PartialEq)]
pub struct QuotingStrategy {
    /// Name, for reports.
    pub name: String,
    /// Reference price.
    pub center: Center,
    /// Depth model.
    pub pricing: Pricing,
    /// σ for model pricing.
    pub sigma: SigmaSource,
    /// Intensity for model pricing.
    pub intensity: IntensitySource,
    /// Order-flow shift of the reference.
    pub flow: Option<FlowSkew>,
    /// Widening after adverse fills.
    pub toxicity: Option<ToxicityGuard>,
    /// Queue-position rule for keeping resting orders.
    pub queue: Option<QueueRule>,
    /// Levels per side.
    pub levels: Levels,
    /// Hard inventory bound, units: no bids at +bound, no asks at −bound, and no level that could
    /// take inventory beyond it.
    pub max_inventory: i64,
    glft: Option<GlftSolution>,
}

/// Names of the ten ladder steps.
pub const LADDER: [&str; 10] = [
    "fixed spread",
    "inventory-aware",
    "micro-price",
    "Avellaneda-Stoikov",
    "GLFT",
    "volatility-adjusted",
    "order-flow-aware",
    "adverse-selection-aware",
    "queue-position-aware",
    "multi-level",
];

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 0.02 * a.abs().max(b.abs())
}

impl QuotingStrategy {
    /// A fixed-spread strategy around the mid (step 1 of the ladder).
    pub fn new(name: &str, half_spread_ticks: f64, max_inventory: i64) -> Self {
        QuotingStrategy {
            name: name.to_string(),
            center: Center::Mid,
            pricing: Pricing::Fixed { half_spread_ticks },
            sigma: SigmaSource::Fixed(1.0),
            intensity: IntensitySource::Fixed { a: 20.0, k: 2.0 },
            flow: None,
            toxicity: None,
            queue: None,
            levels: Levels { count: 1, spacing_ticks: 1, units: 1 },
            max_inventory,
            glft: None,
        }
    }

    /// Step `step` (1–10) of the ladder; each step is the previous one plus one idea.
    ///
    /// # Panics
    /// If `step` is not in 1..=10.
    pub fn ladder(step: usize, p: &LadderParams) -> Self {
        assert!((1..=10).contains(&step), "ladder steps are 1 to 10");
        let mut s = QuotingStrategy::new(LADDER[step - 1], p.half_spread_ticks, p.max_inventory);
        if step >= 2 {
            s.pricing = Pricing::LinearSkew { half_spread_ticks: p.half_spread_ticks, skew_ticks_per_unit: p.skew_ticks_per_unit };
        }
        if step >= 3 {
            s.center = Center::MicroPrice;
        }
        s.sigma = SigmaSource::Fixed(p.sigma);
        s.intensity = IntensitySource::Fixed { a: p.a, k: p.k };
        if step == 4 {
            s.pricing = Pricing::AvellanedaStoikov { gamma: p.gamma, tau_s: p.as_horizon_s };
        }
        if step >= 5 {
            s.pricing = Pricing::Glft { gamma: p.gamma };
        }
        if step >= 6 {
            // Only σ goes online: the intensity estimator measures the whole market's aggressive
            // flow, an upper bound on one market maker's fills (IntensitySource::Online is there
            // for experiments).
            s.sigma = SigmaSource::Online { fallback: p.sigma, floor: 0.1 };
        }
        if step >= 7 {
            s.flow = Some(FlowSkew { ofi_ticks: 0.5, imbalance_ticks: 0.25 });
        }
        if step >= 8 {
            s.toxicity = Some(ToxicityGuard { threshold_ticks: 0.25, widen_ticks: 1.0 });
        }
        if step >= 9 {
            s.queue = Some(QueueRule { tolerance_ticks: 1, max_ahead_share: 0.5 });
        }
        if step >= 10 {
            s.levels = Levels { count: 3, spacing_ticks: 1, units: 1 };
        }
        s
    }

    /// σ in use.
    pub fn sigma_for(&self, s: &Snapshot) -> f64 {
        match self.sigma {
            SigmaSource::Fixed(x) => x,
            SigmaSource::Online { fallback, floor } => s.sigma.unwrap_or(fallback).max(floor),
        }
    }

    /// (A, k) in use.
    pub fn intensity_for(&self, s: &Snapshot) -> (f64, f64) {
        match self.intensity {
            IntensitySource::Fixed { a, k } => (a, k),
            IntensitySource::Online { fallback_a, fallback_k, k_floor } => {
                let (a, k) = s.intensity.unwrap_or((fallback_a, fallback_k));
                (a, k.max(k_floor))
            }
        }
    }

    /// Reference price, ticks.
    pub fn reference(&self, s: &Snapshot) -> f64 {
        let t = s.tick as f64;
        let base = match self.center {
            Center::Mid => s.top.mid() / t,
            Center::WeightedMid => {
                let i = s.top.imbalance();
                (s.top.bid as f64 * (1.0 - i) + s.top.ask as f64 * i) / t
            }
            Center::MicroPrice => s.micro_price,
        };
        match self.flow {
            Some(f) => base + f.ofi_ticks * s.ofi.clamp(-1.0, 1.0) + f.imbalance_ticks * (2.0 * s.top.imbalance() - 1.0),
            None => base,
        }
    }

    /// Depths of the best level on each side, ticks (`None`: that side is not quoted).
    pub fn depths(&mut self, s: &Snapshot) -> Depths {
        let q = s.inventory;
        let mut d = match self.pricing {
            Pricing::Fixed { half_spread_ticks: h } => Depths { bid: Some(h), ask: Some(h) },
            Pricing::LinearSkew { half_spread_ticks: h, skew_ticks_per_unit: k } => Depths { bid: Some(h + k * q), ask: Some(h - k * q) },
            Pricing::AvellanedaStoikov { gamma, tau_s } => {
                let (a, k) = self.intensity_for(s);
                MmModel { sigma: self.sigma_for(s), a, k, gamma }.avellaneda_stoikov(q, tau_s)
            }
            Pricing::Glft { gamma } => {
                let (a, k) = self.intensity_for(s);
                let model = MmModel { sigma: self.sigma_for(s), a, k, gamma };
                let stale = self.glft.as_ref().is_none_or(|g| {
                    let m = g.model;
                    g.q_max != self.max_inventory || !(close(m.sigma, model.sigma) && close(m.a, model.a) && close(m.k, model.k) && m.gamma == model.gamma)
                });
                if stale {
                    self.glft = Some(GlftSolution::new(model, self.max_inventory));
                }
                let g = self.glft.as_ref().expect("solution just computed");
                g.asymptotic_depths((q.round() as i64).clamp(-self.max_inventory, self.max_inventory))
            }
        };
        if let (Some(t), Some(m)) = (self.toxicity, s.markout) {
            if m < -t.threshold_ticks {
                d.bid = d.bid.map(|x| x + t.widen_ticks);
                d.ask = d.ask.map(|x| x + t.widen_ticks);
            }
        }
        d
    }

    /// The levels to quote. Prices are on the tick grid, rounded to the nearest tick (ties away
    /// from the reference), never at or through the opposite best quote of the agent's view (so
    /// post-only orders are not rejected on that view), and every bid is below every ask. Sizes are
    /// cut so that no fill can take inventory beyond the bound.
    pub fn quotes(&mut self, s: &Snapshot) -> Vec<Level> {
        let r = self.reference(s);
        let d = self.depths(s);
        let tick = s.tick;
        let (best_bid, best_ask) = (s.top.bid / tick, s.top.ask / tick);
        let q = s.inventory;
        let mut out = Vec::new();
        let mut room_buy = (self.max_inventory as f64 - q).floor() as i64;
        let mut room_sell = (self.max_inventory as f64 + q).floor() as i64;
        let first_bid = d.bid.map(|db| {
            let x = r - db;
            let p = if x - x.floor() > 0.5 { x.ceil() } else { x.floor() };
            (p as i64).min(best_ask - 1)
        });
        let first_ask = d.ask.map(|da| {
            let x = r + da;
            let p = if x.ceil() - x > 0.5 { x.floor() } else { x.ceil() };
            (p as i64).max(best_bid + 1)
        });
        // Keep the quotes uncrossed with each other: if both sides land on the same tick (or cross),
        // move back the side that rounding pushed further inside its model price.
        let (first_bid, first_ask) = match (first_bid, first_ask) {
            (Some(b), Some(a)) if b >= a => {
                let bid_inside = b as f64 - (r - d.bid.unwrap_or(0.0));
                let ask_inside = (r + d.ask.unwrap_or(0.0)) - a as f64;
                if bid_inside > ask_inside { (Some(a - 1), Some(a)) } else { (Some(b), Some(b + 1)) }
            }
            other => other,
        };
        for i in 0..self.levels.count {
            let step = i as i64 * self.levels.spacing_ticks;
            if let Some(b) = first_bid {
                let units = self.levels.units.min(room_buy);
                if units > 0 {
                    out.push(Level { side: Side::Buy, price: (b - step) * tick, units });
                    room_buy -= units;
                }
            }
            if let Some(a) = first_ask {
                let units = self.levels.units.min(room_sell);
                if units > 0 {
                    out.push(Level { side: Side::Sell, price: (a + step) * tick, units });
                    room_sell -= units;
                }
            }
        }
        out
    }
}

/// Queue rule: keep a resting order at `resting` instead of moving it to `target`, given the
/// quantity ahead of it and the total quantity at its level?
pub fn keep_resting(rule: Option<QueueRule>, resting: Price, target: Price, tick: Price, ahead: Option<Qty>, level_qty: Qty) -> bool {
    if resting == target {
        return true;
    }
    let Some(rule) = rule else { return false };
    let Some(ahead) = ahead else { return false };
    (resting - target).abs() <= rule.tolerance_ticks * tick && level_qty > 0 && (ahead as f64) <= rule.max_ahead_share * level_qty as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(bid: Price, bq: Qty, ask: Price, aq: Qty, inventory: f64) -> Snapshot {
        let top = Top { bid, bid_qty: bq, ask, ask_qty: aq };
        Snapshot { now: 0, tick: 1, top, inventory, micro_price: top.mid(), sigma: None, intensity: None, ofi: 0.0, markout: None }
    }

    fn sides(levels: &[Level]) -> (Vec<(Price, i64)>, Vec<(Price, i64)>) {
        let pick = |s: Side| levels.iter().filter(|l| l.side == s).map(|l| (l.price, l.units)).collect();
        (pick(Side::Buy), pick(Side::Sell))
    }

    #[test]
    fn fixed_spread_joins_the_touch_and_ignores_inventory() {
        let mut s = QuotingStrategy::ladder(1, &LadderParams::default());
        assert_eq!(sides(&s.quotes(&snap(100, 50, 101, 50, 0.0))), (vec![(100, 1)], vec![(101, 1)]));
        assert_eq!(sides(&s.quotes(&snap(100, 50, 101, 50, 5.0))), (vec![(100, 1)], vec![(101, 1)]));
        // Two-tick spread: mid 101, half-spread 0.5 → 100.5 / 101.5, rounded away from the mid.
        assert_eq!(sides(&s.quotes(&snap(100, 50, 102, 50, 0.0))), (vec![(100, 1)], vec![(102, 1)]));
        // At the bound the side that would add inventory disappears; one unit short of it, one unit fits.
        assert_eq!(sides(&s.quotes(&snap(100, 50, 101, 50, 10.0))), (vec![], vec![(101, 1)]));
        assert_eq!(sides(&s.quotes(&snap(100, 50, 101, 50, -10.0))), (vec![(100, 1)], vec![]));
    }

    #[test]
    fn inventory_skew_moves_both_quotes_against_the_position() {
        let p = LadderParams { skew_ticks_per_unit: 0.25, ..LadderParams::default() };
        let mut s = QuotingStrategy::ladder(2, &p);
        // Long 4 units on a wide 96/106 market (mid 101): bid 101 − 0.5 − 1 = 99.5 → 99, ask 101 + 0.5 − 1 = 100.5 → 101.
        assert_eq!(sides(&s.quotes(&snap(96, 50, 106, 50, 4.0))), (vec![(99, 1)], vec![(101, 1)]));
        // Short 4: the mirror image.
        assert_eq!(sides(&s.quotes(&snap(96, 50, 106, 50, -4.0))), (vec![(101, 1)], vec![(103, 1)]));
        // On a one-tick market a large skew pins the ask at the best ask (never through the bid).
        assert_eq!(sides(&s.quotes(&snap(100, 50, 101, 50, 8.0))), (vec![(98, 1)], vec![(101, 1)]));
    }

    #[test]
    fn micro_price_centre_follows_the_estimator() {
        let mut s = QuotingStrategy::ladder(3, &LadderParams { skew_ticks_per_unit: 0.0, ..LadderParams::default() });
        let mut x = snap(96, 50, 106, 50, 0.0);
        x.micro_price = 103.0;
        assert_eq!(sides(&s.quotes(&x)), (vec![(102, 1)], vec![(104, 1)]));
        let mut w = QuotingStrategy::new("weighted", 0.5, 10);
        w.center = Center::WeightedMid;
        // 90% of the top quantity on the bid: 96·0.1 + 106·0.9 = 105.
        assert!((w.reference(&snap(96, 90, 106, 10, 0.0)) - 105.0).abs() < 1e-12);
    }

    #[test]
    fn avellaneda_stoikov_quotes_follow_the_formula() {
        let p = LadderParams { gamma: 0.1, as_horizon_s: 2.0, sigma: 1.0, k: 2.0, ..LadderParams::default() };
        let mut s = QuotingStrategy::ladder(4, &p);
        let m = MmModel { sigma: 1.0, a: p.a, k: 2.0, gamma: 0.1 };
        for q in [-6.0, -2.0, 0.0, 3.0, 7.0] {
            let x = snap(80, 50, 120, 50, q);
            let d = m.avellaneda_stoikov(q, 2.0);
            let (bids, asks) = sides(&s.quotes(&x));
            let round = |v: f64, up: bool| -> Price { if up { (v - 0.5).ceil() as Price } else { (v + 0.5).floor() as Price } };
            // Reservation price 100 − 0.2q, spread 0.2 + 20 ln 1.05, rounded away from the reference on ties.
            assert_eq!(bids[0].0, round(100.0 - d.bid.unwrap(), false), "bid at q = {q}");
            assert_eq!(asks[0].0, round(100.0 + d.ask.unwrap(), true), "ask at q = {q}");
        }
    }

    #[test]
    fn glft_quotes_are_the_asymptotic_solution_and_widen_with_volatility() {
        let p = LadderParams::default();
        let mut s = QuotingStrategy::ladder(5, &p);
        let exact = GlftSolution::new(MmModel { sigma: p.sigma, a: p.a, k: p.k, gamma: p.gamma }, p.max_inventory);
        for q in [-9i64, -3, 0, 4, 9] {
            let d = exact.asymptotic_depths(q);
            let x = snap(60, 50, 140, 50, q as f64);
            let (bids, asks) = sides(&s.quotes(&x));
            // Nearest tick (ties away from the reference); if both sides land on one tick, the
            // side rounded further inside its model price steps back.
            let (mb, ma) = (100.0 - d.bid.unwrap(), 100.0 + d.ask.unwrap());
            let (mut b, mut a) = ((mb + 0.5).floor() as Price, (ma - 0.5).ceil() as Price);
            if b >= a {
                if b as f64 - mb > ma - a as f64 { b = a - 1 } else { a = b + 1 }
            }
            assert_eq!((bids[0].0, asks[0].0), (b, a), "q = {q}: exact depths {d:?}");
        }
        // Step 6 takes σ online: a calmer market gives tighter quotes, a wilder one wider quotes
        // (and a stronger inventory skew).
        let mut v = QuotingStrategy::ladder(6, &p);
        let depths = |v: &mut QuotingStrategy, sigma: f64, q: f64| {
            let mut x = snap(60, 50, 140, 50, q);
            x.sigma = Some(sigma);
            v.depths(&x)
        };
        let width = |d: Depths| d.bid.unwrap() + d.ask.unwrap();
        assert!(width(depths(&mut v, 0.5, 0.0)) < width(depths(&mut v, 1.0, 0.0)));
        assert!(width(depths(&mut v, 1.0, 0.0)) < width(depths(&mut v, 4.0, 0.0)));
        let skew = |v: &mut QuotingStrategy, sigma: f64| {
            let d = depths(v, sigma, 3.0);
            d.bid.unwrap() - d.ask.unwrap()
        };
        assert!(skew(&mut v, 2.0) > skew(&mut v, 1.0), "more volatility, stronger skew against a long position");
        // The defaults give the model steps the linear step's 0.1-tick skew per unit near zero.
        let near_zero = |d0: Depths, d1: Depths| d1.bid.unwrap() - d0.bid.unwrap();
        let (d0, d1) = (exact.asymptotic_depths(0), exact.asymptotic_depths(1));
        assert!((near_zero(d0, d1) - 0.1).abs() < 0.02, "GLFT skew per unit {}", near_zero(d0, d1));
        let m = MmModel { sigma: p.sigma, a: p.a, k: p.k, gamma: p.gamma };
        let as_skew = m.avellaneda_stoikov(1.0, p.as_horizon_s).bid.unwrap() - m.avellaneda_stoikov(0.0, p.as_horizon_s).bid.unwrap();
        assert!((as_skew - 0.1).abs() < 0.01, "A–S skew per unit {as_skew}");
    }

    #[test]
    fn order_flow_shifts_the_reference_toward_the_pressure() {
        let p = LadderParams::default();
        let s = QuotingStrategy::ladder(7, &p);
        let mut x = snap(96, 50, 106, 50, 0.0);
        let base = s.reference(&x);
        x.ofi = 0.6;
        assert!((s.reference(&x) - base - 0.3).abs() < 1e-12, "0.5 tick per unit of normalised OFI");
        x.ofi = 5.0;
        assert!((s.reference(&x) - base - 0.5).abs() < 1e-12, "clamped at one unit");
        let heavy_bid = snap(96, 90, 106, 10, 0.0);
        assert!(s.reference(&heavy_bid) > heavy_bid.micro_price, "bid-heavy book shifts the reference up");
    }

    #[test]
    fn adverse_markouts_widen_both_sides() {
        let p = LadderParams::default();
        let mut s = QuotingStrategy::ladder(8, &p);
        let mut x = snap(60, 50, 140, 50, 0.0);
        let calm = s.depths(&x);
        x.markout = Some(-0.1);
        assert_eq!(s.depths(&x), calm, "inside the threshold");
        x.markout = Some(-0.5);
        let toxic = s.depths(&x);
        assert!((toxic.bid.unwrap() - calm.bid.unwrap() - 1.0).abs() < 1e-12 && (toxic.ask.unwrap() - calm.ask.unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn queue_rule_keeps_well_placed_orders_near_the_target() {
        let rule = Some(QueueRule { tolerance_ticks: 1, max_ahead_share: 0.5 });
        assert!(keep_resting(None, 100, 100, 1, None, 0), "an order at the target is always kept");
        assert!(!keep_resting(None, 100, 101, 1, Some(0), 50), "without the rule any move replaces it");
        assert!(keep_resting(rule, 100, 101, 1, Some(10), 50), "one tick off, near the front");
        assert!(!keep_resting(rule, 100, 101, 1, Some(40), 50), "one tick off, at the back");
        assert!(!keep_resting(rule, 100, 102, 1, Some(0), 50), "two ticks off");
        assert!(!keep_resting(rule, 100, 101, 1, None, 50), "queue position unknown");
    }

    #[test]
    fn multi_level_ladders_respect_the_inventory_bound() {
        let mut s = QuotingStrategy::ladder(10, &LadderParams::default());
        let (bids, asks) = sides(&s.quotes(&snap(60, 50, 140, 50, 0.0)));
        assert_eq!(bids.len(), 3);
        assert_eq!(asks.len(), 3);
        assert!(bids.windows(2).all(|w| w[0].0 - w[1].0 == 1) && asks.windows(2).all(|w| w[1].0 - w[0].0 == 1));
        // Long 8.4 of 10: only one more unit can be bought; selling has room for all three levels.
        let (bids, asks) = sides(&s.quotes(&snap(60, 50, 140, 50, 8.4)));
        assert_eq!(bids.iter().map(|b| b.1).sum::<i64>(), 1);
        assert_eq!(asks.iter().map(|a| a.1).sum::<i64>(), 3);
    }

    #[test]
    fn every_ladder_step_quotes_inside_the_post_only_bounds_without_self_crossing() {
        let p = LadderParams::default();
        for step in 1..=10 {
            let mut s = QuotingStrategy::ladder(step, &p);
            for (bid, ask) in [(100, 101), (100, 102), (95, 105)] {
                for q in [-9.0, -3.0, 0.0, 2.5, 9.0] {
                    for micro in [bid as f64 - 3.0, (bid + ask) as f64 / 2.0, ask as f64 + 3.0] {
                        let mut x = snap(bid, 50, ask, 50, q);
                        x.micro_price = micro;
                        x.markout = Some(-1.0);
                        x.ofi = 1.0;
                        let levels = s.quotes(&x);
                        let max_bid = levels.iter().filter(|l| l.side == Side::Buy).map(|l| l.price).max();
                        let min_ask = levels.iter().filter(|l| l.side == Side::Sell).map(|l| l.price).min();
                        assert!(max_bid.is_none_or(|b| b < ask), "{}: bid {max_bid:?} at or through the ask {ask}", s.name);
                        assert!(min_ask.is_none_or(|a| a > bid), "{}: ask {min_ask:?} at or through the bid {bid}", s.name);
                        if let (Some(b), Some(a)) = (max_bid, min_ask) {
                            assert!(b < a, "{}: own quotes cross ({b} ≥ {a})", s.name);
                        }
                    }
                }
            }
        }
    }
}
