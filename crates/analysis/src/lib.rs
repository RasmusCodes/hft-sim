//! # analysis: stylised facts of a simulated market
//!
//! Measurements computed from the matching engine's event log (the ground truth, not any agent's
//! view) plus the parent-order records of execution agents:
//!
//! * [`SpreadStats`]: time-weighted distribution of the bid/ask spread;
//! * [`OrderFlowStats`]: how much submitted quantity executes and how many resting orders end
//!   cancelled rather than filled;
//! * [`SignStats`]: autocorrelation of market-order signs (one sign per aggressive order) and its
//!   power-law decay;
//! * [`ReturnStats`]: mid-price returns (fat tails, absence of linear autocorrelation, volatility
//!   clustering, variance ratio);
//! * [`ImpactStats`]: price impact of metaorders as a function of size.
//!
//! [`check_targets`] compares a report with the phase-2 targets (docs/STYLIZED_FACTS.md).

#![forbid(unsafe_code)]

pub mod metrics;

use std::fmt;

use lob::{CancelReason, DetHashMap, Event, EventKind, MboBook, OrderId, Price, Qty, Side, Timestamp};

/// One parent order worked over time (for impact analysis).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metaorder {
    /// Side.
    pub side: Side,
    /// Executed quantity.
    pub qty: Qty,
    /// Start time.
    pub start: Timestamp,
    /// End time.
    pub end: Timestamp,
}

/// Best bid and ask of the true book over time: one record per timestamp, holding the state
/// after every event at that timestamp.
#[derive(Clone, Debug, Default)]
pub struct BookPath {
    /// Record times, increasing.
    pub times: Vec<Timestamp>,
    /// Best bid after each record.
    pub bids: Vec<Option<Price>>,
    /// Best ask after each record.
    pub asks: Vec<Option<Price>>,
    /// Quantity at the best bid after each record (0 if none).
    pub bid_qty: Vec<Qty>,
    /// Quantity at the best ask after each record (0 if none).
    pub ask_qty: Vec<Qty>,
}

impl BookPath {
    /// Rebuild from an engine event log (public events only are used).
    pub fn from_events(events: &[Event]) -> Self {
        let mut book = MboBook::new();
        let mut path = BookPath::default();
        for e in events.iter().filter(|e| e.kind.is_public()) {
            book.apply(e).expect("the engine's event log is a consistent market-by-order stream");
            let (b, a) = (book.best_bid(), book.best_ask());
            if path.times.last() == Some(&e.ts) {
                *path.bids.last_mut().expect("record exists") = b.map(|x| x.0);
                *path.asks.last_mut().expect("record exists") = a.map(|x| x.0);
                *path.bid_qty.last_mut().expect("record exists") = b.map_or(0, |x| x.1);
                *path.ask_qty.last_mut().expect("record exists") = a.map_or(0, |x| x.1);
            } else {
                path.times.push(e.ts);
                path.bids.push(b.map(|x| x.0));
                path.asks.push(a.map(|x| x.0));
                path.bid_qty.push(b.map_or(0, |x| x.1));
                path.ask_qty.push(a.map_or(0, |x| x.1));
            }
        }
        path
    }

    /// Twice the mid of the last two-sided book at or before `t` (an integer, since prices are),
    /// or `None` if there was none.
    pub fn mid2_at(&self, t: Timestamp) -> Option<i64> {
        let k = self.times.partition_point(|&x| x <= t);
        (0..k).rev().find_map(|j| Some(self.bids[j]? + self.asks[j]?))
    }

    /// Best bid and ask in effect at `t` (after every event at or before `t`).
    pub fn at(&self, t: Timestamp) -> (Option<Price>, Option<Price>) {
        match self.times.partition_point(|&x| x <= t) {
            0 => (None, None),
            k => (self.bids[k - 1], self.asks[k - 1]),
        }
    }

    /// Mid price in effect at `t`.
    pub fn mid_at(&self, t: Timestamp) -> Option<f64> {
        let (bid, ask) = self.at(t);
        Some((bid? + ask?) as f64 / 2.0)
    }
}

/// Time-weighted spread distribution.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpreadStats {
    /// Share of two-sided time with a spread of 1, 2, 3, 4 and at least 5 ticks.
    pub share_by_ticks: [f64; 5],
    /// Time-weighted mean spread over two-sided time, in ticks.
    pub mean_ticks: f64,
    /// Share of the window with at least one side of the book empty.
    pub one_sided_share: f64,
}

fn spread_stats(path: &BookPath, from: Timestamp, to: Timestamp, tick: Price) -> SpreadStats {
    let mut buckets = [0f64; 5];
    let (mut two_sided, mut one_sided, mut weighted) = (0f64, 0f64, 0f64);
    let mut k = path.times.partition_point(|&x| x <= from);
    let mut state = path.at(from);
    let mut t = from;
    while t < to {
        let next = path.times.get(k).copied().unwrap_or(to).min(to);
        let dur = (next - t) as f64;
        match state {
            (Some(b), Some(a)) => {
                let ticks = (a - b) / tick;
                two_sided += dur;
                weighted += dur * ticks as f64;
                buckets[(ticks.max(1) as usize - 1).min(4)] += dur;
            }
            _ => one_sided += dur,
        }
        if next >= to {
            break;
        }
        state = (path.bids[k], path.asks[k]);
        t = next;
        k += 1;
    }
    let total = two_sided + one_sided;
    SpreadStats {
        share_by_ticks: buckets.map(|b| if two_sided > 0.0 { b / two_sided } else { 0.0 }),
        mean_ticks: if two_sided > 0.0 { weighted / two_sided } else { 0.0 },
        one_sided_share: if total > 0.0 { one_sided / total } else { 0.0 },
    }
}

/// Order-flow statistics for orders accepted inside the window.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OrderFlowStats {
    /// Orders accepted.
    pub accepted_orders: u64,
    /// Quantity accepted.
    pub accepted_qty: Qty,
    /// Quantity filled, summed over orders (each trade counts once for each side).
    pub filled_qty: Qty,
    /// Executed share of submitted quantity: `filled_qty / accepted_qty`.
    pub executed_share: f64,
    /// Trade volume (each trade once) over submitted quantity, the SEC-style ratio.
    pub trade_to_order_volume: f64,
    /// Orders that rested in the book at some point.
    pub rested_orders: u64,
    /// Resting orders that ended cancelled.
    pub rested_cancelled: u64,
    /// Resting orders that ended completely filled.
    pub rested_filled: u64,
    /// Share of finished resting orders that ended cancelled rather than filled.
    pub cancel_share: f64,
    /// Trades printed in the window.
    pub trades: u64,
}

#[derive(Clone, Copy, Default)]
struct OrderTrack {
    rested: bool,
    filled: bool,
    cancelled: bool,
}

fn order_flow(events: &[Event], from: Timestamp, to: Timestamp) -> OrderFlowStats {
    let mut s = OrderFlowStats::default();
    let mut orders: DetHashMap<OrderId, OrderTrack> = DetHashMap::default();
    let mut trade_volume: Qty = 0;
    for e in events {
        let inside = e.ts >= from && e.ts <= to;
        match e.kind {
            EventKind::Accepted { order_id, qty, .. } if inside => {
                s.accepted_orders += 1;
                s.accepted_qty += qty;
                orders.insert(order_id, OrderTrack::default());
            }
            EventKind::BookAdd { order_id, .. } => {
                if let Some(o) = orders.get_mut(&order_id) {
                    o.rested = true;
                }
            }
            EventKind::Fill { order_id, qty, leaves, .. } => {
                if let Some(o) = orders.get_mut(&order_id) {
                    s.filled_qty += qty;
                    if leaves == 0 {
                        o.filled = true;
                    }
                }
            }
            EventKind::Cancelled { order_id, reason, .. } => {
                if let Some(o) = orders.get_mut(&order_id) {
                    if reason != CancelReason::IocRemainder && reason != CancelReason::FokUnfilled {
                        o.cancelled = true;
                    }
                }
            }
            EventKind::Trade { qty, .. } if inside => {
                s.trades += 1;
                trade_volume += qty;
            }
            _ => {}
        }
    }
    for o in orders.values().filter(|o| o.rested) {
        s.rested_orders += 1;
        if o.filled {
            s.rested_filled += 1;
        } else if o.cancelled {
            s.rested_cancelled += 1;
        }
    }
    if s.accepted_qty > 0 {
        s.executed_share = s.filled_qty as f64 / s.accepted_qty as f64;
        s.trade_to_order_volume = trade_volume as f64 / s.accepted_qty as f64;
    }
    let finished = s.rested_cancelled + s.rested_filled;
    if finished > 0 {
        s.cancel_share = s.rested_cancelled as f64 / finished as f64;
    }
    s
}

/// Least-squares line through (ln x, ln y).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PowerLawFit {
    /// Slope of ln y on ln x (y ∝ x^slope).
    pub slope: f64,
    /// Prefactor: y ≈ prefactor · x^slope.
    pub prefactor: f64,
    /// Coefficient of determination in log-log space.
    pub r2: f64,
    /// Points used (positive x and y only).
    pub points: usize,
}

/// Fit y = c · x^slope by least squares in log-log space, using points with x > 0 and y > 0.
pub fn power_law_fit(points: &[(f64, f64)]) -> Option<PowerLawFit> {
    let weighted: Vec<(f64, f64, f64)> = points.iter().map(|&(x, y)| (x, y, 1.0)).collect();
    power_law_fit_weighted(&weighted)
}

/// Fit y = c · x^slope by weighted least squares in log-log space over points `(x, y, weight)`,
/// using points with x > 0, y > 0 and weight > 0. Needs at least three such points; r² is
/// weighted the same way.
pub fn power_law_fit_weighted(points: &[(f64, f64, f64)]) -> Option<PowerLawFit> {
    let logs: Vec<(f64, f64, f64)> = points
        .iter()
        .filter(|(x, y, w)| *x > 0.0 && *y > 0.0 && *w > 0.0)
        .map(|(x, y, w)| (x.ln(), y.ln(), *w))
        .collect();
    let n = logs.len();
    if n < 3 {
        return None;
    }
    let sw: f64 = logs.iter().map(|p| p.2).sum();
    let mx = logs.iter().map(|p| p.2 * p.0).sum::<f64>() / sw;
    let my = logs.iter().map(|p| p.2 * p.1).sum::<f64>() / sw;
    let sxx: f64 = logs.iter().map(|p| p.2 * (p.0 - mx).powi(2)).sum();
    let sxy: f64 = logs.iter().map(|p| p.2 * (p.0 - mx) * (p.1 - my)).sum();
    let syy: f64 = logs.iter().map(|p| p.2 * (p.1 - my).powi(2)).sum();
    if sxx == 0.0 {
        return None;
    }
    let slope = sxy / sxx;
    let intercept = my - slope * mx;
    let ss_res: f64 = logs.iter().map(|p| p.2 * (p.1 - intercept - slope * p.0).powi(2)).sum();
    Some(PowerLawFit {
        slope,
        prefactor: intercept.exp(),
        r2: if syy > 0.0 { 1.0 - ss_res / syy } else { 1.0 },
        points: n,
    })
}

/// Sample autocorrelation at lags 1..=`max_lag`.
pub fn autocorrelation(x: &[f64], max_lag: usize) -> Vec<f64> {
    let n = x.len();
    if n < 2 {
        return vec![0.0; max_lag];
    }
    let mean = x.iter().sum::<f64>() / n as f64;
    let centred: Vec<f64> = x.iter().map(|v| v - mean).collect();
    let var = centred.iter().map(|v| v * v).sum::<f64>();
    if var == 0.0 {
        return vec![0.0; max_lag];
    }
    (1..=max_lag)
        .map(|lag| {
            if lag >= n {
                0.0
            } else {
                centred[..n - lag].iter().zip(&centred[lag..]).map(|(a, b)| a * b).sum::<f64>() / var
            }
        })
        .collect()
}

/// Market-order sign statistics.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SignStats {
    /// Aggressive orders observed (one sign each, however many trades they produced).
    pub market_orders: usize,
    /// Share of buys.
    pub buy_share: f64,
    /// Autocorrelation of signs at lags 1..=max_lag.
    pub acf: Vec<f64>,
    /// Power-law fit of the autocorrelation over the fit range (slope = −γ).
    pub fit: Option<PowerLawFit>,
    /// Lag range used for the fit.
    pub fit_range: (usize, usize),
    /// Share of lags in 1..=100 with a positive autocorrelation.
    pub positive_share_1_100: f64,
}

impl SignStats {
    /// Decay exponent γ of C(ℓ) ∝ ℓ^−γ.
    pub fn gamma(&self) -> Option<f64> {
        self.fit.map(|f| -f.slope)
    }
}

/// One sign (+1 buy, −1 sell) per aggressive order in continuous trading, in time order.
pub fn market_order_signs(events: &[Event], from: Timestamp, to: Timestamp) -> Vec<f64> {
    let mut signs = Vec::new();
    let mut last_taker: Option<OrderId> = None;
    for e in events.iter().filter(|e| e.ts >= from && e.ts <= to) {
        if let EventKind::Trade { aggressor: Some(side), buy_order_id, sell_order_id, .. } = e.kind {
            let taker = if side == Side::Buy { buy_order_id } else { sell_order_id };
            if last_taker != Some(taker) {
                signs.push(if side == Side::Buy { 1.0 } else { -1.0 });
                last_taker = Some(taker);
            }
        }
    }
    signs
}

/// Power-law fit C(ℓ) = c·ℓ^slope of an autocorrelation function (`acf[ℓ − 1]` at lag ℓ) over
/// log-spaced lags (a factor 1.25 apart) in `fit_range`, so every decade weighs the same. Lags
/// with a non-positive autocorrelation are skipped. The decay exponent is γ = −slope. Averaging
/// the ACFs of independent runs before fitting gives a pooled estimate.
pub fn sign_acf_fit(acf: &[f64], fit_range: (usize, usize)) -> Option<PowerLawFit> {
    let mut lags: Vec<usize> = Vec::new();
    let mut l = fit_range.0.max(1) as f64;
    while (l as usize) <= fit_range.1.min(acf.len()) {
        let lag = l.round() as usize;
        if lags.last() != Some(&lag) && lag <= acf.len() {
            lags.push(lag);
        }
        l *= 1.25;
    }
    let points: Vec<(f64, f64)> = lags.iter().map(|&lag| (lag as f64, acf[lag - 1])).collect();
    power_law_fit(&points)
}

fn sign_stats(events: &[Event], from: Timestamp, to: Timestamp, max_lag: usize, fit_range: (usize, usize)) -> SignStats {
    let signs = market_order_signs(events, from, to);
    let acf = autocorrelation(&signs, max_lag);
    let upto = max_lag.min(100);
    let positive = acf[..upto].iter().filter(|&&c| c > 0.0).count();
    SignStats {
        market_orders: signs.len(),
        buy_share: if signs.is_empty() { 0.0 } else { signs.iter().filter(|&&s| s > 0.0).count() as f64 / signs.len() as f64 },
        fit: sign_acf_fit(&acf, fit_range),
        acf,
        fit_range,
        positive_share_1_100: if upto > 0 { positive as f64 / upto as f64 } else { 0.0 },
    }
}

/// Mid-price return statistics at a fixed sampling interval.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReturnStats {
    /// Sampling interval.
    pub interval_ns: i64,
    /// Returns used.
    pub returns: usize,
    /// Standard deviation of returns, in ticks.
    pub sd_ticks: f64,
    /// Excess kurtosis (0 for a Gaussian).
    pub excess_kurtosis: f64,
    /// Autocorrelation of returns at lag 1.
    pub acf1: f64,
    /// Autocorrelation of absolute returns at lags 1..=10 (volatility clustering).
    pub abs_acf: Vec<f64>,
    /// Variance of 10-interval returns over 10 × the variance of 1-interval returns.
    pub variance_ratio_10: f64,
}

fn return_stats(path: &BookPath, from: Timestamp, to: Timestamp, interval: i64, tick: Price) -> ReturnStats {
    let mut mids = Vec::new();
    let mut t = from;
    while t <= to {
        mids.push(path.mid_at(t));
        t += interval;
    }
    let returns: Vec<f64> = mids
        .windows(2)
        .filter_map(|w| Some((w[1]? - w[0]?) / tick as f64))
        .collect();
    let n = returns.len();
    if n < 20 {
        return ReturnStats { interval_ns: interval, returns: n, ..ReturnStats::default() };
    }
    let mean = returns.iter().sum::<f64>() / n as f64;
    let m2 = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n as f64;
    let m4 = returns.iter().map(|r| (r - mean).powi(4)).sum::<f64>() / n as f64;
    let abs: Vec<f64> = returns.iter().map(|r| r.abs()).collect();
    let tens: Vec<f64> = returns.chunks_exact(10).map(|c| c.iter().sum()).collect();
    let var10 = if tens.len() > 1 {
        let m = tens.iter().sum::<f64>() / tens.len() as f64;
        tens.iter().map(|x| (x - m).powi(2)).sum::<f64>() / tens.len() as f64
    } else {
        0.0
    };
    ReturnStats {
        interval_ns: interval,
        returns: n,
        sd_ticks: m2.sqrt(),
        excess_kurtosis: if m2 > 0.0 { m4 / (m2 * m2) - 3.0 } else { 0.0 },
        acf1: autocorrelation(&returns, 1)[0],
        abs_acf: autocorrelation(&abs, 10),
        variance_ratio_10: if m2 > 0.0 { var10 / (10.0 * m2) } else { 0.0 },
    }
}

/// Signed mid-price change over one metaorder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImpactSample {
    /// Executed quantity.
    pub qty: f64,
    /// Mid at the end minus mid at the start, in ticks, signed so that a move against the
    /// metaorder (up for a buy, down for a sell) is positive.
    pub impact_ticks: f64,
}

/// Impact of every metaorder with a fill that starts and ends inside [`from`, `to`] and has a
/// two-sided book at both ends.
pub fn impact_samples(path: &BookPath, metaorders: &[Metaorder], from: Timestamp, to: Timestamp, tick: Price) -> Vec<ImpactSample> {
    metaorders
        .iter()
        .filter(|m| m.qty > 0 && m.start >= from && m.end <= to)
        .filter_map(|m| {
            let move_ticks = (path.mid_at(m.end)? - path.mid_at(m.start)?) / tick as f64;
            let impact_ticks = if m.side == Side::Buy { move_ticks } else { -move_ticks };
            Some(ImpactSample { qty: m.qty as f64, impact_ticks })
        })
        .collect()
}

/// Mean impact of the metaorders in one size bin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImpactBin {
    /// Smallest quantity of the bin (inclusive).
    pub lower_qty: f64,
    /// Largest quantity of the bin (exclusive).
    pub upper_qty: f64,
    /// Mean executed quantity.
    pub mean_qty: f64,
    /// Mean impact, in ticks (positive = against the metaorder).
    pub mean_impact_ticks: f64,
    /// Standard error of the mean impact.
    pub stderr_ticks: f64,
    /// Metaorders in the bin.
    pub count: usize,
    /// Whether the bin holds enough metaorders to enter the fit.
    pub fitted: bool,
}

/// Metaorder impact against size.
///
/// Bins are log-spaced and anchored at fixed edges, [r^k, r^(k+1)) for bin ratio r, as in the
/// empirical literature. Equal-count bins are a poor choice under a fat-tailed size distribution:
/// most bins crowd into a narrow size range and the top bin spans decades, and averaging a concave
/// impact over such a wide bin biases δ downward (Jensen's inequality).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImpactStats {
    /// Metaorders measured.
    pub samples: Vec<ImpactSample>,
    /// Bin ratio r.
    pub bin_ratio: f64,
    /// Bins with fewer metaorders are reported but left out of the fit.
    pub min_bin_count: usize,
    /// Non-empty bins, increasing size.
    pub bins: Vec<ImpactBin>,
    /// Power-law fit I = Y·Q^δ of mean impact on mean size over the fitted bins, weighted by
    /// metaorder count (slope = δ).
    pub fit: Option<PowerLawFit>,
}

impl ImpactStats {
    /// Bin `samples` and fit the impact law. Pooling samples from independent runs with the same
    /// parameters is legitimate and narrows the estimate.
    ///
    /// # Panics
    /// If `bin_ratio` is not greater than 1.
    pub fn from_samples(mut samples: Vec<ImpactSample>, bin_ratio: f64, min_bin_count: usize) -> Self {
        assert!(bin_ratio > 1.0, "bin ratio must exceed 1");
        samples.sort_by(|a, b| a.qty.total_cmp(&b.qty).then(a.impact_ticks.total_cmp(&b.impact_ticks)));
        let mut bins = Vec::new();
        let (mut lower, mut k) = (1.0f64, 0usize);
        while k < samples.len() {
            // Edges are built by repeated multiplication, so they are exact for integer ratios.
            while samples[k].qty >= lower * bin_ratio {
                lower *= bin_ratio;
            }
            let upper = lower * bin_ratio;
            let end = k + samples[k..].partition_point(|s| s.qty < upper);
            let chunk = &samples[k..end];
            let c = chunk.len() as f64;
            let mean_qty = chunk.iter().map(|s| s.qty).sum::<f64>() / c;
            let mean_impact = chunk.iter().map(|s| s.impact_ticks).sum::<f64>() / c;
            let var = chunk.iter().map(|s| (s.impact_ticks - mean_impact).powi(2)).sum::<f64>() / (c - 1.0).max(1.0);
            bins.push(ImpactBin {
                lower_qty: lower,
                upper_qty: upper,
                mean_qty,
                mean_impact_ticks: mean_impact,
                stderr_ticks: (var / c).sqrt(),
                count: chunk.len(),
                fitted: chunk.len() >= min_bin_count.max(2),
            });
            k = end;
        }
        let points: Vec<(f64, f64, f64)> =
            bins.iter().filter(|b| b.fitted).map(|b| (b.mean_qty, b.mean_impact_ticks, b.count as f64)).collect();
        let fit = power_law_fit_weighted(&points);
        ImpactStats { samples, bin_ratio, min_bin_count, bins, fit }
    }

    /// Bins that enter the fit.
    pub fn fitted_bins(&self) -> impl Iterator<Item = &ImpactBin> {
        self.bins.iter().filter(|b| b.fitted)
    }
}

/// What to measure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FactsConfig {
    /// Window start (skip the warm-up while the book fills).
    pub from: Timestamp,
    /// Window end.
    pub to: Timestamp,
    /// Tick size.
    pub tick: Price,
    /// Return sampling interval.
    pub return_interval_ns: i64,
    /// Largest sign-autocorrelation lag.
    pub max_sign_lag: usize,
    /// Lag range of the power-law fit.
    pub sign_fit_range: (usize, usize),
    /// Ratio between the edges of consecutive metaorder size bins.
    pub impact_bin_ratio: f64,
    /// Size bins with fewer metaorders are left out of the impact fit.
    pub impact_min_bin_count: usize,
}

impl FactsConfig {
    /// The settings used for the phase-2 calibration: 1 s returns, sign lags up to 1,000 with the
    /// power law fitted over lags 5 to 500, size bins a factor 2 apart holding at least 30
    /// metaorders.
    pub fn standard(from: Timestamp, to: Timestamp, tick: Price) -> Self {
        FactsConfig {
            from,
            to,
            tick,
            return_interval_ns: 1_000_000_000,
            max_sign_lag: 1_000,
            sign_fit_range: (5, 500),
            impact_bin_ratio: 2.0,
            impact_min_bin_count: 30,
        }
    }
}

/// All stylised facts of one run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FactsReport {
    /// Window measured.
    pub window: (Timestamp, Timestamp),
    /// Spread.
    pub spread: SpreadStats,
    /// Order flow.
    pub flow: OrderFlowStats,
    /// Market-order signs.
    pub signs: SignStats,
    /// Returns.
    pub returns: ReturnStats,
    /// Metaorder impact.
    pub impact: ImpactStats,
}

/// Measure every stylised fact over `cfg`'s window.
pub fn stylized_facts(events: &[Event], metaorders: &[Metaorder], cfg: &FactsConfig) -> FactsReport {
    let path = BookPath::from_events(events);
    FactsReport {
        window: (cfg.from, cfg.to),
        spread: spread_stats(&path, cfg.from, cfg.to, cfg.tick),
        flow: order_flow(events, cfg.from, cfg.to),
        signs: sign_stats(events, cfg.from, cfg.to, cfg.max_sign_lag, cfg.sign_fit_range),
        returns: return_stats(&path, cfg.from, cfg.to, cfg.return_interval_ns, cfg.tick),
        impact: ImpactStats::from_samples(
            impact_samples(&path, metaorders, cfg.from, cfg.to, cfg.tick),
            cfg.impact_bin_ratio,
            cfg.impact_min_bin_count,
        ),
    }
}

/// One target check.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    /// Fact.
    pub name: &'static str,
    /// Target, in words.
    pub target: &'static str,
    /// Measured value, in words.
    pub measured: String,
    /// Whether the target is met.
    pub pass: bool,
}

/// Compare a report with the phase-2 targets (docs/STYLIZED_FACTS.md gives the evidence behind
/// each threshold).
pub fn check_targets(r: &FactsReport) -> Vec<Check> {
    let gamma = r.signs.gamma();
    vec![
        Check {
            name: "cancellations",
            target: "≥ 90% of finished resting orders end cancelled, not filled",
            measured: format!("{:.1}%", 100.0 * r.flow.cancel_share),
            pass: r.flow.cancel_share >= 0.90,
        },
        Check {
            name: "executed share",
            target: "≤ 10% of submitted quantity executes",
            measured: format!("{:.1}% (trade/order volume {:.1}%)", 100.0 * r.flow.executed_share, 100.0 * r.flow.trade_to_order_volume),
            pass: r.flow.executed_share <= 0.10,
        },
        Check {
            name: "spread",
            target: "spread is one tick ≥ 50% of two-sided time",
            measured: format!("{:.1}% at one tick, mean {:.2} ticks", 100.0 * r.spread.share_by_ticks[0], r.spread.mean_ticks),
            pass: r.spread.share_by_ticks[0] >= 0.50,
        },
        Check {
            name: "order-sign memory",
            target: "sign autocorrelation positive at ≥ 95% of lags 1-100, power-law decay γ in [0.3, 0.8]",
            measured: format!(
                "{:.0}% positive, γ = {}",
                100.0 * r.signs.positive_share_1_100,
                gamma.map_or("n/a".to_string(), |g| format!("{g:.2}"))
            ),
            pass: r.signs.positive_share_1_100 >= 0.95 && gamma.is_some_and(|g| (0.3..=0.8).contains(&g)),
        },
        impact_check(&r.impact),
    ]
}

/// The concave-impact target on its own (also usable on samples pooled across runs): I ∝ Q^δ
/// with δ in [0.3, 0.7], fitted over at least three size bins, each with a positive mean impact.
pub fn impact_check(m: &ImpactStats) -> Check {
    let delta = m.fit.map(|f| f.slope);
    let fitted = m.fitted_bins().count();
    let positive = m.fitted_bins().filter(|b| b.mean_impact_ticks > 0.0).count();
    Check {
        name: "concave impact",
        target: "metaorder impact I ∝ Q^δ with δ in [0.3, 0.7] over ≥ 3 size bins, positive in every bin",
        measured: format!(
            "δ = {} (r² {}), {}/{} fitted bins positive",
            delta.map_or("n/a".to_string(), |d| format!("{d:.2}")),
            m.fit.map_or("n/a".to_string(), |f| format!("{:.2}", f.r2)),
            positive,
            fitted
        ),
        pass: delta.is_some_and(|d| (0.3..=0.7).contains(&d)) && fitted >= 3 && positive == fitted,
    }
}

impl fmt::Display for FactsReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = &self.spread;
        writeln!(f, "window {:.0} s to {:.0} s", self.window.0 as f64 / 1e9, self.window.1 as f64 / 1e9)?;
        writeln!(
            f,
            "spread: mean {:.2} ticks; 1/2/3/4/5+ ticks {:.1}/{:.1}/{:.1}/{:.1}/{:.1}% of two-sided time; one-sided {:.2}% of the time",
            s.mean_ticks,
            100.0 * s.share_by_ticks[0],
            100.0 * s.share_by_ticks[1],
            100.0 * s.share_by_ticks[2],
            100.0 * s.share_by_ticks[3],
            100.0 * s.share_by_ticks[4],
            100.0 * s.one_sided_share
        )?;
        let o = &self.flow;
        writeln!(
            f,
            "orders: {} accepted, {} trades; executed share {:.1}%, trade/order volume {:.1}%; resting orders {} ({} cancelled, {} filled, cancel share {:.1}%)",
            o.accepted_orders,
            o.trades,
            100.0 * o.executed_share,
            100.0 * o.trade_to_order_volume,
            o.rested_orders,
            o.rested_cancelled,
            o.rested_filled,
            100.0 * o.cancel_share
        )?;
        let g = &self.signs;
        let lag = |l: usize| g.acf.get(l - 1).copied().unwrap_or(f64::NAN);
        writeln!(
            f,
            "signs: {} market orders ({:.1}% buys); C(1) {:.3}, C(10) {:.3}, C(100) {:.4}; {:.0}% of lags 1-100 positive; γ {} over lags {}-{}",
            g.market_orders,
            100.0 * g.buy_share,
            lag(1),
            lag(10),
            lag(100),
            100.0 * g.positive_share_1_100,
            g.fit.map_or("n/a".to_string(), |p| format!("{:.2} (r² {:.2})", -p.slope, p.r2)),
            g.fit_range.0,
            g.fit_range.1
        )?;
        let r = &self.returns;
        writeln!(
            f,
            "returns ({} ms): {} returns, sd {:.2} ticks, excess kurtosis {:.2}, ACF(1) {:.3}, |r| ACF(1..3) {:.3}/{:.3}/{:.3}, variance ratio(10) {:.2}",
            r.interval_ns / 1_000_000,
            r.returns,
            r.sd_ticks,
            r.excess_kurtosis,
            r.acf1,
            r.abs_acf.first().copied().unwrap_or(f64::NAN),
            r.abs_acf.get(1).copied().unwrap_or(f64::NAN),
            r.abs_acf.get(2).copied().unwrap_or(f64::NAN),
            r.variance_ratio_10
        )?;
        write!(f, "{}", self.impact)
    }
}

impl fmt::Display for ImpactStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "impact: {} metaorders; δ {} over bins ×{} wide with ≥ {} metaorders, weighted by count",
            self.samples.len(),
            self.fit.map_or("n/a".to_string(), |p| format!("{:.2} (r² {:.2}, Y {:.3} ticks)", p.slope, p.r2, p.prefactor)),
            self.bin_ratio,
            self.min_bin_count
        )?;
        for b in &self.bins {
            write!(
                f,
                "\n  Q [{:.0}, {:.0}): mean {:>7.1}, impact {:>6.2} ± {:.2} ticks ({} metaorders{})",
                b.lower_qty,
                b.upper_qty,
                b.mean_qty,
                b.mean_impact_ticks,
                b.stderr_ticks,
                b.count,
                if b.fitted { "" } else { ", not fitted" }
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_law_fit_recovers_the_exponent() {
        let pts: Vec<(f64, f64)> = (1..50).map(|x| (x as f64, 3.0 * (x as f64).powf(-0.5))).collect();
        let fit = power_law_fit(&pts).expect("fit");
        assert!((fit.slope + 0.5).abs() < 1e-9 && (fit.prefactor - 3.0).abs() < 1e-9 && fit.r2 > 0.999_999);
        assert!(power_law_fit(&[(1.0, 1.0), (2.0, -1.0)]).is_none());
    }

    #[test]
    fn impact_bins_sit_on_fixed_log_edges_and_recover_the_exponent() {
        // Sizes 20..=2559 with impact exactly 0.1·√Q: bins [16, 32), [32, 64), ..., [2048, 4096).
        let samples: Vec<ImpactSample> =
            (20..2560).map(|q| ImpactSample { qty: q as f64, impact_ticks: 0.1 * (q as f64).sqrt() }).collect();
        let m = ImpactStats::from_samples(samples, 2.0, 30);
        let edges: Vec<(f64, f64)> = m.bins.iter().map(|b| (b.lower_qty, b.upper_qty)).collect();
        assert_eq!(edges, [(16.0, 32.0), (32.0, 64.0), (64.0, 128.0), (128.0, 256.0), (256.0, 512.0), (512.0, 1024.0), (1024.0, 2048.0), (2048.0, 4096.0)]);
        assert_eq!(m.bins[0].count, 12); // 20..=31: below the minimum, reported but not fitted
        assert!(!m.bins[0].fitted && m.bins[1..].iter().all(|b| b.fitted));
        assert_eq!(m.bins.iter().map(|b| b.count).sum::<usize>(), 2540);
        let fit = m.fit.expect("fit");
        assert!((fit.slope - 0.5).abs() < 0.01, "δ {}", fit.slope);
        assert!(impact_check(&m).pass);
        // A linear law fails the concavity target.
        let linear: Vec<ImpactSample> = (20..2560).map(|q| ImpactSample { qty: q as f64, impact_ticks: 0.01 * q as f64 }).collect();
        let m = ImpactStats::from_samples(linear, 2.0, 30);
        assert!((m.fit.expect("fit").slope - 1.0).abs() < 0.01 && !impact_check(&m).pass);
        // A negative fitted bin fails it too.
        let mut s: Vec<ImpactSample> = (20..2560).map(|q| ImpactSample { qty: q as f64, impact_ticks: 0.1 * (q as f64).sqrt() }).collect();
        s.iter_mut().filter(|x| (64.0..128.0).contains(&x.qty)).for_each(|x| x.impact_ticks = -1.0);
        assert!(!impact_check(&ImpactStats::from_samples(s, 2.0, 30)).pass);
    }

    #[test]
    fn weighted_power_law_fit_follows_the_heavy_points() {
        // Two exact power laws mixed: heavy points on slope 0.5, light outliers far off it.
        let mut pts: Vec<(f64, f64, f64)> = (1..20).map(|x| (x as f64, (x as f64).sqrt(), 1_000.0)).collect();
        pts.extend((1..20).map(|x| (x as f64, (x as f64).powi(2), 0.001)));
        let fit = power_law_fit_weighted(&pts).expect("fit");
        assert!((fit.slope - 0.5).abs() < 0.01, "slope {}", fit.slope);
        assert!(power_law_fit_weighted(&[(1.0, 1.0, 1.0), (2.0, 2.0, 0.0), (3.0, 3.0, 1.0)]).is_none());
    }

    #[test]
    fn autocorrelation_of_an_alternating_series() {
        let x: Vec<f64> = (0..1000).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let acf = autocorrelation(&x, 2);
        assert!((acf[0] + 1.0).abs() < 0.01 && (acf[1] - 1.0).abs() < 0.01);
    }
}
