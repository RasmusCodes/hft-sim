//! Single-price call auction: uncross price determination.
//!
//! Rule (`docs/MATCHING_RULES.md`, R7). Let D be every tick-grid price between the lowest
//! and highest limit price in the book (or just the reference price when only market
//! orders rest). For p in D, B(p) is the buy quantity willing to trade at p (market buys
//! plus limit buys priced >= p) and S(p) likewise for sells (limit <= p). Then:
//!
//! 1. maximise executable volume V(p) = min(B(p), S(p)); no trade if the maximum is 0;
//! 2. among those, minimise the absolute surplus |B(p) - S(p)|;
//! 3. if every remaining price has a buy surplus, take the highest; if every one has a
//!    sell surplus, the lowest (market pressure);
//! 4. otherwise take the price closest to the reference price, the higher one on a tie.
//!
//! This module evaluates the rule in O(L) for L price levels. B and S only change at
//! limit prices, so it scores each limit price plus one interval per gap between
//! consecutive limit prices instead of every tick; the reference model scores every tick
//! by brute force and the differential tests check that the two always agree.

use crate::types::{Price, Qty};

/// Outcome of auction price determination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AuctionQuote {
    /// Uncross price.
    pub price: Price,
    /// Volume executable at `price`.
    pub volume: Qty,
    /// Buy minus sell quantity willing to trade at `price` (positive: buy surplus).
    pub imbalance: Qty,
}

/// A price interval on which B and S are constant.
struct Segment {
    lo: Price,
    hi: Price,
    buy: Qty,
    sell: Qty,
}

/// `bids` and `asks` are aggregated limit levels `(price, qty)` in ascending price order.
pub(crate) fn determine(
    bids: &[(Price, Qty)],
    asks: &[(Price, Qty)],
    market_buy: Qty,
    market_sell: Qty,
    tick: Price,
    reference: Price,
) -> Option<AuctionQuote> {
    // Distinct limit prices of both sides, ascending.
    let mut prices: Vec<Price> = Vec::with_capacity(bids.len() + asks.len());
    let (mut i, mut j) = (0, 0);
    while i < bids.len() || j < asks.len() {
        let p = match (bids.get(i), asks.get(j)) {
            (Some(b), Some(a)) if b.0 <= a.0 => {
                i += 1;
                if b.0 == a.0 {
                    j += 1;
                }
                b.0
            }
            (Some(_), Some(a)) => {
                j += 1;
                a.0
            }
            (Some(b), None) => {
                i += 1;
                b.0
            }
            (None, Some(a)) => {
                j += 1;
                a.0
            }
            (None, None) => unreachable!(),
        };
        prices.push(p);
    }

    if prices.is_empty() {
        let volume = market_buy.min(market_sell);
        return (volume > 0).then_some(AuctionQuote {
            price: reference,
            volume,
            imbalance: market_buy - market_sell,
        });
    }

    let m = prices.len();
    // buy_at[k] = B(prices[k]): market buys + bids priced >= prices[k].
    let mut buy_at = vec![0; m];
    let mut acc = market_buy;
    let mut b = bids.len();
    for k in (0..m).rev() {
        while b > 0 && bids[b - 1].0 >= prices[k] {
            acc += bids[b - 1].1;
            b -= 1;
        }
        buy_at[k] = acc;
    }
    // sell_at[k] = S(prices[k]): market sells + asks priced <= prices[k].
    let mut sell_at = vec![0; m];
    let mut acc = market_sell;
    let mut a = 0;
    for k in 0..m {
        while a < asks.len() && asks[a].0 <= prices[k] {
            acc += asks[a].1;
            a += 1;
        }
        sell_at[k] = acc;
    }

    let mut segments = Vec::with_capacity(2 * m);
    for k in 0..m {
        segments.push(Segment { lo: prices[k], hi: prices[k], buy: buy_at[k], sell: sell_at[k] });
        // Strictly between two consecutive limit prices no order changes eligibility:
        // B equals B at the upper price and S equals S at the lower price.
        if k + 1 < m && prices[k + 1] - prices[k] >= 2 * tick {
            segments.push(Segment {
                lo: prices[k] + tick,
                hi: prices[k + 1] - tick,
                buy: buy_at[k + 1],
                sell: sell_at[k],
            });
        }
    }
    select(&segments, reference)
}

fn select(segments: &[Segment], reference: Price) -> Option<AuctionQuote> {
    let volume = |s: &Segment| s.buy.min(s.sell);
    let surplus = |s: &Segment| (s.buy - s.sell).abs();

    let vmax = segments.iter().map(volume).max()?;
    if vmax <= 0 {
        return None;
    }
    let smin = segments.iter().filter(|s| volume(s) == vmax).map(surplus).min()?;
    let best: Vec<&Segment> = segments
        .iter()
        .filter(|s| volume(s) == vmax && surplus(s) == smin)
        .collect();

    let (price, seg) = if best.iter().all(|s| s.buy > s.sell) {
        let s = best.iter().max_by_key(|s| s.hi)?;
        (s.hi, *s)
    } else if best.iter().all(|s| s.buy < s.sell) {
        let s = best.iter().min_by_key(|s| s.lo)?;
        (s.lo, *s)
    } else {
        let mut chosen: Option<(Price, &Segment)> = None;
        for s in &best {
            let c = reference.clamp(s.lo, s.hi);
            let better = match chosen {
                None => true,
                Some((p, _)) => {
                    let (d, dp) = ((c - reference).abs(), (p - reference).abs());
                    d < dp || (d == dp && c > p)
                }
            };
            if better {
                chosen = Some((c, *s));
            }
        }
        chosen?
    };
    Some(AuctionQuote { price, volume: vmax, imbalance: seg.buy - seg.sell })
}
