//! Online estimators a market maker keeps from what it sees itself: its delayed book view, the
//! trades on its feed and its own fills. All in ticks and seconds; forgetting is exponential in
//! time with a stated half-life. Transcendental functions come from `sim::detmath`.

use std::collections::VecDeque;

use lob::{Price, Qty, Side, Timestamp};
use sim::detmath::{exp, ln};

use super::linalg::solve;

const LN2: f64 = std::f64::consts::LN_2;

fn decay(dt_s: f64, half_life_s: f64) -> f64 {
    exp(-LN2 * dt_s.max(0.0) / half_life_s)
}

/// Volatility of the mid, ticks per √second: σ² = Σ r²/Σ Δt over mid changes sampled at most every
/// `sample_s` seconds, with exponential forgetting. Sampling on a time grid rather than at every
/// tick keeps bid-ask flicker (fast reversals of the mid) from inflating the estimate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VolEstimator {
    /// Half-life of the forgetting, seconds.
    pub half_life_s: f64,
    /// Minimum time between samples, seconds.
    pub sample_s: f64,
    sum_sq: f64,
    sum_dt: f64,
    last: Option<(Timestamp, f64)>,
}

impl VolEstimator {
    /// New estimator.
    pub fn new(half_life_s: f64, sample_s: f64) -> Self {
        VolEstimator { half_life_s, sample_s, sum_sq: 0.0, sum_dt: 0.0, last: None }
    }

    /// Observe the mid (in ticks) at `now`.
    pub fn update(&mut self, now: Timestamp, mid_ticks: f64) {
        match self.last {
            None => self.last = Some((now, mid_ticks)),
            Some((t, m)) => {
                let dt = (now - t) as f64 / 1e9;
                if dt >= self.sample_s {
                    let w = decay(dt, self.half_life_s);
                    self.sum_sq = self.sum_sq * w + (mid_ticks - m) * (mid_ticks - m);
                    self.sum_dt = self.sum_dt * w + dt;
                    self.last = Some((now, mid_ticks));
                }
            }
        }
    }

    /// Current estimate, once at least a quarter of a half-life has been observed.
    pub fn sigma(&self) -> Option<f64> {
        (self.sum_dt >= 0.25 * self.half_life_s).then(|| (self.sum_sq / self.sum_dt).sqrt())
    }
}

/// Best bid and ask with their quantities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Top {
    /// Best bid.
    pub bid: Price,
    /// Quantity at the best bid.
    pub bid_qty: Qty,
    /// Best ask.
    pub ask: Price,
    /// Quantity at the best ask.
    pub ask_qty: Qty,
}

impl Top {
    /// Mid, in price units.
    pub fn mid(&self) -> f64 {
        (self.bid + self.ask) as f64 / 2.0
    }

    /// Share of top-of-book quantity on the bid, in [0, 1].
    pub fn imbalance(&self) -> f64 {
        let total = (self.bid_qty + self.ask_qty) as f64;
        if total > 0.0 { self.bid_qty as f64 / total } else { 0.5 }
    }
}

/// Order-flow imbalance at the best quotes (Cont, Kukanov & Stoikov 2014, transcribed from
/// arXiv:1011.6402 §2.1): for consecutive observations n−1, n,
/// eₙ = 1{Pᴮₙ ≥ Pᴮₙ₋₁}·qᴮₙ − 1{Pᴮₙ ≤ Pᴮₙ₋₁}·qᴮₙ₋₁ − 1{Pᴬₙ ≤ Pᴬₙ₋₁}·qᴬₙ + 1{Pᴬₙ ≥ Pᴬₙ₋₁}·qᴬₙ₋₁.
/// Positive values mean demand grew or supply shrank. The tracker sums eₙ with exponential
/// forgetting and also keeps a forgetting average of the top-of-book depth for normalisation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OfiTracker {
    /// Half-life, seconds.
    pub half_life_s: f64,
    last: Option<(Timestamp, Top)>,
    ofi: f64,
    depth: f64,
    depth_weight: f64,
}

impl OfiTracker {
    /// New tracker.
    pub fn new(half_life_s: f64) -> Self {
        OfiTracker { half_life_s, last: None, ofi: 0.0, depth: 0.0, depth_weight: 0.0 }
    }

    /// The contribution eₙ of moving from `prev` to `next`.
    pub fn contribution(prev: &Top, next: &Top) -> f64 {
        let mut e = 0.0;
        if next.bid >= prev.bid {
            e += next.bid_qty as f64;
        }
        if next.bid <= prev.bid {
            e -= prev.bid_qty as f64;
        }
        if next.ask <= prev.ask {
            e -= next.ask_qty as f64;
        }
        if next.ask >= prev.ask {
            e += prev.ask_qty as f64;
        }
        e
    }

    /// Observe the best quotes at `now`; returns eₙ (0 for the first observation).
    pub fn update(&mut self, now: Timestamp, top: Top) -> f64 {
        let e = match self.last {
            Some((t, prev)) => {
                let w = decay((now - t) as f64 / 1e9, self.half_life_s);
                self.ofi *= w;
                self.depth *= w;
                self.depth_weight *= w;
                Self::contribution(&prev, &top)
            }
            None => 0.0,
        };
        self.ofi += e;
        self.depth += 0.5 * (top.bid_qty + top.ask_qty) as f64;
        self.depth_weight += 1.0;
        self.last = Some((now, top));
        e
    }

    /// Forgetting sum of eₙ, in quantity.
    pub fn ofi(&self) -> f64 {
        self.ofi
    }

    /// OFI divided by the average top-of-book depth: a dimensionless pressure signal.
    pub fn normalized(&self) -> f64 {
        if self.depth_weight > 0.0 && self.depth > 0.0 { self.ofi / (self.depth / self.depth_weight) } else { 0.0 }
    }
}

/// Fill-intensity estimator for λ(δ) = A·e^(−kδ): the rate at which aggressive orders reach at least
/// depth δ from the mid (each aggressive order counted once, at its deepest execution), fitted by
/// least squares on ln(rate) against δ over a grid of depths. This is the usual proxy for how often
/// a quote at depth δ would be hit; it ignores queue position, so it overstates fills for an order
/// at the back of a long queue.
#[derive(Clone, Debug, PartialEq)]
pub struct IntensityEstimator {
    /// Depth grid, ticks.
    pub depths: Vec<f64>,
    /// Half-life, seconds.
    pub half_life_s: f64,
    /// Depths with fewer (forgetting-weighted) events are left out of the fit.
    pub min_count: f64,
    counts: Vec<f64>,
    elapsed_s: f64,
    last: Option<Timestamp>,
}

impl IntensityEstimator {
    /// Estimator on depths 0.5, 1.0, …, 5.0 ticks.
    pub fn new(half_life_s: f64) -> Self {
        let depths: Vec<f64> = (1..=10).map(|j| 0.5 * j as f64).collect();
        IntensityEstimator { counts: vec![0.0; depths.len()], depths, half_life_s, min_count: 10.0, elapsed_s: 0.0, last: None }
    }

    /// Let time pass to `now`.
    pub fn advance(&mut self, now: Timestamp) {
        if let Some(t) = self.last {
            let dt = (now - t) as f64 / 1e9;
            let w = decay(dt, self.half_life_s);
            self.counts.iter_mut().for_each(|c| *c *= w);
            self.elapsed_s = self.elapsed_s * w + dt;
        }
        self.last = Some(now);
    }

    /// One aggressive order whose deepest execution was `depth_ticks` from the mid before it.
    pub fn record(&mut self, depth_ticks: f64) {
        for (c, &d) in self.counts.iter_mut().zip(&self.depths) {
            if depth_ticks >= d - 1e-9 {
                *c += 1.0;
            }
        }
    }

    /// Fitted (A, k), when at least two depths have enough events and k comes out positive.
    pub fn fit(&self) -> Option<(f64, f64)> {
        if self.elapsed_s <= 0.0 {
            return None;
        }
        let pts: Vec<(f64, f64)> = self
            .depths
            .iter()
            .zip(&self.counts)
            .filter(|(_, c)| **c >= self.min_count)
            .map(|(&d, &c)| (d, ln(c / self.elapsed_s)))
            .collect();
        if pts.len() < 2 {
            return None;
        }
        let n = pts.len() as f64;
        let (mx, my) = (pts.iter().map(|p| p.0).sum::<f64>() / n, pts.iter().map(|p| p.1).sum::<f64>() / n);
        let sxx: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
        let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
        let slope = sxy / sxx;
        (slope < 0.0).then(|| (exp(my - slope * mx), -slope))
    }
}

/// Mark-outs of the agent's own fills: the mid `horizon` after a fill against the fill price, in
/// ticks per unit, signed so that positive is good for the market maker (a buy followed by a rise).
/// Negative mark-outs are adverse selection: the counterparty knew something.
#[derive(Clone, Debug, PartialEq)]
pub struct Markouts {
    /// Horizon, nanoseconds.
    pub horizon_ns: i64,
    /// Weight of each new mark-out in the running average.
    pub alpha: f64,
    pending: VecDeque<(Timestamp, Side, f64, f64)>,
    average: Option<f64>,
    /// Settled fills.
    pub count: u64,
    /// Sum of quantity × mark-out, ticks × quantity.
    pub total: f64,
}

impl Markouts {
    /// New tracker.
    pub fn new(horizon_ns: i64, alpha: f64) -> Self {
        Markouts { horizon_ns, alpha, pending: VecDeque::new(), average: None, count: 0, total: 0.0 }
    }

    /// One of the agent's fills (price in ticks).
    pub fn record_fill(&mut self, at: Timestamp, side: Side, price_ticks: f64, qty: f64) {
        self.pending.push_back((at, side, price_ticks, qty));
    }

    /// Settle every fill at least `horizon` old against the current mid (in ticks).
    pub fn update(&mut self, now: Timestamp, mid_ticks: f64) {
        while let Some(&(at, side, price, qty)) = self.pending.front() {
            if now - at < self.horizon_ns {
                break;
            }
            self.pending.pop_front();
            let m = match side {
                Side::Buy => mid_ticks - price,
                Side::Sell => price - mid_ticks,
            };
            self.average = Some(self.average.map_or(m, |a| a + self.alpha * (m - a)));
            self.count += 1;
            self.total += m * qty;
        }
    }

    /// Recent average mark-out per fill, ticks.
    pub fn recent(&self) -> Option<f64> {
        self.average
    }

    /// Fills waiting for their horizon.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

/// Micro-price: the expected mid after the mid has changed many times, given the current top-of-book
/// imbalance and spread. Defined so that it is a martingale.
///
/// It is estimated as a Markov chain on states (imbalance bucket, spread). For every observed
/// transition x → y with mid change Δ:
/// * Q(x, y) is the probability of moving without a mid change, T(x, y) of moving with one, and R(x)
///   the expected Δ;
/// * the first mid change from x has expected size g₁ = (I − Q)⁻¹R, and it leaves the chain in state
///   B = (I − Q)⁻¹T;
/// * so the expected change after many mid changes is G = Σᵢ Bⁱg₁.
///
/// Transitions are counted twice, once mirrored (imbalance i ↔ 1 − i, Δ ↔ −Δ). That makes the chain
/// symmetric and the series converge. This construction follows the descriptions of Stoikov (2018)
/// in the papers that cite it; the original paper was not available, so the method is checked here
/// against direct simulation (see the tests), not against the paper.
#[derive(Clone, Debug, PartialEq)]
pub struct MicroPrice {
    /// Number of imbalance buckets.
    pub buckets: usize,
    /// Spreads above this many ticks share the last state.
    pub max_spread_ticks: i64,
    /// Terms of the series Σ Bⁱ g₁.
    pub terms: usize,
    /// Refit after this many new transitions.
    pub refit_every: u64,
    no_change: Vec<Vec<f64>>,
    change: Vec<Vec<f64>>,
    moves: Vec<f64>,
    totals: Vec<f64>,
    last: Option<(usize, f64)>,
    since_fit: u64,
    adjustment: Option<Vec<f64>>,
}

impl MicroPrice {
    /// New estimator: `buckets` imbalance buckets, spreads 1..=`max_spread_ticks`.
    pub fn new(buckets: usize, max_spread_ticks: i64) -> Self {
        let n = buckets * max_spread_ticks as usize;
        MicroPrice {
            buckets,
            max_spread_ticks,
            terms: 8,
            refit_every: 500,
            no_change: vec![vec![0.0; n]; n],
            change: vec![vec![0.0; n]; n],
            moves: vec![0.0; n],
            totals: vec![0.0; n],
            last: None,
            since_fit: 0,
            adjustment: None,
        }
    }

    fn state(&self, imbalance: f64, spread_ticks: i64) -> usize {
        let b = ((imbalance * self.buckets as f64) as usize).min(self.buckets - 1);
        (spread_ticks.clamp(1, self.max_spread_ticks) as usize - 1) * self.buckets + b
    }

    fn mirror(&self, x: usize) -> usize {
        let (s, b) = (x / self.buckets, x % self.buckets);
        s * self.buckets + (self.buckets - 1 - b)
    }

    /// Observe a state directly (for tests and other feeds): mid in ticks.
    pub fn observe_state(&mut self, state: usize, mid_ticks: f64) {
        if let Some((x, m)) = self.last {
            let d = mid_ticks - m;
            if x != state || d != 0.0 {
                for (from, to, dm) in [(x, state, d), (self.mirror(x), self.mirror(state), -d)] {
                    self.totals[from] += 1.0;
                    if dm == 0.0 {
                        self.no_change[from][to] += 1.0;
                    } else {
                        self.change[from][to] += 1.0;
                        self.moves[from] += dm;
                    }
                }
                self.since_fit += 1;
                if self.since_fit >= self.refit_every {
                    self.refit();
                }
            }
        }
        self.last = Some((state, mid_ticks));
    }

    /// Observe the best quotes (prices in ticks).
    pub fn observe(&mut self, top: &Top, tick: Price) {
        let spread = ((top.ask - top.bid) / tick).max(1);
        let s = self.state(top.imbalance(), spread);
        self.observe_state(s, top.mid() / tick as f64);
    }

    /// Recompute the adjustments from the counts so far.
    pub fn refit(&mut self) {
        self.since_fit = 0;
        let n = self.totals.len();
        if self.totals.iter().any(|&t| t == 0.0) {
            return; // every state must have been seen
        }
        let p = |m: &Vec<Vec<f64>>, i: usize, j: usize| m[i][j] / self.totals[i];
        let i_minus_q: Vec<Vec<f64>> =
            (0..n).map(|i| (0..n).map(|j| if i == j { 1.0 } else { 0.0 } - p(&self.no_change, i, j)).collect()).collect();
        let mut rhs: Vec<Vec<f64>> = (0..n).map(|i| (0..n).map(|j| p(&self.change, i, j)).collect()).collect();
        for (i, row) in rhs.iter_mut().enumerate() {
            row.push(self.moves[i] / self.totals[i]);
        }
        let Some(sol) = solve(&i_minus_q, &rhs) else { return };
        let b: Vec<Vec<f64>> = sol.iter().map(|r| r[..n].to_vec()).collect();
        let g1: Vec<f64> = sol.iter().map(|r| r[n]).collect();
        let (mut g, mut term) = (g1.clone(), g1);
        for _ in 1..self.terms {
            term = (0..n).map(|i| (0..n).map(|j| b[i][j] * term[j]).sum()).collect();
            g.iter_mut().zip(&term).for_each(|(a, t)| *a += t);
        }
        self.adjustment = Some(g);
    }

    /// Adjustment G (ticks) for a state, once fitted.
    pub fn adjustment_for_state(&self, state: usize) -> Option<f64> {
        self.adjustment.as_ref().map(|g| g[state])
    }

    /// Micro-price in ticks for the given best quotes, falling back to the size-weighted mid
    /// before the first fit.
    pub fn price_ticks(&self, top: &Top, tick: Price) -> f64 {
        let spread = ((top.ask - top.bid) / tick).max(1);
        match self.adjustment_for_state(self.state(top.imbalance(), spread)) {
            Some(g) => top.mid() / tick as f64 + g,
            None => {
                let i = top.imbalance();
                (top.bid as f64 * (1.0 - i) + top.ask as f64 * i) / tick as f64
            }
        }
    }

    /// Whether the estimator has been fitted.
    pub fn is_fitted(&self) -> bool {
        self.adjustment.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lob::rng::Rng;
    use sim::detmath;

    #[test]
    fn volatility_of_a_random_walk_is_recovered() {
        // Mid moves ±1 tick every 0.25 s on average (σ² = 4 ticks²/s); observed every 10 ms.
        let mut rng = Rng::new(5);
        let mut v = VolEstimator::new(120.0, 1.0);
        let (mut t, mut mid, mut next) = (0i64, 0.0f64, 0i64);
        while t < 600_000_000_000 {
            while next <= t {
                mid += if rng.chance(0.5) { 1.0 } else { -1.0 };
                next += (detmath::exponential(250_000_000.0, &mut rng)).round() as i64;
            }
            v.update(t, mid);
            t += 10_000_000;
        }
        let s = v.sigma().expect("enough data");
        assert!((s - 2.0).abs() < 0.25, "σ {s}");
        // Sparse observations (exponential gaps of 3 s on average, as in a quiet market): the
        // estimate must divide by the time actually elapsed, not by the nominal sampling step.
        let mut v = VolEstimator::new(600.0, 1.0);
        let (mut t, mut mid, mut next) = (0i64, 0.0f64, 0i64);
        while t < 6_000_000_000_000 {
            while next <= t {
                mid += if rng.chance(0.5) { 1.0 } else { -1.0 };
                next += (detmath::exponential(250_000_000.0, &mut rng)).round() as i64;
            }
            v.update(t, mid);
            t += detmath::exponential(3e9, &mut rng).round() as i64;
        }
        let s = v.sigma().expect("enough data");
        assert!((s - 2.0).abs() < 0.3, "σ from sparse observations {s}");
    }

    #[test]
    fn ofi_follows_the_cks_cases() {
        let top = |bid, bq, ask, aq| Top { bid, bid_qty: bq, ask, ask_qty: aq };
        let base = top(100, 50, 101, 40);
        // Nothing changes: 0.
        assert_eq!(OfiTracker::contribution(&base, &base), 0.0);
        // Bid size grows by 10 at the same price: +10.
        assert_eq!(OfiTracker::contribution(&base, &top(100, 60, 101, 40)), 10.0);
        // Bid price improves with 7 shares: +7.
        assert_eq!(OfiTracker::contribution(&top(100, 50, 102, 40), &top(101, 7, 102, 40)), 7.0);
        // Bid level wiped out (price drops to 99 with 30 behind): −50.
        assert_eq!(OfiTracker::contribution(&base, &top(99, 30, 101, 40)), -50.0);
        // Ask size grows by 15: −15 (more supply).
        assert_eq!(OfiTracker::contribution(&base, &top(100, 50, 101, 55)), -15.0);
        // Ask level lifted (price rises to 102): +40 (supply removed).
        assert_eq!(OfiTracker::contribution(&base, &top(100, 50, 102, 25)), 40.0);
        let mut t = OfiTracker::new(1.0);
        t.update(0, base);
        t.update(1_000_000_000, top(100, 60, 101, 40));
        assert!((t.ofi() - 10.0).abs() < 1e-12);
        t.update(2_000_000_000, top(100, 60, 101, 40));
        assert!((t.ofi() - 5.0).abs() < 1e-9, "one half-life later: {}", t.ofi());
    }

    #[test]
    fn intensity_of_an_exponential_depth_profile_is_recovered() {
        // 20 aggressive orders per second; depth beyond the touch exponential with rate 1.5 per
        // tick: λ(δ) = 20·e^(−1.5(δ − 0.5)) for δ ≥ 0.5, i.e. A = 20·e^0.75, k = 1.5.
        let mut rng = Rng::new(8);
        let mut est = IntensityEstimator::new(1e9);
        let mut t = 0i64;
        for _ in 0..200_000 {
            t += detmath::exponential(50_000_000.0, &mut rng).round() as i64;
            est.advance(t);
            est.record(0.5 + detmath::exponential(1.0 / 1.5, &mut rng));
        }
        let (a, k) = est.fit().expect("fit");
        assert!((k - 1.5).abs() < 0.05, "k {k}");
        assert!((a / (20.0 * detmath::exp(0.75)) - 1.0).abs() < 0.08, "A {a}");
    }

    #[test]
    fn markouts_are_signed_for_the_market_maker() {
        let mut m = Markouts::new(1_000, 0.5);
        m.record_fill(0, Side::Buy, 100.0, 2.0);
        m.record_fill(10, Side::Sell, 101.0, 1.0);
        m.update(999, 104.0);
        assert_eq!(m.pending(), 2, "nothing settles before the horizon");
        m.update(1_000, 103.0);
        assert_eq!((m.count, m.total), (1, 6.0), "a buy followed by a rise of 3 ticks, 2 units");
        m.update(1_010, 102.0);
        assert_eq!((m.count, m.total), (2, 5.0), "a sell followed by a rise of 1 tick is adverse");
        assert_eq!(m.recent(), Some(1.0));
    }

    /// A synthetic book: three imbalance states (low, even, high) and a one-tick spread. From each
    /// state the mid moves half a tick with probability 0.3, up with probability 0.8 / 0.5 / 0.2 in the
    /// high / even / low state, and the next state is drawn from a sticky distribution.
    fn synthetic_step(rng: &mut Rng, state: usize) -> (usize, f64) {
        let up = [0.2, 0.5, 0.8][state];
        let d = if rng.chance(0.3) { if rng.chance(up) { 0.5 } else { -0.5 } } else { 0.0 };
        let next = if rng.chance(0.6) { state } else { rng.index(3) };
        (next, d)
    }

    #[test]
    fn micro_price_matches_the_simulated_long_run_expected_mid() {
        // Ground truth by brute force: from each state, the mid after 8 mid changes, averaged over
        // many simulated paths. Each change shrinks the start state's influence about threefold
        // here, so 8 changes are the long run to within 1e-4 ticks.
        let mut rng = Rng::new(21);
        let truth: Vec<f64> = (0..3)
            .map(|s0| {
                let paths = 200_000;
                let mut sum = 0.0;
                for _ in 0..paths {
                    let (mut s, mut mid, mut changes) = (s0, 0.0, 0);
                    while changes < 8 {
                        let (ns, d) = synthetic_step(&mut rng, s);
                        if d != 0.0 {
                            changes += 1;
                        }
                        mid += d;
                        s = ns;
                    }
                    sum += mid;
                }
                sum / paths as f64
            })
            .collect();
        // The estimator, fitted on one long path.
        let mut mp = MicroPrice::new(3, 1);
        mp.refit_every = u64::MAX;
        let (mut s, mut mid) = (1usize, 0.0);
        mp.observe_state(s, mid);
        for _ in 0..1_000_000 {
            let (ns, d) = synthetic_step(&mut rng, s);
            mid += d;
            s = ns;
            mp.observe_state(s, mid);
        }
        mp.refit();
        for (state, t) in truth.iter().enumerate() {
            let g = mp.adjustment_for_state(state).expect("fitted");
            assert!((g - t).abs() < 0.015, "state {state}: estimate {g:.4}, simulated {t:.4}");
        }
        assert!(truth[2] > 0.1 && truth[0] < -0.1 && truth[1].abs() < 0.02, "the example has real signal: {truth:?}");
    }
}
