//! Market-quality, execution and market-maker metrics (phase 4). Every function reads the
//! matching engine's own event log (and, for races, the gateway's own answers), never an agent's
//! view. Definitions, sources and verification status: docs/METRICS.md.
//!
//! Mids are carried as twice the mid (an integer, since prices are), so that quantities built
//! from them can be summed exactly.

use std::collections::BTreeMap;

use lob::{CancelReason, DetHashMap, Event, EventKind, Liquidity, OrderId, OwnerId, Price, Qty, RejectReason, RequestKind, Side, TimeInForce, Timestamp};

use crate::BookPath;

fn sign(side: Side) -> i64 {
    match side {
        Side::Buy => 1,
        Side::Sell => -1,
    }
}

/// What the engine said about one order when it accepted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderInfo {
    /// Owner.
    pub owner: OwnerId,
    /// Client order id (the gateway passes it as the client tag).
    pub client_tag: u64,
    /// Side.
    pub side: Side,
    /// Limit price (`None` for market orders).
    pub price: Option<Price>,
    /// Quantity.
    pub qty: Qty,
    /// Time in force.
    pub tif: TimeInForce,
    /// Acceptance time.
    pub ts: Timestamp,
}

/// Every accepted order by exchange id, and by (owner, client tag).
#[derive(Clone, Debug, Default)]
pub struct OrderIndex {
    /// By exchange order id.
    pub by_id: DetHashMap<OrderId, OrderInfo>,
    /// Exchange order id by (owner, client tag).
    pub by_client: DetHashMap<(OwnerId, u64), OrderId>,
}

impl OrderIndex {
    /// Index the `Accepted` events of a log.
    pub fn from_events(events: &[Event]) -> Self {
        let mut ix = OrderIndex::default();
        for e in events {
            if let EventKind::Accepted { order_id, owner, client_tag, side, price, qty, tif } = e.kind {
                ix.by_id.insert(order_id, OrderInfo { owner, client_tag, side, price, qty, tif, ts: e.ts });
                ix.by_client.insert((owner, client_tag), order_id);
            }
        }
        ix
    }
}

// ---------------------------------------------------------------------------------------------
// Execution spreads (SEC Rule 600(b) definitions)

/// Share-weighted execution-quality averages over aggressive (taker) executions, as SEC Rule
/// 600(b) defines them (17 CFR 242.600(b), read on eCFR):
/// * the effective spread is 2·(P − M) for buys and 2·(M − P) for sells, with M the midpoint at
///   order receipt;
/// * the realized spread is the same with the midpoint a horizon after execution (or the final
///   midpoint of the window if that comes sooner);
/// * the quoted spread is the best ask minus the best bid at receipt.
///
/// Percentage versions divide the average spread by the average midpoint (a ratio of averages, as
/// the rule says). "Receipt" is the matching engine's processing time: the mid is the one the
/// order met.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecutionSpreads {
    /// Horizons of the realized spreads, nanoseconds.
    pub horizons_ns: Vec<i64>,
    /// Executions counted.
    pub executions: usize,
    /// Shares executed.
    pub shares: Qty,
    /// Average quoted spread, price units.
    pub quoted: f64,
    /// Average midpoint at receipt, price units.
    pub midpoint: f64,
    /// Average effective spread, price units.
    pub effective: f64,
    /// Average realized spread at each horizon, price units.
    pub realized: Vec<f64>,
}

impl ExecutionSpreads {
    /// Average percentage effective spread (fraction of the average midpoint).
    pub fn percentage_effective(&self) -> f64 {
        self.effective / self.midpoint
    }

    /// Average percentage realized spread at horizon `k`.
    pub fn percentage_realized(&self, k: usize) -> f64 {
        self.realized[k] / self.midpoint
    }

    /// Price impact at horizon `k`: effective minus realized spread, price units.
    pub fn price_impact(&self, k: usize) -> f64 {
        self.effective - self.realized[k]
    }
}

/// Execution spreads of taker executions in [`from`, `to`], optionally only for orders of `owners`.
pub fn execution_spreads(events: &[Event], path: &BookPath, ix: &OrderIndex, from: Timestamp, to: Timestamp, horizons_ns: &[i64], owners: Option<&[OwnerId]>) -> ExecutionSpreads {
    let mut out = ExecutionSpreads { horizons_ns: horizons_ns.to_vec(), realized: vec![0.0; horizons_ns.len()], ..Default::default() };
    let Some(final_mid2) = path.mid2_at(to) else { return out };
    let (mut q_sum, mut quoted, mut mid2_sum, mut eff) = (0i128, 0i128, 0i128, 0i128);
    let mut real = vec![0i128; horizons_ns.len()];
    for e in events.iter().filter(|e| e.ts >= from && e.ts <= to) {
        let EventKind::Fill { order_id, owner, price, qty, liquidity: Liquidity::Taker, .. } = e.kind else { continue };
        if owners.is_some_and(|o| !o.contains(&owner)) {
            continue;
        }
        let Some(info) = ix.by_id.get(&order_id) else { continue };
        let (b, a) = path.at(e.ts - 1);
        let (Some(b), Some(a)) = (b, a) else { continue };
        let (d, q, m2) = (sign(info.side), i128::from(qty), i128::from(b + a));
        q_sum += q;
        quoted += i128::from(a - b) * q;
        mid2_sum += m2 * q;
        eff += i128::from(d) * (2 * i128::from(price) - m2) * q;
        for (r, &h) in real.iter_mut().zip(horizons_ns) {
            let mh = if e.ts + h <= to { path.mid2_at(e.ts + h).unwrap_or(final_mid2) } else { final_mid2 };
            *r += i128::from(d) * (2 * i128::from(price) - i128::from(mh)) * q;
        }
        out.executions += 1;
    }
    if q_sum == 0 {
        return out;
    }
    let n = q_sum as f64;
    out.shares = q_sum as Qty;
    out.quoted = quoted as f64 / n;
    out.midpoint = mid2_sum as f64 / (2.0 * n);
    out.effective = eff as f64 / n;
    out.realized = real.iter().map(|&r| r as f64 / n).collect();
    out
}

// ---------------------------------------------------------------------------------------------
// Market-maker PnL decomposition

/// Fee rates per unit of quantity, in fee units (`scale` fee units = one price unit).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeeRates {
    /// Passive executions.
    pub maker: i64,
    /// Aggressive executions.
    pub taker: i64,
    /// Auction executions.
    pub auction: i64,
    /// Fee units per price unit.
    pub scale: i64,
}

/// One owner's PnL split into parts that sum exactly to its mark-to-market PnL after fees
/// (ARCHITECTURE §8). For each fill with direction D (+1 buy), price P, quantity Q, mid M before
/// the fill, mid Mₕ a horizon later (capped at the mark time) and mark mid M_T:
/// * spread capture = D(M − P)Q;
/// * adverse selection = D(Mₕ − M)Q;
/// * inventory revaluation = D(M_T − Mₕ)Q;
/// * fees as charged.
///
/// The three price terms telescope to D(M_T − P)Q, which is the fill's contribution to cash plus
/// position marked at M_T, so the identity is exact. All amounts are integers in half fee units
/// (1/(2·scale) price units × quantity).
///
/// Over a window (from, T] ([`pnl_decomposition_window`]) only fills inside the window are split,
/// and the position held at `from` adds a carried term pos(from)·(M_T − M(from)), so the parts sum
/// exactly to PnL(T) − PnL(from).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PnlDecomposition {
    /// Horizon, nanoseconds.
    pub horizon_ns: i64,
    /// Fills.
    pub fills: usize,
    /// Quantity traded.
    pub volume: Qty,
    /// Quantity traded passively (maker fills).
    pub maker_volume: Qty,
    /// Position at the window start (0 for a whole-run decomposition).
    pub opening_position: Qty,
    /// Position at the mark time.
    pub position: Qty,
    /// Spread capture.
    pub spread_capture: i128,
    /// Adverse selection (negative when prices moved against the fills).
    pub adverse_selection: i128,
    /// Inventory revaluation.
    pub inventory: i128,
    /// Revaluation of the position held at the window start (0 for a whole-run decomposition).
    pub carried: i128,
    /// Fees paid (negative = rebates).
    pub fees: i128,
    /// Total: spread capture + adverse selection + inventory + carried − fees.
    pub total: i128,
    /// Half fee units per price unit (2·scale).
    pub units_per_price: i128,
}

impl PnlDecomposition {
    /// Convert an amount to price units × quantity.
    pub fn price_units(&self, x: i128) -> f64 {
        x as f64 / self.units_per_price as f64
    }
}

/// PnL decomposition of `owner`'s fills up to `mark_time`. `None` if the book was never two-sided
/// by then (nothing to mark at).
pub fn pnl_decomposition(events: &[Event], path: &BookPath, owner: OwnerId, mark_time: Timestamp, horizon_ns: i64, fees: FeeRates) -> Option<PnlDecomposition> {
    decompose(events, path, owner, None, mark_time, horizon_ns, fees)
}

/// PnL decomposition over the window (`from`, `mark_time`]: the parts sum exactly to the change
/// in mark-to-market PnL after fees, PnL(T) − PnL(from), where PnL(t) = cash(t) + position(t)·M(t)
/// − fees(t) counts fills at or before t. A fill exactly at `from` belongs before the window.
/// `None` if the book was never two-sided by `mark_time`, or the owner held a position at `from`
/// before the book had ever been two-sided.
pub fn pnl_decomposition_window(
    events: &[Event],
    path: &BookPath,
    owner: OwnerId,
    from: Timestamp,
    mark_time: Timestamp,
    horizon_ns: i64,
    fees: FeeRates,
) -> Option<PnlDecomposition> {
    decompose(events, path, owner, Some(from), mark_time, horizon_ns, fees)
}

fn decompose(events: &[Event], path: &BookPath, owner: OwnerId, from: Option<Timestamp>, mark_time: Timestamp, horizon_ns: i64, fees: FeeRates) -> Option<PnlDecomposition> {
    let m_t = i128::from(path.mid2_at(mark_time)?);
    let scale = i128::from(fees.scale);
    let mut sides: DetHashMap<OrderId, Side> = DetHashMap::default();
    let mut out = PnlDecomposition { horizon_ns, units_per_price: 2 * scale, ..Default::default() };
    for e in events.iter().filter(|e| e.ts <= mark_time) {
        match e.kind {
            EventKind::Accepted { order_id, owner: o, side, .. } if o == owner => {
                sides.insert(order_id, side);
            }
            EventKind::Fill { order_id, owner: o, price, qty, liquidity, .. } if o == owner => {
                let d = i128::from(sign(sides[&order_id]));
                if from.is_some_and(|f| e.ts <= f) {
                    out.opening_position += (d as Qty) * qty;
                    continue;
                }
                let (p2, q) = (2 * i128::from(price), i128::from(qty));
                // Any choice of M and Mₕ keeps the identity exact; these fall back to "no edge"
                // when the book was one-sided.
                let m0 = path.mid2_at(e.ts - 1).map_or(p2, i128::from);
                let mh = path.mid2_at((e.ts + horizon_ns).min(mark_time)).map_or(m0, i128::from);
                out.spread_capture += d * (m0 - p2) * q * scale;
                out.adverse_selection += d * (mh - m0) * q * scale;
                out.inventory += d * (m_t - mh) * q * scale;
                let rate = match liquidity {
                    Liquidity::Maker => fees.maker,
                    Liquidity::Taker => fees.taker,
                    Liquidity::Auction => fees.auction,
                };
                out.fees += 2 * i128::from(rate) * q;
                out.fills += 1;
                out.volume += qty;
                if liquidity == Liquidity::Maker {
                    out.maker_volume += qty;
                }
                out.position += (d as Qty) * qty;
            }
            _ => {}
        }
    }
    if let Some(f) = from {
        if out.opening_position != 0 {
            let m_from = i128::from(path.mid2_at(f)?);
            out.carried = i128::from(out.opening_position) * (m_t - m_from) * scale;
        }
        out.position += out.opening_position;
    }
    out.total = out.spread_capture + out.adverse_selection + out.inventory + out.carried - out.fees;
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Implementation shortfall

/// Implementation shortfall of a parent order against its decision price (Perold 1988), split into
/// delay, execution and opportunity costs. In half price units × quantity, positive = cost:
/// * delay = D(arrival − decision)·target;
/// * execution = D·Σ(pᵢ − arrival)·qᵢ;
/// * opportunity = D(end − arrival)·unfilled.
///
/// The three sum exactly to D·[Σ(pᵢ − decision)·qᵢ + (end − decision)·unfilled].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shortfall {
    /// Delay cost.
    pub delay: i128,
    /// Execution cost.
    pub execution: i128,
    /// Opportunity cost of the unfilled part.
    pub opportunity: i128,
    /// Total.
    pub total: i128,
}

/// Shortfall of a parent on `side` with `target` quantity; prices of the decision, arrival and end
/// are passed doubled (half-tick mids are integers), fills as (price, quantity).
pub fn implementation_shortfall(side: Side, decision2: i64, arrival2: i64, fills: &[(Price, Qty)], target: Qty, end2: i64) -> Shortfall {
    let d = i128::from(sign(side));
    let filled: i128 = fills.iter().map(|f| i128::from(f.1)).sum();
    let unfilled = (i128::from(target) - filled).max(0);
    let execution = d * fills.iter().map(|&(p, q)| (2 * i128::from(p) - i128::from(arrival2)) * i128::from(q)).sum::<i128>();
    let delay = d * (i128::from(arrival2) - i128::from(decision2)) * i128::from(target).max(filled);
    let opportunity = d * (i128::from(end2) - i128::from(arrival2)) * unfilled;
    let total = d * (fills.iter().map(|&(p, q)| (2 * i128::from(p) - i128::from(decision2)) * i128::from(q)).sum::<i128>()
        + (i128::from(end2) - i128::from(decision2)) * unfilled);
    Shortfall { delay, execution, opportunity, total }
}

// ---------------------------------------------------------------------------------------------
// Liquidity and volatility

fn sampled_mid2(path: &BookPath, from: Timestamp, to: Timestamp, interval_ns: i64) -> Vec<(Timestamp, i64)> {
    let mut v = Vec::new();
    let mut t = from;
    while t <= to {
        if let Some(m) = path.mid2_at(t) {
            v.push((t, m));
        }
        t += interval_ns;
    }
    v
}

/// Realized volatility of the mid sampled every `interval_ns`: √(Σr²/elapsed), ticks per √second.
pub fn realized_volatility(path: &BookPath, from: Timestamp, to: Timestamp, interval_ns: i64, tick: Price) -> Option<f64> {
    let s = sampled_mid2(path, from, to, interval_ns);
    if s.len() < 2 {
        return None;
    }
    let sum_sq: f64 = s.windows(2).map(|w| ((w[1].1 - w[0].1) as f64 / (2.0 * tick as f64)).powi(2)).sum();
    let elapsed = (s[s.len() - 1].0 - s[0].0) as f64 / 1e9;
    Some((sum_sq / elapsed).sqrt())
}

/// Amihud (2002) illiquidity on intervals of `interval_ns`: the mean, over intervals with trading,
/// of |relative mid change| ÷ traded value (price units × quantity).
pub fn amihud(events: &[Event], path: &BookPath, from: Timestamp, to: Timestamp, interval_ns: i64) -> Option<f64> {
    let s = sampled_mid2(path, from, to, interval_ns);
    let mut vals = Vec::new();
    let mut k = 0usize;
    let trades: Vec<(Timestamp, f64)> = events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Trade { price, qty, .. } if e.ts > from && e.ts <= to => Some((e.ts, price as f64 * qty as f64)),
            _ => None,
        })
        .collect();
    for w in s.windows(2) {
        let mut value = 0.0;
        while k < trades.len() && trades[k].0 <= w[1].0 {
            if trades[k].0 > w[0].0 {
                value += trades[k].1;
            }
            k += 1;
        }
        if value > 0.0 {
            vals.push(((w[1].1 - w[0].1) as f64 / w[0].1 as f64).abs() / value);
        }
    }
    (!vals.is_empty()).then(|| vals.iter().sum::<f64>() / vals.len() as f64)
}

/// Roll (1984) implied spread from consecutive trade prices: 2·√(−γ₁) with γ₁ the first-order
/// autocovariance of price changes (mean taken over all changes), `None` when γ₁ ≥ 0.
pub fn roll_spread(events: &[Event], from: Timestamp, to: Timestamp) -> Option<f64> {
    let prices: Vec<f64> = events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Trade { price, .. } if e.ts >= from && e.ts <= to => Some(price as f64),
            _ => None,
        })
        .collect();
    let dp: Vec<f64> = prices.windows(2).map(|w| w[1] - w[0]).collect();
    if dp.len() < 3 {
        return None;
    }
    let mean = dp.iter().sum::<f64>() / dp.len() as f64;
    let g1 = dp.windows(2).map(|w| (w[1] - mean) * (w[0] - mean)).sum::<f64>() / (dp.len() - 1) as f64;
    (g1 < 0.0).then(|| 2.0 * (-g1).sqrt())
}

// ---------------------------------------------------------------------------------------------
// Order-flow imbalance

/// Linear fit of mid changes on order-flow imbalance over fixed intervals (Cont, Kukanov & Stoikov
/// 2014): ΔP_k = α + β·OFI_k, with OFI_k the sum of eₙ over consecutive best-quote observations
/// in the interval, ΔP_k in ticks, and the depth the average of (bid size + ask size)/2 over
/// those observations.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OfiFit {
    /// Interval length, nanoseconds.
    pub interval_ns: i64,
    /// Intervals used.
    pub intervals: usize,
    /// Slope, ticks per share of OFI.
    pub beta: f64,
    /// Intercept, ticks.
    pub intercept: f64,
    /// Coefficient of determination.
    pub r2: f64,
    /// Average top-of-book depth, shares.
    pub depth: f64,
}

/// OFI contribution eₙ of a change of best quotes (prices and sizes), as in CKS §2.1.
pub fn ofi_contribution(prev: (Price, Qty, Price, Qty), next: (Price, Qty, Price, Qty)) -> i64 {
    let (pb, pqb, pa, pqa) = prev;
    let (nb, nqb, na, nqa) = next;
    let mut e = 0;
    if nb >= pb {
        e += nqb;
    }
    if nb <= pb {
        e -= pqb;
    }
    if na <= pa {
        e -= nqa;
    }
    if na >= pa {
        e += pqa;
    }
    e
}

/// Fit ΔP on OFI over intervals of `interval_ns` in [`from`, `to`].
pub fn ofi_regression(path: &BookPath, from: Timestamp, to: Timestamp, interval_ns: i64, tick: Price) -> Option<OfiFit> {
    let mut pts: Vec<(f64, f64)> = Vec::new();
    let mut depth_sum = 0.0;
    let mut depth_n = 0usize;
    let start = path.times.partition_point(|&x| x < from);
    let mut k = start;
    let mut t0 = from;
    let top = |j: usize| -> Option<(Price, Qty, Price, Qty)> { Some((path.bids[j]?, path.bid_qty[j], path.asks[j]?, path.ask_qty[j])) };
    let mut prev = if start > 0 { top(start - 1) } else { None };
    while t0 + interval_ns <= to {
        let t1 = t0 + interval_ns;
        let mut ofi = 0i64;
        while k < path.times.len() && path.times[k] <= t1 {
            if let Some(cur) = top(k) {
                if let Some(p) = prev {
                    ofi += ofi_contribution(p, cur);
                }
                depth_sum += (cur.1 + cur.3) as f64 / 2.0;
                depth_n += 1;
                prev = Some(cur);
            }
            k += 1;
        }
        if let (Some(m0), Some(m1)) = (path.mid2_at(t0), path.mid2_at(t1)) {
            pts.push((ofi as f64, (m1 - m0) as f64 / (2.0 * tick as f64)));
        }
        t0 = t1;
    }
    let n = pts.len();
    if n < 3 {
        return None;
    }
    let (mx, my) = (pts.iter().map(|p| p.0).sum::<f64>() / n as f64, pts.iter().map(|p| p.1).sum::<f64>() / n as f64);
    let sxx: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
    let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let syy: f64 = pts.iter().map(|p| (p.1 - my).powi(2)).sum();
    if sxx == 0.0 {
        return None;
    }
    let beta = sxy / sxx;
    let r2 = if syy > 0.0 { beta * beta * sxx / syy } else { 0.0 };
    Some(OfiFit { interval_ns, intervals: n, beta, intercept: my - beta * mx, r2, depth: if depth_n > 0 { depth_sum / depth_n as f64 } else { 0.0 } })
}

// ---------------------------------------------------------------------------------------------
// Fill probability against queue position

/// Resting orders grouped by the quantity ahead of them when they joined their price level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueBucket {
    /// Smallest quantity ahead in the bucket.
    pub min_ahead: Qty,
    /// Largest quantity ahead in the bucket (`None`: no upper bound).
    pub max_ahead: Option<Qty>,
    /// Orders that finished (filled or cancelled) by the end of the window.
    pub orders: usize,
    /// … of which at least partly filled.
    pub any_fill: usize,
    /// … of which completely filled.
    pub complete: usize,
}

impl QueueBucket {
    /// Share of orders at least partly filled.
    pub fn any_fill_share(&self) -> f64 {
        if self.orders > 0 { self.any_fill as f64 / self.orders as f64 } else { 0.0 }
    }
}

/// Fill outcome of orders that started resting in [`from`, `to`] and finished by `to`, bucketed by
/// queue ahead at entry (bucket edges are lower bounds, e.g. `[0, 1, 50, 200]`). Orders still
/// resting at `to` are left out (censored). `owner` restricts to one participant; `touch_only`
/// to orders that joined or improved the best price on their side when they arrived.
pub fn fill_probability_by_queue(
    events: &[Event],
    ix: &OrderIndex,
    from: Timestamp,
    to: Timestamp,
    owner: Option<OwnerId>,
    edges: &[Qty],
    touch_only: bool,
) -> Vec<QueueBucket> {
    struct Resting {
        side: Side,
        price: Price,
        qty: Qty,
        ahead: Qty,
        tracked: bool,
        filled: bool,
    }
    let mut buckets: Vec<QueueBucket> =
        edges.iter().enumerate().map(|(k, &lo)| QueueBucket { min_ahead: lo, max_ahead: edges.get(k + 1).map(|h| h - 1), ..Default::default() }).collect();
    // Quantity per price level, one ordered map per side, so the best price is known.
    let mut levels: [BTreeMap<Price, Qty>; 2] = [BTreeMap::new(), BTreeMap::new()];
    let idx = |s: Side| usize::from(s == Side::Sell);
    let change = |levels: &mut [BTreeMap<Price, Qty>; 2], side: Side, price: Price, delta: Qty| {
        let m = &mut levels[idx(side)];
        let q = m.entry(price).or_insert(0);
        *q += delta;
        if *q <= 0 {
            m.remove(&price);
        }
    };
    let mut live: DetHashMap<OrderId, Resting> = DetHashMap::default();
    let finish = |r: &Resting, complete: bool, buckets: &mut Vec<QueueBucket>| {
        if r.tracked {
            if let Some(b) = buckets.iter_mut().rev().find(|b| r.ahead >= b.min_ahead) {
                b.orders += 1;
                b.any_fill += usize::from(r.filled || complete);
                b.complete += usize::from(complete);
            }
        }
    };
    for e in events.iter().filter(|e| e.ts <= to) {
        match e.kind {
            EventKind::BookAdd { order_id, side, price: Some(price), qty } => {
                let ahead = levels[idx(side)].get(&price).copied().unwrap_or(0);
                let at_touch = match side {
                    Side::Buy => levels[0].keys().next_back().is_none_or(|&b| price >= b),
                    Side::Sell => levels[1].keys().next().is_none_or(|&a| price <= a),
                };
                let tracked = e.ts >= from
                    && (!touch_only || at_touch)
                    && owner.is_none_or(|o| ix.by_id.get(&order_id).is_some_and(|i| i.owner == o));
                live.insert(order_id, Resting { side, price, qty, ahead, tracked, filled: false });
                change(&mut levels, side, price, qty);
            }
            EventKind::BookUpdate { order_id, qty } => {
                if let Some(r) = live.get_mut(&order_id) {
                    change(&mut levels, r.side, r.price, qty - r.qty);
                    r.qty = qty;
                }
            }
            EventKind::BookRemove { order_id } => {
                if let Some(r) = live.remove(&order_id) {
                    change(&mut levels, r.side, r.price, -r.qty);
                    finish(&r, false, &mut buckets);
                }
            }
            EventKind::Trade { qty, aggressor, buy_order_id, sell_order_id, .. } => {
                let passive: Vec<OrderId> = match aggressor {
                    Some(Side::Buy) => vec![sell_order_id],
                    Some(Side::Sell) => vec![buy_order_id],
                    None => vec![buy_order_id, sell_order_id],
                };
                for id in passive {
                    if let Some(r) = live.get_mut(&id) {
                        change(&mut levels, r.side, r.price, -qty);
                        r.qty -= qty;
                        r.filled = true;
                        if r.qty == 0 {
                            let r = live.remove(&id).expect("present");
                            finish(&r, true, &mut buckets);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    buckets
}

// ---------------------------------------------------------------------------------------------
// Latency races

/// One message that can take part in a race: an immediate-or-cancel order at a limit price (a
/// "take" aimed at resting quotes at that price) or a cancel of a resting quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RaceMessage {
    /// Time the exchange handled it (engine, or gateway for a cancel it refused itself).
    pub ts: Timestamp,
    /// Sender.
    pub owner: OwnerId,
    /// Quotes it aimed at: (side of the resting quotes, price).
    pub target: (Side, Price),
    /// Whether it succeeded (traded, or cancelled a live order).
    pub success: bool,
    /// Take (true) or cancel (false).
    pub take: bool,
}

/// Latency races (after Aquilina, Budish & O'Neill 2022): within `window_ns` of a first message,
/// messages from at least two participants aimed at the same quotes, at least one succeeding and
/// at least one failing. The duration runs from the first success to the first failure. Window
/// and participant rule are this project's operationalisation (docs/METRICS.md).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RaceReport {
    /// Window, nanoseconds.
    pub window_ns: i64,
    /// Messages considered.
    pub messages: usize,
    /// Races found.
    pub races: usize,
    /// Races won by a take.
    pub won_by_take: usize,
    /// Races won by a cancel.
    pub won_by_cancel: usize,
    /// Mean duration (first success to first failure), nanoseconds.
    pub mean_duration_ns: f64,
    /// Races won, by owner.
    pub wins: BTreeMap<OwnerId, usize>,
    /// Failed messages in races, by owner.
    pub losses: BTreeMap<OwnerId, usize>,
}

/// Collect race messages: takes and cancels from the engine log, plus cancels the gateway refused
/// itself, given as (time, owner, client order id).
pub fn race_messages(events: &[Event], ix: &OrderIndex, gateway_failed_cancels: &[(Timestamp, OwnerId, u64)], from: Timestamp, to: Timestamp) -> Vec<RaceMessage> {
    let opposite = |s: Side| if s == Side::Buy { Side::Sell } else { Side::Buy };
    let mut traded: DetHashMap<OrderId, bool> = DetHashMap::default();
    for e in events {
        if let EventKind::Fill { order_id, liquidity: Liquidity::Taker, .. } = e.kind {
            traded.insert(order_id, true);
        }
    }
    let mut out = Vec::new();
    for e in events.iter().filter(|e| e.ts >= from && e.ts <= to) {
        match e.kind {
            EventKind::Accepted { order_id, owner, side, price: Some(p), tif: TimeInForce::Ioc | TimeInForce::Fok, .. } => {
                out.push(RaceMessage { ts: e.ts, owner, target: (opposite(side), p), success: traded.contains_key(&order_id), take: true });
            }
            EventKind::Cancelled { order_id, owner, reason: CancelReason::Requested | CancelReason::MassCancel, .. } => {
                if let Some(OrderInfo { side, price: Some(p), .. }) = ix.by_id.get(&order_id).copied() {
                    out.push(RaceMessage { ts: e.ts, owner, target: (side, p), success: true, take: false });
                }
            }
            EventKind::RequestRejected { order_id, owner, request: RequestKind::Cancel, reason: RejectReason::UnknownOrder } => {
                if let Some(OrderInfo { side, price: Some(p), .. }) = ix.by_id.get(&order_id).copied() {
                    out.push(RaceMessage { ts: e.ts, owner, target: (side, p), success: false, take: false });
                }
            }
            _ => {}
        }
    }
    for &(ts, owner, client) in gateway_failed_cancels.iter().filter(|g| g.0 >= from && g.0 <= to) {
        if let Some(info) = ix.by_client.get(&(owner, client)).and_then(|id| ix.by_id.get(id)) {
            if let Some(p) = info.price {
                out.push(RaceMessage { ts, owner, target: (info.side, p), success: false, take: false });
            }
        }
    }
    out.sort_by_key(|m| (m.ts, m.owner));
    out
}

/// Find races among `messages` (any order).
pub fn latency_races(messages: &[RaceMessage], window_ns: i64) -> RaceReport {
    let mut by_target: BTreeMap<(u8, Price), Vec<RaceMessage>> = BTreeMap::new();
    for m in messages {
        by_target.entry((u8::from(m.target.0 == Side::Sell), m.target.1)).or_default().push(*m);
    }
    let mut out = RaceReport { window_ns, messages: messages.len(), ..Default::default() };
    let mut durations = Vec::new();
    for msgs in by_target.values_mut() {
        msgs.sort_by_key(|m| (m.ts, m.owner));
        let mut i = 0;
        while i < msgs.len() {
            let end = msgs[i..].partition_point(|m| m.ts <= msgs[i].ts + window_ns) + i;
            let group = &msgs[i..end];
            let mut owners: Vec<OwnerId> = group.iter().map(|m| m.owner).collect();
            owners.sort_unstable();
            owners.dedup();
            let first_success = group.iter().find(|m| m.success);
            let first_fail = group.iter().find(|m| !m.success);
            if let (true, Some(s), Some(f)) = (owners.len() >= 2, first_success, first_fail) {
                out.races += 1;
                if s.take {
                    out.won_by_take += 1;
                } else {
                    out.won_by_cancel += 1;
                }
                *out.wins.entry(s.owner).or_insert(0) += 1;
                for m in group.iter().filter(|m| !m.success) {
                    *out.losses.entry(m.owner).or_insert(0) += 1;
                }
                durations.push((f.ts - s.ts) as f64);
                i = end;
            } else {
                i += 1;
            }
        }
    }
    if !durations.is_empty() {
        out.mean_duration_ns = durations.iter().sum::<f64>() / durations.len() as f64;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lob::{Command, InstrumentSpec, MatchingEngine, NewOrder, Phase};

    const S: i64 = 1_000_000_000;

    /// A scripted market: run commands through the real matching engine.
    fn run(cmds: &[(Timestamp, Command)]) -> Vec<Event> {
        let spec = InstrumentSpec { tick_size: 1, lot_size: 1, min_price: 1, max_price: 1_000_000, max_order_qty: 1_000_000, reference_price: 100 };
        let mut eng = MatchingEngine::new(spec, Phase::Continuous).expect("valid spec");
        let mut out = Vec::new();
        for (t, c) in cmds {
            eng.process(*t, c, &mut out).expect("valid command");
        }
        out
    }

    fn lim(owner: OwnerId, side: Side, price: Price, qty: Qty) -> Command {
        Command::New(NewOrder::limit(owner, side, price, qty))
    }

    fn ioc(owner: OwnerId, side: Side, price: Price, qty: Qty) -> Command {
        Command::New(NewOrder::limit(owner, side, price, qty).with_tif(TimeInForce::Ioc))
    }

    /// Owner 1 quotes 98 / 102 (mid 100). At 2 µs owner 2 buys 10 at 102. At 1 s owner 3 bids 50
    /// at 100 (mid 101). At 10 s owner 4 sells 5 into that bid.
    fn quoted_market() -> Vec<Event> {
        run(&[
            (1_000, lim(1, Side::Buy, 98, 100)),
            (1_000, lim(1, Side::Sell, 102, 100)),
            (2_000, ioc(2, Side::Buy, 102, 10)),
            (S, lim(3, Side::Buy, 100, 50)),
            (10 * S, ioc(4, Side::Sell, 98, 5)),
        ])
    }

    #[test]
    fn execution_spreads_follow_rule_600() {
        let ev = quoted_market();
        let (path, ix) = (BookPath::from_events(&ev), OrderIndex::from_events(&ev));
        let s = execution_spreads(&ev, &path, &ix, 0, 20 * S, &[50_000_000, 5 * S], None);
        // Buy 10 at 102: mid 100, quoted 4, effective 2·(102 − 100) = 4; realized at 50 ms (mid
        // still 100) 4, at 5 s (mid 101) 2. Sell 5 at 100: mid 101, quoted 2, effective 2; after
        // it the bid is 100 (45 left), so the mid stays 101 and the realized spread is 2 at both
        // horizons (10 s + 5 s is inside the 20 s window).
        assert_eq!((s.executions, s.shares), (2, 15));
        assert!((s.quoted - (10.0 * 4.0 + 5.0 * 2.0) / 15.0).abs() < 1e-12);
        assert!((s.effective - (10.0 * 4.0 + 5.0 * 2.0) / 15.0).abs() < 1e-12);
        assert!((s.realized[0] - (10.0 * 4.0 + 5.0 * 2.0) / 15.0).abs() < 1e-12);
        assert!((s.realized[1] - (10.0 * 2.0 + 5.0 * 2.0) / 15.0).abs() < 1e-12);
        assert!((s.price_impact(1) - (10.0 * 2.0) / 15.0).abs() < 1e-12);
        assert!((s.midpoint - (10.0 * 100.0 + 5.0 * 101.0) / 15.0).abs() < 1e-12);
        assert!((s.percentage_effective() - s.effective / s.midpoint).abs() < 1e-15);
        // Only owner 4's execution.
        let s4 = execution_spreads(&ev, &path, &ix, 0, 20 * S, &[S], Some(&[4]));
        assert_eq!((s4.shares, s4.effective), (5, 2.0));
        // A horizon past the window end uses the window's final midpoint: the window ends at 0.5 s
        // (mid 100), before the move to 101 at 1 s.
        let short = execution_spreads(&ev, &path, &ix, 0, S / 2, &[5 * S], Some(&[2]));
        assert_eq!(short.realized[0], 4.0);
    }

    #[test]
    fn pnl_decomposition_is_exact_and_matches_hand_computation() {
        let ev = quoted_market();
        let path = BookPath::from_events(&ev);
        let fees = FeeRates { maker: -20, taker: 30, auction: 0, scale: 10_000 };
        // Owner 1 sold 10 at 102 (maker) with the mid at 100; mid 101 from 1 s; marked at 20 s.
        let short = pnl_decomposition(&ev, &path, 1, 20 * S, S / 2, fees).expect("decomposition");
        let unit = |x: i128| short.price_units(x);
        assert_eq!(unit(short.spread_capture), 20.0, "−1 × (100 − 102) × 10");
        assert_eq!(unit(short.adverse_selection), 0.0, "mid unchanged half a second later");
        assert_eq!(unit(short.inventory), -10.0, "−1 × (101 − 100) × 10");
        assert_eq!(short.fees, 2 * -20 * 10, "maker rebate: −20 fee units × 10 shares, in half fee units");
        // 20 + 0 − 10 price units, plus the 200 fee-unit rebate (0.02 price units).
        assert!((unit(short.total) - 10.02).abs() < 1e-12, "total {}", unit(short.total));
        let long = pnl_decomposition(&ev, &path, 1, 20 * S, 5 * S, fees).expect("decomposition");
        assert_eq!((unit(long.adverse_selection), unit(long.inventory)), (-10.0, 0.0), "the move falls inside a 5 s horizon");
        // A horizon past the mark time is cut at the mark: marked at 0.5 s, the move at 1 s is
        // not adverse selection yet.
        let early = pnl_decomposition(&ev, &path, 1, S / 2, 5 * S, fees).expect("decomposition");
        assert_eq!((unit(early.adverse_selection), unit(early.inventory)), (0.0, 0.0));
        assert_eq!(long.total, short.total, "the total does not depend on the horizon");
        // Exactness against an independent ledger: cash 1,020, position −10, mark 101,
        // fees −200 fee units: 2 × (1,020 − 1,010) × 10,000 + 400.
        assert_eq!(long.total, 2 * (1_020 - 1_010) * 10_000 + 400);
        assert_eq!((long.fills, long.volume, long.maker_volume, long.position), (1, 10, 10, -10));
        assert_eq!((long.opening_position, long.carried), (0, 0));
    }

    #[test]
    fn metrics_use_the_mid_the_order_met() {
        // Owner 2's buy empties the 102 ask, so the fill itself moves the mid from 100 to 101.
        // Spreads and spread capture must use the mid before it.
        let ev = run(&[
            (1_000, lim(1, Side::Buy, 98, 10)),
            (1_000, lim(1, Side::Sell, 102, 10)),
            (1_000, lim(5, Side::Sell, 104, 10)),
            (2_000, ioc(2, Side::Buy, 102, 10)),
        ]);
        let (path, ix) = (BookPath::from_events(&ev), OrderIndex::from_events(&ev));
        assert_eq!((path.mid2_at(1_999), path.mid2_at(2_000)), (Some(200), Some(202)));
        let s = execution_spreads(&ev, &path, &ix, 0, S, &[S], None);
        assert_eq!((s.quoted, s.effective, s.midpoint), (4.0, 4.0, 100.0), "102 − 98; 2 × (102 − 100)");
        let fees = FeeRates { maker: 0, taker: 0, auction: 0, scale: 10_000 };
        let d = pnl_decomposition(&ev, &path, 2, S, S / 2, fees).expect("decomposition");
        assert_eq!((d.price_units(d.spread_capture), d.price_units(d.adverse_selection)), (-20.0, 10.0), "+1 × (100 − 102) × 10; +1 × (101 − 100) × 10");
    }

    #[test]
    fn windowed_decomposition_carries_the_opening_position() {
        let ev = quoted_market();
        let path = BookPath::from_events(&ev);
        let fees = FeeRates { maker: -20, taker: 30, auction: 0, scale: 10_000 };
        // Owner 1 enters the window (0.5 s, 20 s] short 10 (sold at 2 µs); the mid goes 100 → 101.
        let w = pnl_decomposition_window(&ev, &path, 1, S / 2, 20 * S, S, fees).expect("window");
        assert_eq!((w.fills, w.volume, w.opening_position, w.position, w.fees), (0, 0, -10, -10, 0));
        assert_eq!(w.price_units(w.carried), -10.0, "−10 × (101 − 100)");
        assert_eq!(w.price_units(w.total), -10.0);
        // Owner 3's only fill (passive buy of 5 at 100 at 10 s, mid 101) is inside (5 s, 20 s].
        let w3 = pnl_decomposition_window(&ev, &path, 3, 5 * S, 20 * S, S, fees).expect("window");
        assert_eq!((w3.price_units(w3.spread_capture), w3.carried, w3.fees, w3.maker_volume), (5.0, 0, 2 * -20 * 5, 5));
        // For every owner and start, the window total is the change in the whole-run total,
        // including a start exactly at a fill (2 µs, 10 s), which puts the fill before the window.
        for owner in 1..=4 {
            let whole = pnl_decomposition(&ev, &path, owner, 20 * S, S / 2, fees).expect("whole run");
            for from in [2_000, S / 2, S, 10 * S, 15 * S] {
                let before = pnl_decomposition(&ev, &path, owner, from, S / 2, fees).expect("marked at the start");
                let w = pnl_decomposition_window(&ev, &path, owner, from, 20 * S, S / 2, fees).expect("window");
                assert_eq!(w.total, whole.total - before.total, "owner {owner}, from {from}");
                assert_eq!(w.spread_capture + w.adverse_selection + w.inventory + w.carried - w.fees, w.total);
                assert_eq!((w.opening_position, w.position), (before.position, whole.position), "owner {owner}, from {from}");
                assert_eq!(w.volume + before.volume, whole.volume);
            }
        }
    }

    #[test]
    fn implementation_shortfall_components_add_up() {
        // Buy 100: decision mid 100, arrival 101, fills 60 at 102 and 30 at 103, 10 unfilled, end 105.
        let s = implementation_shortfall(Side::Buy, 200, 202, &[(102, 60), (103, 30)], 100, 210);
        assert_eq!((s.delay, s.execution, s.opportunity), (2 * 100, 2 * 120, 2 * 40));
        assert_eq!(s.total, 2 * 260);
        assert_eq!(s.delay + s.execution + s.opportunity, s.total);
        // The mirror-image sell has the same costs.
        let m = implementation_shortfall(Side::Sell, 200, 198, &[(98, 60), (97, 30)], 100, 190);
        assert_eq!(m, s);
    }

    #[test]
    fn roll_amihud_and_realized_volatility_by_hand() {
        // Trades at 100, 101, 100, 100, 100, 101, 100: changes [1, −1, 0, 0, 1, −1] with mean 0;
        // lag-1 products sum to −2 over 5 pairs, so γ₁ = −0.4 and the Roll spread is 2√0.4.
        let ev = run(&[
            (1, lim(1, Side::Sell, 101, 1_000)),
            (1, lim(1, Side::Buy, 100, 1_000)),
            (S, ioc(2, Side::Sell, 100, 1)),
            (2 * S, ioc(2, Side::Buy, 101, 1)),
            (3 * S, ioc(2, Side::Sell, 100, 1)),
            (4 * S, ioc(2, Side::Sell, 100, 1)),
            (5 * S, ioc(2, Side::Sell, 100, 1)),
            (6 * S, ioc(2, Side::Buy, 101, 1)),
            (7 * S, ioc(2, Side::Sell, 100, 1)),
        ]);
        let r = roll_spread(&ev, 0, 10 * S).expect("negative autocovariance");
        assert!((r - 2.0 * 0.4f64.sqrt()).abs() < 1e-12, "Roll {r}");
        // The mid never moves (100.5 throughout): realized volatility 0, Amihud 0.
        let path = BookPath::from_events(&ev);
        assert_eq!(realized_volatility(&path, S, 8 * S, S, 1), Some(0.0));
        assert_eq!(amihud(&ev, &path, 0, 8 * S, S), Some(0.0));
        // A market whose mid rises one tick a second: at k seconds a buyer lifts the 10-share ask
        // at 100 + k, then the bid moves to 100 + k and a new 10-share ask goes up at 101 + k
        // (mid 100.5 + k). Samples at 1–4 s (the book is empty at 0): three one-tick changes in
        // three seconds, σ = 1 tick/√s. Amihud windows (1, 2], (2, 3], (3, 4] s: relative changes
        // 1/101.5, 1/102.5, 1/103.5 against traded values 1,020, 1,030, 1,040.
        let mut cmds = vec![(1, lim(1, Side::Sell, 101, 10)), (1, lim(1, Side::Buy, 100, 1_000))];
        for k in 1..=4i64 {
            cmds.push((k * S, ioc(2, Side::Buy, 100 + k, 10)));
            cmds.push((k * S, lim(1, Side::Buy, 100 + k, 1_000)));
            cmds.push((k * S, lim(1, Side::Sell, 101 + k, 10)));
        }
        let ev = run(&cmds);
        let path = BookPath::from_events(&ev);
        assert_eq!(path.mid2_at(4 * S), Some(209));
        assert_eq!(realized_volatility(&path, 0, 4 * S, S, 1), Some(1.0), "one tick per √second");
        let a = amihud(&ev, &path, 0, 4 * S, S).expect("trading");
        let expect = [(1.0 / 101.5) / 1_020.0, (1.0 / 102.5) / 1_030.0, (1.0 / 103.5) / 1_040.0];
        assert!((a - expect.iter().sum::<f64>() / 3.0).abs() < 1e-15, "Amihud {a}");
    }

    #[test]
    fn ofi_regression_recovers_an_exact_linear_book() {
        assert_eq!(ofi_contribution((100, 50, 101, 40), (100, 60, 101, 40)), 10, "bid grows");
        assert_eq!(ofi_contribution((100, 50, 101, 40), (100, 50, 102, 25)), 40, "ask level lifted");
        // A book path built directly: 20 shares a side, one move per second in the middle of
        // each second. Up moves (bid and ask one tick higher, fresh 20-share levels) have
        // e = +20 + 20 = 40; down moves have e = −20 − 20 = −40. So ΔP = OFI/40 exactly:
        // β = 0.025 tick per share, R² = 1, depth 20.
        let mut path = BookPath::default();
        let mut push = |t: Timestamp, b: Price, a: Price| {
            path.times.push(t);
            path.bids.push(Some(b));
            path.asks.push(Some(a));
            path.bid_qty.push(20);
            path.ask_qty.push(20);
        };
        let (mut b, mut a) = (100, 101);
        push(0, b, a);
        for k in 1..=40i64 {
            let d = if k % 3 == 0 { -1 } else { 1 };
            (b, a) = (b + d, a + d);
            push(k * S - S / 2, b, a);
        }
        let fit = ofi_regression(&path, 0, 40 * S, S, 1).expect("fit");
        assert_eq!(fit.intervals, 40);
        assert!((fit.beta - 0.025).abs() < 1e-12 && fit.intercept.abs() < 1e-12 && fit.r2 > 1.0 - 1e-12, "{fit:?}");
        assert_eq!(fit.depth, 20.0);
    }

    #[test]
    fn queue_position_and_fill_outcomes() {
        // Two bids at 100: owner 1's 10 first (0 ahead), owner 2's 10 behind it (10 ahead). A
        // seller takes 10: owner 1 complete. Owner 2 cancels. Owner 3 joins at 99 and is still
        // resting at the end (censored).
        let ev = run(&[
            (1, lim(1, Side::Buy, 100, 10)),
            (2, lim(2, Side::Buy, 100, 10)),
            (3, lim(3, Side::Buy, 99, 10)),
            (4, ioc(4, Side::Sell, 100, 10)),
            (5, Command::Cancel { owner: 2, order_id: 2 }),
        ]);
        let ix = OrderIndex::from_events(&ev);
        let b = fill_probability_by_queue(&ev, &ix, 0, 10, None, &[0, 1, 50], false);
        assert_eq!((b[0].orders, b[0].complete, b[0].any_fill), (1, 1, 1), "front of the queue: filled");
        assert_eq!((b[1].orders, b[1].any_fill), (1, 0), "behind 10 shares: cancelled unfilled");
        assert_eq!(b[2].orders, 0);
        let only2 = fill_probability_by_queue(&ev, &ix, 0, 10, Some(2), &[0, 1], false);
        assert_eq!((only2[0].orders, only2[1].orders), (0, 1));
        // Touch only: a bid at 99 under a best bid of 100 would be left out; here owner 3's 99 is
        // censored anyway, so add finished ones: owner 5 bids 98 (below the 100 touch), owner 6
        // bids 99 (between the 98 and the 100), and both cancel.
        let ev = run(&[
            (1, lim(1, Side::Buy, 100, 10)),
            (2, lim(5, Side::Buy, 98, 10)),
            (3, lim(2, Side::Buy, 100, 10)),
            (4, lim(6, Side::Buy, 99, 10)),
            (5, Command::Cancel { owner: 5, order_id: 2 }),
            (6, Command::Cancel { owner: 2, order_id: 3 }),
            (7, Command::Cancel { owner: 1, order_id: 1 }),
            (8, Command::Cancel { owner: 6, order_id: 4 }),
        ]);
        let ix = OrderIndex::from_events(&ev);
        let all = fill_probability_by_queue(&ev, &ix, 0, 10, None, &[0, 1], false);
        let touch = fill_probability_by_queue(&ev, &ix, 0, 10, None, &[0, 1], true);
        assert_eq!((all[0].orders, all[1].orders), (3, 1), "the 98 and 99 bids had nothing ahead at their own levels");
        assert_eq!((touch[0].orders, touch[1].orders), (1, 1), "neither the 98 nor the 99 bid was at the touch");
    }

    #[test]
    fn a_sniping_race_is_found_with_its_winner_losers_and_duration() {
        // Owner 1 rests an ask at 101. At 1 ms owner 2's IOC at 101 takes it; 3 µs later owner 3's
        // IOC at 101 finds nothing; 5 µs after the take the gateway refuses owner 1's cancel.
        let ev = run(&[
            (1, lim(1, Side::Sell, 101, 10)),
            (1_000_000, ioc(2, Side::Buy, 101, 10)),
            (1_003_000, ioc(3, Side::Buy, 101, 10)),
        ]);
        let ix = OrderIndex::from_events(&ev);
        let msgs = race_messages(&ev, &ix, &[(1_005_000, 1, 0)], 0, 10_000_000);
        assert_eq!(msgs.len(), 3);
        let r = latency_races(&msgs, 500_000);
        assert_eq!((r.races, r.won_by_take, r.won_by_cancel), (1, 1, 0));
        assert_eq!(r.wins.get(&2), Some(&1));
        assert_eq!((r.losses.get(&3), r.losses.get(&1)), (Some(&1), Some(&1)));
        assert_eq!(r.mean_duration_ns, 3_000.0, "first success at 1 ms, first failure 3 µs later");
        // With a 2 µs window the late messages fall outside: no race.
        assert_eq!(latency_races(&msgs, 2_000).races, 0);
        // A lone failed take (one participant) is not a race.
        let lone = vec![RaceMessage { ts: 0, owner: 5, target: (Side::Sell, 1), success: false, take: true }];
        assert_eq!(latency_races(&lone, 500_000).races, 0);
        // Nor is one participant's own success and failure.
        let alone = vec![
            RaceMessage { ts: 0, owner: 5, target: (Side::Sell, 1), success: true, take: true },
            RaceMessage { ts: 10, owner: 5, target: (Side::Sell, 1), success: false, take: true },
        ];
        assert_eq!(latency_races(&alone, 500_000).races, 0);
    }
}
