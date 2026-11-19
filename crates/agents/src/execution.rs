//! Institutional execution: parent orders worked by child orders over time.
//!
//! Schedules (share of the parent that should be done after slice k of n):
//! * TWAP: k/n.
//! * VWAP: cumulative weight of a volume profile (for example a U shape over the day).
//! * Almgren–Chriss: 1 − sinh(κ(1 − τ))/sinh(κ) with τ = k/n, the optimal trajectory under linear
//!   temporary impact (Almgren & Chriss 2001). κ is the urgency: κ → 0 is TWAP, larger κ
//!   front-loads. κ is a parameter here, not estimated from risk aversion and impact.
//! * POV: a fixed share of the market volume seen on the agent's own feed since the start.
//!
//! At every slice the agent sends one marketable IOC child for the shortfall against the
//! schedule, so missed fills are caught up later. Each parent gets a [`ParentReport`] (arrival
//! price, fills, average price, implementation shortfall).
//!
//! [`MetaorderSource`] creates random parents for background flow: Poisson arrivals, random sides
//! and Pareto-distributed sizes (P(size ≥ s) ∝ s^−α). Splitting such parents into many child
//! orders is the mechanism Lillo, Mike & Farmer (2005) proposed for the long memory of order
//! signs: with tail exponent α the sign autocorrelation decays like lag^−(α−1).

use std::collections::{BTreeMap, VecDeque};

use lob::rng::Rng;
use lob::{Qty, Side, Timestamp};
use sim::detmath;
use sim::{Agent, ClientOrderId, Ctx, ExecReport, InstrumentId, ReportKind, RequestType};

use crate::common::{Limits, View, random_side, send_marketable};

/// Execution schedule.
#[derive(Clone, Debug, PartialEq)]
pub enum Algo {
    /// Equal slices.
    Twap,
    /// Slices weighted by a volume profile over the horizon (any number of non-negative weights).
    Vwap {
        /// Relative volume per equal sub-period of the horizon.
        profile: Vec<f64>,
    },
    /// Almgren–Chriss trajectory with urgency κ (0 = TWAP).
    AlmgrenChriss {
        /// Urgency κ (dimensionless, per horizon).
        urgency: f64,
    },
    /// Participation of volume: execute `rate` times the market volume seen since the start.
    Pov {
        /// Target participation rate in (0, 1].
        rate: f64,
    },
}

fn sinh(x: f64) -> f64 {
    (detmath::exp(x) - detmath::exp(-x)) / 2.0
}

impl Algo {
    /// Share of the parent that should be done after slice `k` of `n` (schedule-based algos).
    pub fn target_fraction(&self, k: u32, n: u32) -> f64 {
        let n = n.max(1);
        let tau = (k.min(n)) as f64 / n as f64;
        match self {
            Algo::Twap | Algo::Pov { .. } => tau,
            Algo::Vwap { profile } => {
                if profile.is_empty() {
                    return tau;
                }
                let (mut done, mut all) = (0.0, 0.0);
                for j in 1..=n {
                    let midpoint = (j as f64 - 0.5) / n as f64;
                    let bucket = ((midpoint * profile.len() as f64) as usize).min(profile.len() - 1);
                    let w = profile[bucket].max(0.0);
                    all += w;
                    if j <= k {
                        done += w;
                    }
                }
                if all > 0.0 { done / all } else { tau }
            }
            Algo::AlmgrenChriss { urgency } => {
                if urgency.abs() < 1e-9 {
                    tau
                } else {
                    1.0 - sinh(urgency * (1.0 - tau)) / sinh(*urgency)
                }
            }
        }
    }
}

/// A parent order.
#[derive(Clone, Debug, PartialEq)]
pub struct ParentOrder {
    /// Side.
    pub side: Side,
    /// Total quantity.
    pub qty: Qty,
    /// Scheduled start.
    pub start: Timestamp,
    /// Scheduled end.
    pub end: Timestamp,
    /// Number of slices (child decisions) between start and end.
    pub slices: u32,
    /// Schedule.
    pub algo: Algo,
}

/// Outcome of one parent order.
#[derive(Clone, Debug, PartialEq)]
pub struct ParentReport {
    /// Side.
    pub side: Side,
    /// Parent quantity.
    pub qty: Qty,
    /// Actual start.
    pub start: Timestamp,
    /// Scheduled end.
    pub end: Timestamp,
    /// The agent's view of the mid at the start (arrival price).
    pub arrival_mid: Option<f64>,
    /// Quantity filled (fills reported after the end still count).
    pub filled: Qty,
    /// Sum of price × quantity over fills.
    pub notional: i128,
    /// Child orders sent.
    pub children: u32,
}

impl ParentReport {
    /// Average fill price.
    pub fn average_price(&self) -> Option<f64> {
        (self.filled > 0).then(|| self.notional as f64 / self.filled as f64)
    }

    /// Implementation shortfall per unit against the arrival mid, in price units (positive = cost).
    pub fn shortfall(&self) -> Option<f64> {
        let avg = self.average_price()?;
        let arrival = self.arrival_mid?;
        Some(match self.side {
            Side::Buy => avg - arrival,
            Side::Sell => arrival - avg,
        })
    }
}

/// Random parent orders for background flow, executed by TWAP at a fixed child rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MetaorderSource {
    /// Mean idle time between one parent's end and the next parent, in nanoseconds.
    pub mean_gap_ns: f64,
    /// Smallest parent, in child orders.
    pub min_children: i64,
    /// Largest parent, in child orders.
    pub max_children: i64,
    /// Tail exponent α: P(children ≥ c) ∝ c^−α.
    pub tail_alpha: f64,
    /// Lots per child order.
    pub child_lots: i64,
    /// Time between child orders, in nanoseconds.
    pub child_interval_ns: i64,
}

impl MetaorderSource {
    /// Draw a parent starting at `now`.
    pub fn draw(&self, now: Timestamp, lot: Qty, rng: &mut Rng) -> ParentOrder {
        let u = 1.0 - rng.f64(); // (0, 1]
        let scale = detmath::exp(-detmath::ln(u) / self.tail_alpha);
        let children = ((self.min_children as f64 * scale).floor() as i64).clamp(self.min_children.max(1), self.max_children);
        ParentOrder {
            side: random_side(rng),
            qty: children * self.child_lots * lot,
            start: now,
            end: now + children * self.child_interval_ns,
            slices: children as u32,
            algo: Algo::Twap,
        }
    }
}

const START: u64 = 0;
const SLICE: u64 = 1;
const FINISH: u64 = 2;

#[derive(Clone, Debug)]
struct Active {
    parent: ParentOrder,
    report: usize,
    slice: u32,
    start_volume: Qty,
}

/// Works parent orders one at a time: a scripted queue first, then (optionally) random parents.
#[derive(Clone, Debug)]
pub struct ExecutionAgent {
    /// Instrument traded.
    pub instrument: InstrumentId,
    /// Scripted parents, executed in order.
    pub parents: VecDeque<ParentOrder>,
    /// Random parents after the queue is empty.
    pub source: Option<MetaorderSource>,
    /// Child orders cross the opposite best by at most this many ticks.
    pub protection_ticks: i64,
    /// Strategy-side limits (they cap child sizes, not the parent).
    pub limits: Limits,
    /// Start no new parent after this time.
    pub stop_at: Option<Timestamp>,
    /// One report per parent started.
    pub reports: Vec<ParentReport>,
    active: Option<Active>,
    generation: u64,
    child_parent: BTreeMap<ClientOrderId, usize>,
}

impl ExecutionAgent {
    /// Agent working the given parents in order.
    pub fn new(instrument: InstrumentId, parents: Vec<ParentOrder>) -> Self {
        ExecutionAgent {
            instrument,
            parents: parents.into(),
            source: None,
            protection_ticks: 3,
            limits: Limits { max_position: Qty::MAX / 4, max_order_qty: Qty::MAX / 4, max_open_orders: 1_000, capital: i128::MAX },
            stop_at: None,
            reports: Vec::new(),
            active: None,
            generation: 0,
            child_parent: BTreeMap::new(),
        }
    }

    /// Agent producing random parents from `source`.
    pub fn random(instrument: InstrumentId, source: MetaorderSource) -> Self {
        let mut agent = Self::new(instrument, Vec::new());
        agent.source = Some(source);
        agent
    }

    fn timer(&self, ctx: &mut Ctx<'_>, at: Timestamp, kind: u64) {
        ctx.set_timer(at.max(ctx.now()), (kind << 32) | (self.generation & 0xffff_ffff));
    }

    fn schedule_next_parent(&mut self, ctx: &mut Ctx<'_>) {
        if self.stop_at.is_some_and(|s| ctx.now() > s) {
            return;
        }
        if let Some(p) = self.parents.front() {
            let at = p.start;
            self.timer(ctx, at, START);
        } else if let Some(src) = self.source {
            let gap = (detmath::exponential(src.mean_gap_ns, ctx.rng()).round() as i64).max(1);
            let at = ctx.now() + gap;
            if self.stop_at.is_none_or(|s| at <= s) {
                self.timer(ctx, at, START);
            }
        }
    }

    fn begin(&mut self, ctx: &mut Ctx<'_>) {
        let i = self.instrument;
        let lot = ctx.spec(i).map_or(1, |s| s.lot_size);
        let parent = match self.parents.pop_front() {
            Some(p) => p,
            None => match self.source {
                Some(src) => {
                    let now = ctx.now();
                    src.draw(now, lot, ctx.rng())
                }
                None => return,
            },
        };
        let (arrival_mid, start_volume) = View::of(ctx, i).map_or((None, 0), |v| (v.mid(), v.book.trade_stats().1));
        self.reports.push(ParentReport {
            side: parent.side,
            qty: parent.qty,
            start: ctx.now(),
            end: parent.end,
            arrival_mid,
            filled: 0,
            notional: 0,
            children: 0,
        });
        self.active = Some(Active { parent, report: self.reports.len() - 1, slice: 0, start_volume });
        let now = ctx.now();
        self.timer(ctx, now, SLICE);
    }

    fn slice(&mut self, ctx: &mut Ctx<'_>) {
        let i = self.instrument;
        let Some(mut active) = self.active.take() else { return };
        active.slice += 1;
        let p = &active.parent;
        let lot = ctx.spec(i).map_or(1, |s| s.lot_size);
        let slices = p.slices.max(1);
        let target = match p.algo {
            Algo::Pov { rate } => {
                let seen = View::of(ctx, i).map_or(0, |v| v.book.trade_stats().1) - active.start_volume;
                (((rate * seen as f64).floor() as Qty).min(p.qty)) / lot * lot
            }
            _ if active.slice >= slices => p.qty,
            ref algo => ((p.qty as f64 * algo.target_fraction(active.slice, slices)).floor() as Qty) / lot * lot,
        };
        let report = active.report;
        let in_flight: Qty = ctx
            .orders()
            .live()
            .filter(|(c, _)| self.child_parent.get(c) == Some(&report))
            .map(|(_, o)| o.leaves)
            .sum();
        let deficit = target - self.reports[report].filled - in_flight;
        if deficit > 0 {
            if let Some(cid) = send_marketable(ctx, i, p.side, deficit, self.protection_ticks, &self.limits) {
                self.child_parent.insert(cid, report);
                self.reports[report].children += 1;
            }
        }
        let next = if active.slice < slices {
            let step = ((p.end - p.start) / slices as i64).max(1);
            Some(p.start + active.slice as i64 * step)
        } else {
            None
        };
        let end = p.end;
        self.active = Some(active);
        match next {
            Some(t) => self.timer(ctx, t, SLICE),
            None => self.timer(ctx, end, FINISH),
        }
    }
}

impl Agent for ExecutionAgent {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        self.schedule_next_parent(ctx);
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, token: u64) {
        if token & 0xffff_ffff != self.generation & 0xffff_ffff {
            return; // stale timer from a finished parent
        }
        match token >> 32 {
            START if self.active.is_none() => self.begin(ctx),
            SLICE => self.slice(ctx),
            FINISH => {
                self.active = None;
                self.generation += 1;
                self.schedule_next_parent(ctx);
            }
            _ => {}
        }
    }

    fn on_report(&mut self, _ctx: &mut Ctx<'_>, report: &ExecReport) {
        let Some(&r) = self.child_parent.get(&report.client_id) else { return };
        match report.kind {
            ReportKind::Fill { price, qty, leaves, .. } => {
                self.reports[r].filled += qty;
                self.reports[r].notional += i128::from(price) * i128::from(qty);
                if leaves == 0 {
                    self.child_parent.remove(&report.client_id);
                }
            }
            ReportKind::Cancelled { .. }
            | ReportKind::Rejected { .. }
            | ReportKind::GatewayRejected { request: RequestType::New, .. } => {
                self.child_parent.remove(&report.client_id);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_start_at_zero_and_end_at_one() {
        for algo in [
            Algo::Twap,
            Algo::Vwap { profile: vec![3.0, 1.0, 1.0, 3.0] },
            Algo::AlmgrenChriss { urgency: 2.0 },
        ] {
            assert_eq!(algo.target_fraction(0, 10), 0.0, "{algo:?}");
            assert!((algo.target_fraction(10, 10) - 1.0).abs() < 1e-12, "{algo:?}");
            let fractions: Vec<f64> = (0..=10).map(|k| algo.target_fraction(k, 10)).collect();
            assert!(fractions.windows(2).all(|w| w[1] >= w[0]), "monotone: {algo:?}");
        }
    }

    #[test]
    fn almgren_chriss_front_loads_and_tends_to_twap() {
        let urgent = Algo::AlmgrenChriss { urgency: 3.0 };
        assert!(urgent.target_fraction(5, 10) > 0.5, "an urgent schedule is more than half done at half time");
        let calm = Algo::AlmgrenChriss { urgency: 1e-6 };
        assert!((calm.target_fraction(5, 10) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn vwap_follows_the_profile() {
        let u = Algo::Vwap { profile: vec![2.0, 1.0, 1.0, 2.0] };
        // First quarter of the horizon carries 2/6 of the volume.
        assert!((u.target_fraction(1, 4) - 2.0 / 6.0).abs() < 1e-12);
    }

    #[test]
    fn metaorder_sizes_have_a_power_law_tail() {
        let src = MetaorderSource {
            mean_gap_ns: 1.0,
            min_children: 1,
            max_children: 1_000_000,
            tail_alpha: 1.5,
            child_lots: 1,
            child_interval_ns: 1,
        };
        let mut rng = Rng::new(9);
        let n = 200_000;
        let sizes: Vec<i64> = (0..n).map(|_| src.draw(0, 1, &mut rng).qty).collect();
        // P(size >= 100) = 100^-1.5 = 1e-3 for a continuous Pareto with x_min = 1 (floor keeps it).
        let tail = sizes.iter().filter(|&&s| s >= 100).count() as f64 / n as f64;
        assert!((tail / 1e-3 - 1.0).abs() < 0.15, "tail share {tail}");
    }
}
