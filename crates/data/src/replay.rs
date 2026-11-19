//! Pure replay (ARCHITECTURE §10, mode a): what a hypothetical resting order would have
//! experienced in a historical message stream. The historical market cannot react to the order,
//! so a queue model decides when it would have filled. The rules are FIFO and conservative
//! (RISKS K3):
//! * the order joins the back of its price level after every message stamped at or before its
//!   entry time. Everything resting there then is ahead of it, including the opening book's
//!   unknown orders (seeds), which entered before the file starts. An order with an expiry
//!   leaves before every message stamped at or after it, so neither the stamp it entered in nor
//!   the one it was cancelled in is seen;
//! * cancellations and deletions of orders ahead move it up; those of orders behind do nothing;
//! * an execution at its price of an order ahead (or of an unknown, pre-existing order) only
//!   shortens the queue ahead. An execution of an order that joined *behind* it is volume that
//!   would have reached it first, so that volume fills it, up to what history shows. The model
//!   never invents demand;
//! * an execution on its side at a worse price (a trade-through) means its level was exhausted:
//!   that volume would have reached it first, so it fills it, up to what is left. (Filling the
//!   whole rest instead would invent volume: the trade-through may have been small.);
//! * optionally ([`replay_passive_with`]), the taker-remainder rule: a taker that ended its
//!   arrival with volume unfilled at a limit reaching the order's price had exhausted every
//!   resting order it could reach, so that volume would have reached the order too;
//! * an execution away from the executed order's own price is one at a price it was ranked at
//!   but did not display (Nasdaq's Price to Comply orders, ITCH `C` messages). Those shares leave
//!   the displayed queue, but nothing traded at the displayed price, so like a hidden execution
//!   it fills nothing;
//! * hidden executions and crosses never fill it, and it never changes the historical book.
//!
//! An order that would cross the book on entry is marketable, not passive; it gets no fills here
//! (that needs an impact model) and is flagged.

use std::collections::HashMap;

use lob::Side;

use crate::lobster::{Kind, Message, Rebuilder, Snapshot};

/// A hypothetical limit order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PassiveOrder {
    /// Entry time (nanoseconds after midnight): it joins after every message stamped at or
    /// before it.
    pub entry_ns: i64,
    /// Side.
    pub side: Side,
    /// Limit price (dollars × 10,000).
    pub price: i64,
    /// Shares.
    pub size: i64,
    /// Cancelled at this time if not filled by then. It sees no message stamped at or after this
    /// time, as it sees none stamped at or before its entry: a stamp holds several messages, and
    /// the cancellation may have come first.
    pub expire_ns: Option<i64>,
    /// A historical order this one stands in for (leave-one-out): that order's own messages are
    /// invisible to it, as if it had never been in the stream. `None` for a purely hypothetical
    /// order.
    pub own_order_id: Option<i64>,
}

/// Why a fill happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillReason {
    /// Execution volume that reached the order once the queue ahead was gone.
    Queue,
    /// An execution on the order's side at a worse price: its level was exhausted.
    TradeThrough,
    /// Volume a taker left unfilled at a limit that reached the order.
    TakerRemainder,
}

impl FillReason {
    /// Name used in reports.
    pub fn name(self) -> &'static str {
        match self {
            FillReason::Queue => "queue",
            FillReason::TradeThrough => "trade-through",
            FillReason::TakerRemainder => "taker remainder",
        }
    }
}

/// A taker that ended its arrival with volume it could not fill at its limit, after exhausting
/// every resting order it could reach: a resting order at a price within that limit would have
/// received that volume. Whoever builds these must set `available` to 0 when the premise fails,
/// for example for a fill-or-kill order killed while liquidity rested, or when an order within
/// the limit was still resting once the taker was done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TakerRemainder {
    /// Arrival time.
    pub ts_ns: i64,
    /// The taker's order id.
    pub id: i64,
    /// The taker's side.
    pub side: Side,
    /// Its limit price.
    pub limit: i64,
    /// Units it left unfilled.
    pub available: i64,
}

/// One modelled fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PassiveFill {
    /// Time of the historical execution that caused it.
    pub ts_ns: i64,
    /// Shares.
    pub size: i64,
    /// Why.
    pub reason: FillReason,
}

/// What happened to one hypothetical order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PassiveOutcome {
    /// Shares ahead in the queue at entry.
    pub queue_ahead: i64,
    /// Fills, in time order.
    pub fills: Vec<PassiveFill>,
    /// Shares filled.
    pub filled: i64,
    /// When it finished (filled or expired); `None` if the stream ended first.
    pub done_ns: Option<i64>,
    /// It would have crossed the book on entry, so it was not modelled.
    pub marketable: bool,
    /// Times history executed an order behind it while the model still counted volume ahead:
    /// the model's queue ahead was overstated (for example by a seed that was too large).
    pub queue_overstated: usize,
}

struct Active {
    k: usize,
    ahead: HashMap<i64, i64>,
    seed_ahead: i64,
    left: i64,
}

/// Replay `orders` through `messages`. With `snapshots` (one per message, the vendor's book after
/// it), the rebuilt book is reconciled to them as in [`crate::lobster::validate`], so the queue
/// ahead includes the opening book's unknown orders; without them it holds only orders submitted
/// in the stream, which understates queues early in the day.
pub fn replay_passive(messages: &[Message], snapshots: Option<&[Snapshot]>, levels: usize, orders: &[PassiveOrder]) -> Vec<PassiveOutcome> {
    replay_passive_with(messages, snapshots, levels, orders, &[])
}

/// Fill resting orders from one taker's unfilled remainder (see [`TakerRemainder`]): each order
/// on the other side at a price within its limit, entered before its stamp and not cancelled by
/// then, gets up to `available`. Each order is its own hypothetical world, so each can get it.
fn take_remainder(tk: &TakerRemainder, orders: &[PassiveOrder], active: &mut Vec<Active>, out: &mut [PassiveOutcome]) {
    if tk.available <= 0 {
        return;
    }
    for a in active.iter_mut() {
        let o = orders[a.k];
        let reaches = match tk.side {
            Side::Buy => o.side == Side::Sell && o.price <= tk.limit,
            Side::Sell => o.side == Side::Buy && o.price >= tk.limit,
        };
        if !reaches || o.entry_ns >= tk.ts_ns || o.expire_ns.is_some_and(|x| x <= tk.ts_ns) || Some(tk.id) == o.own_order_id {
            continue;
        }
        let size = tk.available.min(a.left);
        a.left -= size;
        out[a.k].filled += size;
        out[a.k].fills.push(PassiveFill { ts_ns: tk.ts_ns, size, reason: FillReason::TakerRemainder });
        if a.left == 0 {
            out[a.k].done_ns = Some(tk.ts_ns);
        }
    }
    active.retain(|a| a.left > 0);
}

/// [`replay_passive`] with the taker-remainder rule: each of `takers` (in time order) is applied
/// once its stamp is complete, before the first message stamped later.
pub fn replay_passive_with(messages: &[Message], snapshots: Option<&[Snapshot]>, levels: usize, orders: &[PassiveOrder], takers: &[TakerRemainder]) -> Vec<PassiveOutcome> {
    let mut out = vec![PassiveOutcome::default(); orders.len()];
    let mut pending: Vec<usize> = (0..orders.len()).collect();
    pending.sort_by_key(|&k| (orders[k].entry_ns, k));
    let mut pending = pending.into_iter().peekable();
    let mut active: Vec<Active> = Vec::new();
    let mut book = Rebuilder::new();
    let mut next_taker = 0;
    for (j, m) in messages.iter().enumerate() {
        // Orders whose entry time has passed join now, behind everything resting.
        while let Some(&k) = pending.peek() {
            if orders[k].entry_ns >= m.ts_ns {
                break;
            }
            pending.next();
            let o = orders[k];
            let (bid, ask) = book.best();
            let crosses = match o.side {
                Side::Buy => ask.is_some_and(|a| o.price >= a),
                Side::Sell => bid.is_some_and(|b| o.price <= b),
            };
            if crosses {
                out[k].marketable = true;
                out[k].done_ns = Some(o.entry_ns);
                continue;
            }
            let ahead: HashMap<i64, i64> = book.orders_at(o.side, o.price).into_iter().filter(|&(id, _)| Some(id) != o.own_order_id).collect();
            let seed_ahead = book.seed_at(o.side, o.price);
            out[k].queue_ahead = ahead.values().sum::<i64>() + seed_ahead;
            active.push(Active { k, ahead, seed_ahead, left: o.size });
        }
        // Takers whose stamp is complete.
        while next_taker < takers.len() && takers[next_taker].ts_ns < m.ts_ns {
            take_remainder(&takers[next_taker], orders, &mut active, &mut out);
            next_taker += 1;
        }
        // Expiries before this message.
        active.retain(|a| match orders[a.k].expire_ns {
            Some(x) if x <= m.ts_ns => {
                out[a.k].done_ns = Some(x);
                false
            }
            _ => true,
        });
        // An execution away from the executed order's own price (see the module documentation).
        let off_price = m.kind == Kind::ExecuteVisible && book.price_of(m.order_id).is_some_and(|p| p != m.price);
        for a in &mut active {
            let o = orders[a.k];
            if m.side != o.side || Some(m.order_id) == o.own_order_id {
                continue;
            }
            let fill = match m.kind {
                Kind::ExecuteVisible if off_price => {
                    if let Some(q) = a.ahead.get_mut(&m.order_id) {
                        *q -= m.size.min(*q);
                        if *q == 0 {
                            a.ahead.remove(&m.order_id);
                        }
                    }
                    None
                }
                Kind::Cancel | Kind::Delete if m.price == o.price => {
                    if let Some(q) = a.ahead.get_mut(&m.order_id) {
                        *q -= if m.kind == Kind::Delete { *q } else { m.size.min(*q) };
                        if *q == 0 {
                            a.ahead.remove(&m.order_id);
                        }
                    } else if !book.is_known(m.order_id) {
                        a.seed_ahead -= m.size.min(a.seed_ahead);
                    }
                    None
                }
                Kind::ExecuteVisible if m.price == o.price => {
                    if let Some(q) = a.ahead.get_mut(&m.order_id) {
                        *q -= m.size.min(*q);
                        if *q == 0 {
                            a.ahead.remove(&m.order_id);
                        }
                        None
                    } else if !book.is_known(m.order_id) {
                        // A pre-existing order: it entered before ours, so it was ahead.
                        a.seed_ahead -= m.size.min(a.seed_ahead);
                        None
                    } else {
                        // An order that joined behind ours traded: that volume reached ours first.
                        if !a.ahead.is_empty() || a.seed_ahead > 0 {
                            out[a.k].queue_overstated += 1;
                            a.ahead.clear();
                            a.seed_ahead = 0;
                        }
                        Some((m.size.min(a.left), FillReason::Queue))
                    }
                }
                Kind::ExecuteVisible if (o.side == Side::Buy && m.price < o.price) || (o.side == Side::Sell && m.price > o.price) => Some((m.size.min(a.left), FillReason::TradeThrough)),
                _ => None,
            };
            if let Some((size, reason)) = fill {
                a.left -= size;
                out[a.k].filled += size;
                out[a.k].fills.push(PassiveFill { ts_ns: m.ts_ns, size, reason });
                if a.left == 0 {
                    out[a.k].done_ns = Some(m.ts_ns);
                }
            }
        }
        active.retain(|a| a.left > 0);
        book.apply(m);
        if let Some(s) = snapshots.and_then(|s| s.get(j)) {
            if book.top(levels) != *s {
                book.reconcile(s, levels);
            }
        }
    }
    for tk in &takers[next_taker..] {
        take_remainder(tk, orders, &mut active, &mut out);
    }
    out
}

/// What a real order did, read from the stream itself.
#[derive(Clone, Debug)]
struct Real {
    side: Side,
    price: i64,
    size: i64,
    entry_ns: i64,
    fills: Vec<(i64, i64)>,
    left: i64,
    end_ns: Option<i64>,
    cancelled: bool,
}

/// The replay model checked against real orders, leave-one-out. Every order submitted in a stream
/// is replayed as a copy (same entry time, side, price and size) through the same stream without
/// that order's own messages, and the copy's modelled fills are compared with the order's real
/// executions. A copy expires when its original was cancelled. One whose original filled
/// completely, or still rested at the end, waits to the end of the stream.
///
/// The model sees only what history shows without the order. When an aggressor's last shares went
/// to the original, nothing in that stream reveals them, so the copy fills later or never: the
/// blind spot of [`replay_passive`] (docs/HISTORICAL_DATA.md §4). A copy filled *earlier* than its
/// original, or one never filled in reality, means history broke the model's FIFO assumption.
///
/// That stream is history *minus* the order, not history *without* it: had the order never been
/// there, the volume it took would have gone to the next orders in line, or stayed with the
/// taker. [`LooOptions::counterfactual`] builds that stream for the orders that filled.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeaveOneOut {
    /// The taker-remainder rule was applied.
    pub taker_rule: bool,
    /// Filled orders were compared in a counterfactual stream.
    pub counterfactual: bool,
    /// What re-matching moved (counterfactual only).
    pub rematched: Rematched,
    /// Model volume by reason: queue, trade-through, taker remainder.
    pub volume_by_reason: [i64; 3],
    /// Orders compared: submitted at or after `from_ns`, resting past their entry stamp, and not
    /// marketable on entry.
    pub orders: usize,
    /// Left out: submitted before `from_ns` (an opening book).
    pub before_from: usize,
    /// Left out: gone (filled or cancelled) within the stamp they entered in. A copy joins after
    /// every message of that stamp, so it could never rest beside its original.
    pub gone_within_entry_stamp: usize,
    /// Left out: copies that would have crossed the book on entry (not modelled).
    pub marketable: usize,
    /// Filled, at least partly, by both the exchange and the model.
    pub both: usize,
    /// Filled by the exchange only: the model missed it (conservative).
    pub exchange_only: usize,
    /// Filled by the model only: never filled in reality (optimistic).
    pub model_only: usize,
    /// Filled by neither.
    pub neither: usize,
    /// Of `both`: the model's first fill came before the real first fill (optimistic)...
    pub earlier: usize,
    /// ...at the same stamp...
    pub same_stamp: usize,
    /// ...or after it (conservative).
    pub later: usize,
    /// For `later`: the model's first fill − the real first fill, sorted.
    pub lags_ns: Vec<i64>,
    /// Copies the model filled for more than their original traded before it was cancelled
    /// (optimistic in size).
    pub overfilled: usize,
    /// Real volume traded by the compared orders.
    pub real_volume: i64,
    /// Volume the model filled for their copies.
    pub model_volume: i64,
    /// Times history executed an order behind a copy while the model still counted volume ahead.
    pub queue_overstated: usize,
    /// Up to 10 optimistic cases (model only, earlier, overfilled) as JSON objects.
    pub examples: Vec<String>,
    /// Up to 10 conservative misses (exchange only, later), in the same form.
    pub missed: Vec<String>,
}

/// Taker information an exporter can give alongside a message stream (LOBSTER messages carry
/// only the resting side of each trade).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TakerInfo {
    /// Takers that left volume unfilled, in time order: the taker-remainder rule's input.
    pub remainders: Vec<TakerRemainder>,
    /// Every taker by id: arrival time, side and limit price.
    pub arrivals: HashMap<i64, (i64, Side, i64)>,
    /// For each execution message (type 4) of the stream, in order, the taker that caused it
    /// (0 if unknown).
    pub fill_takers: Vec<i64>,
}

/// How to run [`leave_one_out_with`].
#[derive(Clone, Copy, Debug, Default)]
pub struct LooOptions<'a> {
    /// Taker information (needed for both options below).
    pub takers: Option<&'a TakerInfo>,
    /// Apply the taker-remainder rule.
    pub taker_rule: bool,
    /// Compare each filled order in a counterfactual stream: history without it. The volume it
    /// took is re-matched in price-time priority to the orders behind it, then to worse prices
    /// within the taker's limit, and what is left stays with the taker as unfilled remainder.
    /// Never-filled orders need none: removing an order nothing traded with changes nothing else.
    /// Assumes no one would have acted differently, the same assumption replay itself makes.
    /// Where the taker is unknown (feeds without taker orders, such as ITCH), only the orders
    /// behind it at its price are reachable for certain, so the rest stays unattributed.
    pub counterfactual: bool,
    /// If not 0, compare only an evenly spaced sample of this many orders that really filled
    /// (by submission order), so that variants can be scored on the same orders cheaply.
    pub sample_filled: usize,
}

/// Volume the counterfactual moved, over all filled orders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rematched {
    /// To orders behind the removed order at its price.
    pub behind: i64,
    /// To worse prices within the taker's limit.
    pub deeper: i64,
    /// Left with the taker, unfilled.
    pub leftover: i64,
    /// Left over where the taker is unknown: re-matched within its price level only, the rest
    /// cannot be placed.
    pub unattributed: i64,
}

/// `messages` without `target`, the volume it took re-matched (see [`LooOptions::counterfactual`]),
/// and the unfilled remainders that leaves with the takers.
fn counterfactual_stream(messages: &[Message], target: i64, info: &TakerInfo, re: &mut Rematched) -> (Vec<Message>, Vec<TakerRemainder>) {
    struct Live {
        side: Side,
        price: i64,
        left: i64,
        seq: usize,
    }
    let mut live: HashMap<i64, Live> = HashMap::new();
    let mut out: Vec<Message> = Vec::with_capacity(messages.len() + 16);
    let mut extra: Vec<TakerRemainder> = Vec::new();
    let (mut executions, mut target_seq) = (0usize, usize::MAX);
    for (i, m) in messages.iter().enumerate() {
        let taker = if m.kind == Kind::ExecuteVisible {
            executions += 1;
            info.fill_takers.get(executions - 1).copied().unwrap_or(0)
        } else {
            0
        };
        if m.order_id == target {
            if m.kind == Kind::Submit {
                target_seq = i;
            } else if m.kind == Kind::ExecuteVisible {
                let mut left = m.size;
                let arrival = info.arrivals.get(&taker).copied();
                // The orders the taker would have reached next, in price-time priority: behind
                // the target at its price, then (if its limit is known) worse prices within it.
                let mut next: Vec<(i64, usize, i64)> = live
                    .iter()
                    .filter(|(_, l)| {
                        l.side == m.side
                            && ((l.price == m.price && l.seq > target_seq)
                                || arrival.is_some_and(|(_, _, limit)| match m.side {
                                    Side::Buy => l.price < m.price && l.price >= limit,
                                    Side::Sell => l.price > m.price && l.price <= limit,
                                }))
                    })
                    .map(|(&id, l)| (if m.side == Side::Buy { -l.price } else { l.price }, l.seq, id))
                    .collect();
                next.sort_unstable();
                for (_, _, id) in next {
                    if left == 0 {
                        break;
                    }
                    let l = live.get_mut(&id).expect("listed above");
                    let take = left.min(l.left);
                    out.push(Message { ts_ns: m.ts_ns, kind: Kind::ExecuteVisible, order_id: id, size: take, price: l.price, side: l.side });
                    if l.price == m.price {
                        re.behind += take;
                    } else {
                        re.deeper += take;
                    }
                    l.left -= take;
                    left -= take;
                    if l.left == 0 {
                        live.remove(&id);
                    }
                }
                match (left > 0, arrival) {
                    (true, Some((ts, side, limit))) => {
                        re.leftover += left;
                        extra.push(TakerRemainder { ts_ns: ts, id: taker, side, limit, available: left });
                    }
                    (true, None) => re.unattributed += left,
                    _ => {}
                }
            }
            continue;
        }
        out.push(*m);
        match m.kind {
            Kind::Submit => {
                live.insert(m.order_id, Live { side: m.side, price: m.price, left: m.size, seq: i });
            }
            Kind::Cancel | Kind::ExecuteVisible => {
                if let Some(l) = live.get_mut(&m.order_id) {
                    l.left -= m.size.min(l.left);
                    if l.left == 0 {
                        live.remove(&m.order_id);
                    }
                }
            }
            Kind::Delete => {
                live.remove(&m.order_id);
            }
            _ => {}
        }
    }
    (out, extra)
}

/// Parse an exporter's taker files: `time,id,side,limit,size,subtype,filled,remainder,fate,available`
/// per taker, and `time,maker_id,taker_id,size,price` per execution message of the stream, in
/// stream order (header lines are skipped).
pub fn parse_taker_info(takers_csv: &str, fills_csv: &str) -> Result<TakerInfo, String> {
    let mut info = TakerInfo::default();
    let num = |f: &[&str], k: usize, line: usize| -> Result<i64, String> { f.get(k).and_then(|s| s.trim().parse().ok()).ok_or_else(|| format!("line {line}: field {} is not an integer", k + 1)) };
    for (k, row) in takers_csv.lines().enumerate() {
        if row.trim().is_empty() || row.starts_with("time") {
            continue;
        }
        let f: Vec<&str> = row.split(',').collect();
        let ts = crate::lobster::parse_time_ns(f[0]).ok_or_else(|| format!("takers line {}: bad time", k + 1))?;
        let side = if num(&f, 2, k + 1)? > 0 { Side::Buy } else { Side::Sell };
        let (id, limit, available) = (num(&f, 1, k + 1)?, num(&f, 3, k + 1)?, num(&f, 9, k + 1)?);
        info.arrivals.insert(id, (ts, side, limit));
        if available > 0 {
            info.remainders.push(TakerRemainder { ts_ns: ts, id, side, limit, available });
        }
    }
    info.remainders.sort_by_key(|t| (t.ts_ns, t.id));
    for (k, row) in fills_csv.lines().enumerate() {
        if row.trim().is_empty() || row.starts_with("time") {
            continue;
        }
        let f: Vec<&str> = row.split(',').collect();
        info.fill_takers.push(num(&f, 2, k + 1)?);
    }
    Ok(info)
}

/// Run the leave-one-out check on `messages` for the orders submitted at or after `from_ns`.
pub fn leave_one_out(messages: &[Message], from_ns: i64) -> LeaveOneOut {
    leave_one_out_with(messages, from_ns, &LooOptions::default())
}

/// [`leave_one_out`] with the taker-remainder rule and/or the counterfactual stream.
pub fn leave_one_out_with(messages: &[Message], from_ns: i64, opt: &LooOptions) -> LeaveOneOut {
    let mut real: HashMap<i64, Real> = HashMap::new();
    let mut submitted: Vec<i64> = Vec::new();
    for m in messages {
        match m.kind {
            Kind::Submit => {
                let r = Real { side: m.side, price: m.price, size: m.size, entry_ns: m.ts_ns, fills: Vec::new(), left: m.size, end_ns: None, cancelled: false };
                if real.insert(m.order_id, r).is_none() {
                    submitted.push(m.order_id);
                }
            }
            Kind::ExecuteVisible => {
                if let Some(r) = real.get_mut(&m.order_id) {
                    r.fills.push((m.ts_ns, m.size));
                    r.left -= m.size;
                    if r.left <= 0 {
                        r.end_ns = Some(m.ts_ns);
                    }
                }
            }
            Kind::Cancel | Kind::Delete => {
                if let Some(r) = real.get_mut(&m.order_id) {
                    r.left = if m.kind == Kind::Delete { 0 } else { r.left - m.size };
                    if r.left <= 0 {
                        r.end_ns = Some(m.ts_ns);
                        r.cancelled = true;
                    }
                }
            }
            _ => {}
        }
    }
    let mut v = LeaveOneOut { taker_rule: opt.taker_rule && opt.takers.is_some(), counterfactual: opt.counterfactual && opt.takers.is_some(), ..LeaveOneOut::default() };
    let mut cases: Vec<(i64, PassiveOrder)> = Vec::new();
    for &id in &submitted {
        let r = &real[&id];
        if r.entry_ns < from_ns {
            v.before_from += 1;
        } else if r.end_ns.is_some_and(|e| e <= r.entry_ns) {
            v.gone_within_entry_stamp += 1;
        } else {
            let expire_ns = if r.cancelled { r.end_ns } else { None };
            cases.push((id, PassiveOrder { entry_ns: r.entry_ns, side: r.side, price: r.price, size: r.size, expire_ns, own_order_id: Some(id) }));
        }
    }
    if opt.sample_filled > 0 {
        let filled: Vec<(i64, PassiveOrder)> = cases.into_iter().filter(|(id, _)| !real[id].fills.is_empty()).collect();
        let step = filled.len().div_ceil(opt.sample_filled).max(1);
        cases = filled.into_iter().step_by(step).collect();
    }
    let copies: Vec<PassiveOrder> = cases.iter().map(|c| c.1).collect();
    let base: &[TakerRemainder] = match opt.takers {
        Some(info) if v.taker_rule => &info.remainders,
        _ => &[],
    };
    let mut outcomes = replay_passive_with(messages, None, 1, &copies, base);
    if let (true, Some(info)) = (v.counterfactual, opt.takers) {
        for (n, (id, o)) in cases.iter().enumerate() {
            if real[id].fills.is_empty() {
                continue;
            }
            let (stream, extra) = counterfactual_stream(messages, *id, info, &mut v.rematched);
            let mut takers: Vec<TakerRemainder> = if v.taker_rule { base.iter().copied().chain(extra).collect() } else { Vec::new() };
            takers.sort_by_key(|t| (t.ts_ns, t.id));
            outcomes[n] = replay_passive_with(&stream, None, 1, &[*o], &takers).remove(0);
        }
    }
    for ((id, o), out) in cases.iter().zip(&outcomes) {
        if out.marketable {
            v.marketable += 1;
            continue;
        }
        v.orders += 1;
        let r = &real[id];
        let real_filled: i64 = r.fills.iter().map(|f| f.1).sum();
        let mut why: Vec<&str> = Vec::new();
        let mut miss: Option<&str> = None;
        match (r.fills.first().map(|f| f.0), out.fills.first().map(|f| f.ts_ns)) {
            (Some(t_real), Some(t_model)) => {
                v.both += 1;
                if t_model < t_real {
                    v.earlier += 1;
                    why.push("earlier");
                } else if t_model == t_real {
                    v.same_stamp += 1;
                } else {
                    v.later += 1;
                    v.lags_ns.push(t_model - t_real);
                    miss = Some("later");
                }
            }
            (Some(_), None) => {
                v.exchange_only += 1;
                miss = Some("exchange only");
            }
            (None, Some(_)) => {
                v.model_only += 1;
                why.push("model only");
            }
            (None, None) => v.neither += 1,
        }
        if o.expire_ns.is_some() && out.filled > real_filled {
            v.overfilled += 1;
            why.push("overfilled");
        }
        v.real_volume += real_filled;
        v.model_volume += out.filled;
        v.queue_overstated += out.queue_overstated;
        for f in &out.fills {
            v.volume_by_reason[f.reason as usize] += f.size;
        }
        let describe = |why: &str| {
            let real_fills = r.fills.iter().map(|(t, s)| format!("[{t},{s}]")).collect::<Vec<_>>().join(",");
            let model_fills = out.fills.iter().map(|f| format!("[{},{},\"{}\"]", f.ts_ns, f.size, f.reason.name())).collect::<Vec<_>>().join(",");
            format!(
                "{{\"why\":\"{why}\",\"id\":{id},\"side\":\"{}\",\"price\":{},\"size\":{},\"entry_ns\":{},\"expire_ns\":{},\"queue_ahead\":{},\"real\":[{real_fills}],\"model\":[{model_fills}]}}",
                if o.side == Side::Buy { "buy" } else { "sell" },
                o.price,
                o.size,
                o.entry_ns,
                o.expire_ns.map_or("null".into(), |x| x.to_string()),
                out.queue_ahead
            )
        };
        if !why.is_empty() && v.examples.len() < 10 {
            v.examples.push(describe(&why.join(", ")));
        }
        if let (Some(m), true) = (miss, v.missed.len() < 10) {
            v.missed.push(describe(m));
        }
    }
    v.lags_ns.sort_unstable();
    v
}

impl LeaveOneOut {
    /// Add another check's counts to this one (to total several stocks).
    pub fn absorb(&mut self, o: &LeaveOneOut) {
        self.taker_rule |= o.taker_rule;
        self.counterfactual |= o.counterfactual;
        let (r, s) = (&mut self.rematched, o.rematched);
        (r.behind, r.deeper, r.leftover, r.unattributed) = (r.behind + s.behind, r.deeper + s.deeper, r.leftover + s.leftover, r.unattributed + s.unattributed);
        for k in 0..3 {
            self.volume_by_reason[k] += o.volume_by_reason[k];
        }
        self.orders += o.orders;
        self.before_from += o.before_from;
        self.gone_within_entry_stamp += o.gone_within_entry_stamp;
        self.marketable += o.marketable;
        self.both += o.both;
        self.exchange_only += o.exchange_only;
        self.model_only += o.model_only;
        self.neither += o.neither;
        self.earlier += o.earlier;
        self.same_stamp += o.same_stamp;
        self.later += o.later;
        self.lags_ns.extend_from_slice(&o.lags_ns);
        self.lags_ns.sort_unstable();
        self.overfilled += o.overfilled;
        self.real_volume += o.real_volume;
        self.model_volume += o.model_volume;
        self.queue_overstated += o.queue_overstated;
        for (mine, theirs) in [(&mut self.examples, &o.examples), (&mut self.missed, &o.missed)] {
            mine.extend(theirs.iter().take(10usize.saturating_sub(mine.len())).cloned());
        }
    }

    /// As a JSON object.
    pub fn to_json(&self) -> String {
        let lags = if self.lags_ns.is_empty() {
            "null".to_string()
        } else {
            let q = |f: f64| self.lags_ns[((self.lags_ns.len() - 1) as f64 * f).round() as usize];
            format!("{{\"min\":{},\"p10\":{},\"median\":{},\"p90\":{},\"max\":{}}}", q(0.0), q(0.1), q(0.5), q(0.9), q(1.0))
        };
        let rematched = if self.counterfactual {
            let r = self.rematched;
            format!("{{\"behind\":{},\"deeper\":{},\"leftover\":{},\"unattributed\":{}}}", r.behind, r.deeper, r.leftover, r.unattributed)
        } else {
            "null".into()
        };
        let [queue, through, remainder] = self.volume_by_reason;
        format!(
            "{{\"taker_rule\":{},\"counterfactual\":{},\"orders\":{},\"left_out\":{{\"before_from\":{},\"gone_within_entry_stamp\":{},\"marketable\":{}}},\
             \"filled_by\":{{\"both\":{},\"exchange_only\":{},\"model_only\":{},\"neither\":{}}},\
             \"first_fill\":{{\"model_earlier\":{},\"same_stamp\":{},\"model_later\":{},\"lag_ns\":{}}},\
             \"overfilled\":{},\"real_volume\":{},\"model_volume\":{},\
             \"model_volume_by_reason\":{{\"queue\":{queue},\"trade_through\":{through},\"taker_remainder\":{remainder}}},\
             \"rematched\":{rematched},\"queue_overstated\":{},\"examples\":[{}],\"missed\":[{}]}}",
            self.taker_rule,
            self.counterfactual,
            self.orders,
            self.before_from,
            self.gone_within_entry_stamp,
            self.marketable,
            self.both,
            self.exchange_only,
            self.model_only,
            self.neither,
            self.earlier,
            self.same_stamp,
            self.later,
            lags,
            self.overfilled,
            self.real_volume,
            self.model_volume,
            self.queue_overstated,
            self.examples.join(","),
            self.missed.join(",")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(t: i64, kind: Kind, id: i64, size: i64, price: i64, side: Side) -> Message {
        Message { ts_ns: t, kind, order_id: id, size, price, side }
    }

    fn bid(entry: i64, price: i64, size: i64) -> PassiveOrder {
        PassiveOrder { entry_ns: entry, side: Side::Buy, price, size, expire_ns: None, own_order_id: None }
    }

    #[test]
    fn fills_come_only_after_the_queue_ahead_is_gone() {
        // Bids A (100) and B (50) rest at 99.00; our 30 joins at t = 3. A and B trade away; D
        // (40) joins behind us; history then executes D for 20 and 20. In a FIFO queue those
        // executions would have reached us first: we fill 20, then our last 10.
        let p = 990_000;
        let ms = [
            msg(1, Kind::Submit, 1, 100, p, Side::Buy),
            msg(2, Kind::Submit, 2, 50, p, Side::Buy),
            msg(4, Kind::ExecuteVisible, 1, 100, p, Side::Buy),
            msg(5, Kind::ExecuteVisible, 2, 50, p, Side::Buy),
            msg(6, Kind::Submit, 4, 40, p, Side::Buy),
            msg(7, Kind::ExecuteVisible, 4, 20, p, Side::Buy),
            msg(8, Kind::ExecuteVisible, 4, 20, p, Side::Buy),
        ];
        let r = &replay_passive(&ms, None, 1, &[bid(3, p, 30)])[0];
        assert_eq!(r.queue_ahead, 150);
        assert_eq!(r.fills, vec![PassiveFill { ts_ns: 7, size: 20, reason: FillReason::Queue }, PassiveFill { ts_ns: 8, size: 10, reason: FillReason::Queue }]);
        assert_eq!((r.filled, r.done_ns, r.queue_overstated, r.marketable), (30, Some(8), 0, false));
    }

    #[test]
    fn an_execution_away_from_the_orders_own_price_fills_nothing() {
        // Bids rest at 99.00: order 1 (100) ahead of our 20, order 2 (100) behind it; a second
        // copy of ours bids 10 at 99.01. Order 2 trades at 99.01, a price it was ranked at without
        // displaying it (Nasdaq's Price to Comply; an ITCH "C"): nothing traded at 99.00, and
        // that is no evidence about the queue at 99.01 either, so neither copy fills. Order 1
        // trading at 99.01 takes its shares out of the queue ahead. When order 2 then trades at
        // 99.00, the first copy is at the front and fills, and 99.01 was traded through.
        let p = 990_000;
        let ms = [
            msg(1, Kind::Submit, 1, 100, p, Side::Buy),
            msg(3, Kind::Submit, 2, 100, p, Side::Buy),
            msg(4, Kind::ExecuteVisible, 2, 50, p + 100, Side::Buy),
            msg(5, Kind::ExecuteVisible, 1, 100, p + 100, Side::Buy),
            msg(6, Kind::ExecuteVisible, 2, 30, p, Side::Buy),
        ];
        let rs = replay_passive(&ms, None, 1, &[bid(2, p, 20), bid(2, p + 100, 10)]);
        assert_eq!(rs[0].queue_ahead, 100);
        assert_eq!(rs[0].fills, vec![PassiveFill { ts_ns: 6, size: 20, reason: FillReason::Queue }]);
        assert_eq!(rs[0].queue_overstated, 0, "order 1's shares left the queue ahead when it traded at 99.01");
        assert_eq!(rs[1].fills, vec![PassiveFill { ts_ns: 6, size: 10, reason: FillReason::TradeThrough }], "the 99.01 bid fills only when 99.00 trades");
    }

    #[test]
    fn cancellations_ahead_help_and_behind_do_not() {
        let p = 990_000;
        let ms = [
            msg(1, Kind::Submit, 1, 100, p, Side::Buy),
            msg(3, Kind::Submit, 3, 40, p, Side::Buy),
            msg(4, Kind::Cancel, 3, 40, p, Side::Buy),
            msg(5, Kind::Cancel, 1, 60, p, Side::Buy),
            msg(6, Kind::ExecuteVisible, 1, 40, p, Side::Buy),
            msg(7, Kind::Submit, 5, 10, p, Side::Buy),
            msg(8, Kind::ExecuteVisible, 5, 10, p, Side::Buy),
        ];
        let r = &replay_passive(&ms, None, 1, &[bid(2, p, 10)])[0];
        assert_eq!(r.queue_ahead, 100);
        assert_eq!((r.filled, r.done_ns), (10, Some(8)), "the cancel behind us did not move us; the one ahead did");
        assert_eq!(r.queue_overstated, 0, "the queue ahead had been worked off exactly when an order behind us traded");
    }

    #[test]
    fn a_trade_through_fills_what_traded_through_and_expiry_stops_the_wait() {
        // A seller takes the whole 99.00 bid (order 1, ahead of us) and then 10 at 98.99. With our
        // 50 at 99.00 it would have taken ours before reaching 98.99, but it only had 10 left
        // by then: we get those 10, not all 50.
        let p = 990_000;
        let ms = [
            msg(1, Kind::Submit, 1, 100, p, Side::Buy),
            msg(2, Kind::Submit, 2, 100, p - 100, Side::Buy),
            msg(5, Kind::ExecuteVisible, 1, 100, p, Side::Buy),
            msg(6, Kind::ExecuteVisible, 2, 10, p - 100, Side::Buy),
        ];
        let r = replay_passive(&ms, None, 1, &[bid(1, p, 50), PassiveOrder { expire_ns: Some(4), ..bid(1, p, 50) }]);
        assert_eq!(r[0].fills, vec![PassiveFill { ts_ns: 6, size: 10, reason: FillReason::TradeThrough }]);
        assert_eq!((r[0].filled, r[0].done_ns), (10, None));
        assert_eq!((r[1].filled, r[1].done_ns), (0, Some(4)));
    }

    #[test]
    fn a_copy_sees_nothing_in_the_stamp_it_was_cancelled_in() {
        // Our 20 bids at 99.00 with nothing ahead and is cancelled at t = 5. Order 2 joins behind
        // it and trades at t = 5. Several messages share a stamp (ITCH stamps tie), and the
        // cancellation may have come first, so that execution fills nothing: the same rule as for
        // the stamp an order enters in.
        let p = 990_000;
        let ms = [msg(3, Kind::Submit, 2, 100, p, Side::Buy), msg(5, Kind::ExecuteVisible, 2, 50, p, Side::Buy)];
        let r = &replay_passive(&ms, None, 1, &[PassiveOrder { expire_ns: Some(5), ..bid(2, p, 20) }])[0];
        assert_eq!((r.filled, r.done_ns), (0, Some(5)));
        assert_eq!(replay_passive(&ms, None, 1, &[PassiveOrder { expire_ns: Some(6), ..bid(2, p, 20) }])[0].filled, 20, "cancelled a stamp later, it fills");
    }

    #[test]
    fn a_takers_unfilled_remainder_reaches_orders_within_its_limit() {
        // A seller (taker 50, limit 98.00) takes the 100 bid at 99.00 and is left with 30 it
        // could not fill: nothing else rested within its limit. A bid of ours at 99.00 that
        // entered earlier would have received those 30.
        // A later, unrelated ask at t = 6 makes the order entered at t = 5 join before the taker's
        // remainder is handed out, so its entry time is what keeps it out.
        let p = 990_000;
        let ms = [msg(1, Kind::Submit, 1, 100, p, Side::Buy), msg(5, Kind::ExecuteVisible, 1, 100, p, Side::Buy), msg(6, Kind::Submit, 9, 1, p + 50_000, Side::Sell)];
        let taker = TakerRemainder { ts_ns: 5, id: 50, side: Side::Sell, limit: p - 10_000, available: 30 };
        let orders = [
            bid(2, p, 50),
            bid(2, p - 20_000, 50),                         // below the taker's limit
            bid(5, p, 50),                                  // entered in the taker's own stamp
            PassiveOrder { expire_ns: Some(5), ..bid(2, p, 50) }, // cancelled within that stamp
        ];
        let r = replay_passive_with(&ms, None, 1, &orders, &[taker]);
        assert_eq!(r[0].fills, vec![PassiveFill { ts_ns: 5, size: 30, reason: FillReason::TakerRemainder }]);
        assert_eq!((r[1].filled, r[2].filled, r[3].filled), (0, 0, 0));
        assert_eq!(replay_passive(&ms, None, 1, &orders)[0].filled, 0, "without the rule the 30 are invisible");
    }

    #[test]
    fn the_counterfactual_gives_the_removed_orders_volume_to_the_next_in_line() {
        // O (1) bids 10 at 99.00; Q (2) joins behind it with 4, R (3) bids 5 at 98.99, S (5) 3 at
        // 98.98. A seller (taker 70, limit 98.99) takes O's 10. Without O it would have taken Q's
        // 4 and R's 5, not S (beyond its limit), and kept 1. History minus O shows none of that;
        // history without O shows all of it.
        let p = 990_000;
        let ms = [
            msg(1, Kind::Submit, 1, 10, p, Side::Buy),
            msg(2, Kind::Submit, 2, 4, p, Side::Buy),
            msg(2, Kind::Submit, 3, 5, p - 100, Side::Buy),
            msg(3, Kind::Submit, 5, 3, p - 200, Side::Buy),
            msg(5, Kind::ExecuteVisible, 1, 10, p, Side::Buy),
            msg(9, Kind::Delete, 2, 4, p, Side::Buy),
            msg(9, Kind::Delete, 3, 5, p - 100, Side::Buy),
            msg(9, Kind::Delete, 5, 3, p - 200, Side::Buy),
        ];
        let info = TakerInfo { remainders: vec![], arrivals: HashMap::from([(70, (5, Side::Sell, p - 100))]), fill_takers: vec![70] };
        let naive = leave_one_out(&ms, 0);
        assert_eq!((naive.orders, naive.exchange_only, naive.neither), (4, 1, 3));
        let cf = leave_one_out_with(&ms, 0, &LooOptions { takers: Some(&info), counterfactual: true, ..LooOptions::default() });
        assert_eq!(cf.rematched, Rematched { behind: 4, deeper: 5, leftover: 1, unattributed: 0 }, "S lies beyond the taker's limit");
        assert_eq!((cf.both, cf.same_stamp, cf.neither, cf.model_volume, cf.volume_by_reason), (1, 1, 3, 9, [4, 5, 0]));
        let full = leave_one_out_with(&ms, 0, &LooOptions { takers: Some(&info), taker_rule: true, counterfactual: true, sample_filled: 0 });
        assert_eq!((full.same_stamp, full.model_volume, full.volume_by_reason, full.overfilled), (1, 10, [4, 5, 1], 0), "the 1 left with the taker reaches O's copy");
        assert!(full.to_json().contains("\"rematched\":{\"behind\":4,\"deeper\":5,\"leftover\":1,\"unattributed\":0}"));
        // A feed that names no taker (ITCH): only Q, behind O at its price, is reachable for
        // certain; the other 6 cannot be placed. Sampling keeps only the order that filled.
        let blind = TakerInfo { fill_takers: vec![0], ..TakerInfo::default() };
        let within = leave_one_out_with(&ms, 0, &LooOptions { takers: Some(&blind), counterfactual: true, sample_filled: 1, ..LooOptions::default() });
        assert_eq!(within.rematched, Rematched { behind: 4, deeper: 0, leftover: 0, unattributed: 6 });
        assert_eq!((within.orders, within.same_stamp, within.model_volume), (1, 1, 4), "only the filled order is in the sample");
    }

    #[test]
    fn taker_files_parse() {
        let takers = "time,id,side,limit,size,subtype,filled,remainder,fate,available\n0.000005000,70,-1,989900,10,4,10,0,cancelled,0\n0.000007000,71,1,991000,8,4,5,3,cancelled,3\n";
        let fills = "time,maker_id,taker_id,size,price\n0.000005000,1,70,10,990000\n";
        let info = parse_taker_info(takers, fills).expect("parses");
        assert_eq!(info.arrivals[&70], (5000, Side::Sell, 989_900));
        assert_eq!(info.remainders, vec![TakerRemainder { ts_ns: 7000, id: 71, side: Side::Buy, limit: 991_000, available: 3 }]);
        assert_eq!(info.fill_takers, vec![70]);
        assert!(parse_taker_info("0.1,x,1,1,1,1,1,1,c,1\n", "").is_err());
    }

    #[test]
    fn the_unknown_opening_book_counts_as_ahead_when_snapshots_are_given() {
        // The vendor's first row shows 100 bid at 99.00 that the file never submitted (orders
        // placed before it starts). Our 10 joins behind them. History executes 60 and then 40 of
        // unknown order 7; then known order 2, which joined after us, trades 15.
        let p = 990_000;
        let ms = [
            msg(1, Kind::Submit, 1, 5, p + 1000, Side::Sell),
            msg(3, Kind::Submit, 2, 20, p, Side::Buy),
            msg(4, Kind::ExecuteVisible, 7, 60, p, Side::Buy),
            msg(5, Kind::ExecuteVisible, 7, 40, p, Side::Buy),
            msg(6, Kind::ExecuteVisible, 2, 15, p, Side::Buy),
        ];
        let snap = |bid_size: i64| Snapshot { asks: vec![(p + 1000, 5)], bids: vec![(p, bid_size)] };
        let snaps = [snap(100), snap(120), snap(60), snap(20), snap(5)];
        let with = &replay_passive(&ms, Some(&snaps), 1, &[bid(2, p, 10)])[0];
        assert_eq!((with.queue_ahead, with.filled, with.done_ns, with.queue_overstated), (100, 10, Some(6), 0));
        // Without snapshots the fills are the same, because order ids alone place unknown orders
        // ahead of ours; only the queue ahead reported at entry misses the 100 unknown shares.
        let without = &replay_passive(&ms, None, 1, &[bid(2, p, 10)])[0];
        assert_eq!((without.queue_ahead, without.filled, without.done_ns), (0, 10, Some(6)));
    }

    #[test]
    fn an_order_that_would_cross_is_flagged_not_filled() {
        let ms = [msg(1, Kind::Submit, 1, 100, 990_000, Side::Sell), msg(5, Kind::Submit, 2, 100, 989_000, Side::Buy)];
        let r = &replay_passive(&ms, None, 1, &[bid(2, 990_000, 10)])[0];
        assert_eq!((r.marketable, r.filled, r.done_ns), (true, 0, Some(2)));
    }

    #[test]
    fn a_copy_never_sees_its_originals_messages() {
        // Order 5 rests at 99.00, 6 joins behind it. History fills 5 at t = 3, then 6 trades 4 at
        // t = 4. The copy of 5 stands where 5 stood: nothing is ahead of it, 5's own execution is
        // invisible to it (that volume reached the original), and 6's trade reaches it first.
        let p = 990_000;
        let ms = [
            msg(1, Kind::Submit, 5, 10, p, Side::Buy),
            msg(2, Kind::Submit, 6, 10, p, Side::Buy),
            msg(3, Kind::ExecuteVisible, 5, 10, p, Side::Buy),
            msg(4, Kind::ExecuteVisible, 6, 4, p, Side::Buy),
        ];
        let copy = &replay_passive(&ms, None, 1, &[PassiveOrder { own_order_id: Some(5), ..bid(1, p, 10) }])[0];
        assert_eq!((copy.queue_ahead, copy.filled, copy.fills[0].ts_ns), (0, 4, 4));
        // A purely hypothetical order beside it queues behind order 5 instead.
        let other = &replay_passive(&ms, None, 1, &[bid(1, p, 10)])[0];
        assert_eq!((other.queue_ahead, other.filled), (10, 4));
    }

    #[test]
    fn leave_one_out_sorts_real_orders_by_what_the_model_made_of_them() {
        // A (1) and B (2) bid 99.00; C (3) bids 98.99; D (4) is an ask gone within its stamp. An
        // aggressor fills A and 3 of B at t = 5. E (7) and F (8) join at 99.00; history then fills
        // F at t = 8 while B and E still rest ahead of it, a FIFO violation in the data. C, B's
        // rest and E are cancelled.
        let p = 990_000;
        let ms = [
            msg(1, Kind::Submit, 1, 10, p, Side::Buy),
            msg(2, Kind::Submit, 2, 10, p, Side::Buy),
            msg(3, Kind::Submit, 3, 5, p - 100, Side::Buy),
            msg(4, Kind::Submit, 4, 5, p + 1000, Side::Sell),
            msg(4, Kind::Delete, 4, 5, p + 1000, Side::Sell),
            msg(5, Kind::ExecuteVisible, 1, 10, p, Side::Buy),
            msg(5, Kind::ExecuteVisible, 2, 3, p, Side::Buy),
            msg(6, Kind::Submit, 7, 10, p, Side::Buy),
            msg(7, Kind::Submit, 8, 5, p, Side::Buy),
            msg(8, Kind::ExecuteVisible, 8, 5, p, Side::Buy),
            msg(8, Kind::Delete, 3, 5, p - 100, Side::Buy),
            msg(9, Kind::Delete, 2, 7, p, Side::Buy),
            msg(10, Kind::Delete, 7, 10, p, Side::Buy),
        ];
        let v = leave_one_out(&ms, 0);
        assert_eq!((v.orders, v.gone_within_entry_stamp, v.marketable, v.before_from), (5, 1, 0, 0), "D left within its stamp");
        // A: B's 3 traded behind it at t = 5, so its copy fills then (same stamp). B: the
        // aggressor's last shares went to B itself; its copy fills only from F's trade at t = 8
        // (3 later). C: neither. E: filled by the model only, from F's out-of-order trade. F:
        // its own fill is invisible to its copy, and nothing else reaches it (exchange only).
        assert_eq!((v.both, v.exchange_only, v.model_only, v.neither), (2, 1, 1, 1));
        assert_eq!((v.earlier, v.same_stamp, v.later, v.lags_ns.clone()), (0, 1, 1, vec![3]));
        assert_eq!((v.overfilled, v.queue_overstated, v.real_volume, v.model_volume), (2, 1, 18, 18), "B's and E's copies took F's 5");
        assert!(v.examples[0].starts_with("{\"why\":\"overfilled\",\"id\":2,") && v.examples[1].starts_with("{\"why\":\"model only, overfilled\",\"id\":7,"));
        assert_eq!(leave_one_out(&ms, 2).before_from, 1, "A entered before t = 2");
    }
}
