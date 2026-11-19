//! Order-level (L3) feeds that also publish order-level snapshots, such as Bitstamp's public
//! `live_orders` and `detail_order_book` channels (docs/HISTORICAL_DATA.md §6). The book is rebuilt
//! from the order events and compared, order by order, with each vendor snapshot: the same order
//! ids, prices, amounts and priority order.
//!
//! A recording starts with a book already full of orders, and the event stream says nothing about
//! such an order until it changes or leaves. Before each comparison the snapshot may therefore
//! **reveal** orders the stream has not mentioned: a listed one is added (or corrected), and one
//! inside the listed range that the vendor no longer shows is retired. A snapshot never changes an
//! order the stream has mentioned (created, changed or deleted), so one late or inconsistent
//! snapshot cannot corrupt the rebuild. Each snapshot is then:
//! * **exact**: the rebuilt book equals it, with nothing revealed, corrected or retired;
//! * **explained**: it equals it once orders the stream never mentioned were revealed, corrected or
//!   retired;
//! * **matched with offset**: it equals the rebuilt book at another event boundary within ± the
//!   tolerance, a timing offset between the vendor's two channels;
//! * **unexplained**: none of these. Its differences, all about orders the stream mentioned, are
//!   counted by kind, with examples.
//!
//! Plain-text inputs (written by `python -m hftsim.bitstamp convert`):
//! * events: `ts_us,kind,order_id,side,price,amount` with kind `created|changed|deleted`, side
//!   `buy|sell`, integer price (e.g. cents) and amount (e.g. satoshis), in exchange order;
//! * snapshots: `ts_us,snapshot,side,rank,order_id,price,amount`, one row per listed order in the
//!   vendor's priority order.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use lob::Side;

/// What happened to an order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum L3Kind {
    /// It entered the book.
    Created,
    /// Its price or remaining amount changed (a partial fill or an amendment).
    Changed,
    /// It left the book.
    Deleted,
}

/// One order event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct L3Event {
    /// Exchange time, microseconds.
    pub ts_us: i64,
    /// What happened.
    pub kind: L3Kind,
    /// Order id.
    pub id: u64,
    /// Side.
    pub side: Side,
    /// Price after the event (integer units).
    pub price: i64,
    /// Remaining amount after the event (integer units).
    pub amount: i64,
}

/// One vendor snapshot: the listed orders per side in priority order, as (price, amount, id).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct L3Snapshot {
    /// Exchange time, microseconds.
    pub ts_us: i64,
    /// Bids, best first.
    pub bids: Vec<(i64, i64, u64)>,
    /// Asks, best first.
    pub asks: Vec<(i64, i64, u64)>,
}

/// Result of a check.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct L3Validation {
    /// Order events applied.
    pub events: usize,
    /// Snapshots compared.
    pub snapshots: usize,
    /// Snapshots equal to the rebuilt book (ids, prices, amounts, priority order) with nothing
    /// revealed, corrected or retired.
    pub exact: usize,
    /// Snapshots equal to the rebuilt book once orders the stream never mentioned were revealed,
    /// corrected or retired from them.
    pub explained: usize,
    /// The event stamps' unit (microseconds): an event stamped `t` happened in `[t, t + unit)`.
    pub event_stamp_us: i64,
    /// Of the exact and explained snapshots, those that matched the book part-way through the
    /// events that cannot be ordered against the snapshot (stamped within one unit before it, or
    /// at its time), not after all of them.
    pub exact_within_stamp: usize,
    /// Snapshots that did not match at their own time but matched the book at another event
    /// boundary within ± the tolerance: a timing offset between the vendor's two channels, not a
    /// reconstruction error.
    pub matched_with_offset: usize,
    /// Offsets of those: when the matching book state began − snapshot time (microseconds), sorted.
    pub offsets_us: Vec<i64>,
    /// When the matching state ended (the stamp of the first later event after which the book no
    /// longer matched) − snapshot time, sorted; +tolerance if it had not ended by the window's
    /// end. That event happened before `ended + event_stamp_us`, so a snapshot with `ended +
    /// event_stamp_us <= 0` shows a book at least that much older than its stamp.
    pub offset_ends_us: Vec<i64>,
    /// Snapshots that matched in none of these ways.
    pub unexplained: usize,
    /// First snapshot (1-based) that is unexplained.
    pub first_unexplained: Option<usize>,
    /// Orders first seen in a snapshot (placed before the recording started), counted once each.
    pub revealed: usize,
    /// Of those, orders whose id is above the id of the stream's first created order. If the
    /// vendor's ids rise with time, these were placed during the recording and the stream missed
    /// their creation, so this should be 0.
    pub revealed_above_first_created_id: usize,
    /// Times a later snapshot changed a revealed order's price or amount with no event about it.
    pub corrected: usize,
    /// Times a later snapshot stopped showing a revealed order inside its listed range with no
    /// event about it, so it was removed.
    pub retired: usize,
    /// Differences in unexplained snapshots, by kind: the vendor lists an order the rebuilt book
    /// lacks; the rebuilt book has an order inside the listed range the vendor does not show; same
    /// order with another amount; same order at another price; same orders and values in another
    /// priority order (counted once per side).
    pub missing: usize,
    /// See `missing`.
    pub extra: usize,
    /// See `missing`.
    pub amount_differs: usize,
    /// See `missing`.
    pub price_differs: usize,
    /// See `missing`.
    pub order_differs: usize,
    /// Changes and deletions of orders the rebuilt book had never held (placed before the
    /// recording and never revealed, e.g. deeper than the snapshots list).
    pub unknown_order_events: usize,
    /// Vendor snapshots whose best bid is at or above their best ask.
    pub crossed_snapshots: usize,
    /// Times the rebuilt book became crossed or locked (best bid at or above best ask) through an
    /// event or a reveal, from the first snapshot on (before it the book lacks the orders placed
    /// before the recording): e.g. an incoming order published as resting before its trades.
    pub crossed_book_episodes: usize,
    /// Total time the rebuilt book stayed crossed, microseconds (by event stamps).
    pub crossed_book_us: i64,
    /// Longest single episode, microseconds.
    pub crossed_book_longest_us: i64,
    /// Events whose time is earlier than the event before them (the file is not in time order).
    pub out_of_order_events: usize,
    /// Up to 10 unexplained snapshots, each as a JSON object listing up to 5 orders per kind of
    /// difference with their creation, change and deletion times relative to the snapshot (from
    /// the whole stream), to tell timing effects from real disagreements.
    pub examples: Vec<String>,
}

fn key(side: Side, price: i64, id: u64) -> (i64, u64) {
    // Priority order: best price first, then earlier (lower) id.
    match side {
        Side::Buy => (-price, id),
        Side::Sell => (price, id),
    }
}

/// The rebuilt order-level book.
#[derive(Clone, Debug, Default)]
pub struct L3Book {
    orders: HashMap<u64, (Side, i64, i64)>,
    queue: [BTreeMap<(i64, u64), ()>; 2],
}

fn idx(side: Side) -> usize {
    match side {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

impl L3Book {
    /// Set an order's state (insert, move or resize).
    pub fn set(&mut self, id: u64, side: Side, price: i64, amount: i64) {
        self.remove(id);
        if amount > 0 {
            self.orders.insert(id, (side, price, amount));
            self.queue[idx(side)].insert(key(side, price, id), ());
        }
    }

    /// Remove an order if present; returns whether it was.
    pub fn remove(&mut self, id: u64) -> bool {
        match self.orders.remove(&id) {
            Some((side, price, _)) => {
                self.queue[idx(side)].remove(&key(side, price, id));
                true
            }
            None => false,
        }
    }

    /// Whether the order is live.
    pub fn contains(&self, id: u64) -> bool {
        self.orders.contains_key(&id)
    }

    /// The best `n` orders of one side, in priority order, as (price, amount, id).
    pub fn top(&self, side: Side, n: usize) -> Vec<(i64, i64, u64)> {
        self.queue[idx(side)].keys().take(n).map(|&(_, id)| {
            let (_, p, a) = self.orders[&id];
            (p, a, id)
        }).collect()
    }

    /// Live orders.
    pub fn len(&self) -> usize {
        self.orders.len()
    }

    /// Whether the book is empty.
    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    /// Whether the best bid is at or above the best ask.
    pub fn crossed(&self) -> bool {
        match (self.queue[0].keys().next(), self.queue[1].keys().next()) {
            (Some(&(neg_bid, _)), Some(&(ask, _))) => -neg_bid >= ask,
            _ => false,
        }
    }

    fn apply(&mut self, e: &L3Event) {
        match e.kind {
            L3Kind::Created | L3Kind::Changed => self.set(e.id, e.side, e.price, e.amount),
            L3Kind::Deleted => {
                self.remove(e.id);
            }
        }
    }
}

/// Whether one side's best `depth` orders equal the vendor's list (same as comparing
/// `book.top(side, depth)` with it, without building the list).
fn side_matches(book: &L3Book, side: Side, listed: &[(i64, i64, u64)], depth: usize) -> bool {
    let mut ours = book.queue[idx(side)].keys().take(depth);
    for &(p, a, id) in listed {
        match ours.next() {
            Some(&(_, oid)) if oid == id => {
                let (_, op, oa) = book.orders[&oid];
                if (op, oa) != (p, a) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    ours.next().is_none()
}

fn book_matches(book: &L3Book, snap: &L3Snapshot, depth: usize) -> bool {
    side_matches(book, Side::Buy, &snap.bids, depth) && side_matches(book, Side::Sell, &snap.asks, depth)
}

/// Whether `id` lies inside a side's listed range: at or before the vendor's last listed order
/// when the list is full, anywhere when it is not (the vendor then lists the whole side).
fn inside(side: Side, price: i64, id: u64, listed: &[(i64, i64, u64)], depth: usize) -> bool {
    listed.len() < depth || listed.last().is_some_and(|&(p, _, i)| key(side, price, id) <= key(side, p, i))
}

/// What the whole stream says about one order.
#[derive(Clone, Debug, Default)]
struct Life {
    /// Index of the first event about it.
    first: usize,
    /// Time of its creation, if the stream created it.
    created: Option<i64>,
    /// Time of its (last) deletion.
    deleted: Option<i64>,
    /// Times of its first five changes.
    changed: Vec<i64>,
}

struct Lives {
    by_id: HashMap<u64, Life>,
    first_created_id: Option<u64>,
}

impl Lives {
    fn new(events: &[L3Event]) -> Lives {
        let mut by_id: HashMap<u64, Life> = HashMap::with_capacity(events.len() / 2);
        for (j, e) in events.iter().enumerate() {
            let l = by_id.entry(e.id).or_insert_with(|| Life { first: j, ..Life::default() });
            match e.kind {
                L3Kind::Created => {
                    l.created.get_or_insert(e.ts_us);
                }
                L3Kind::Deleted => l.deleted = Some(e.ts_us),
                L3Kind::Changed => {
                    if l.changed.len() < 5 {
                        l.changed.push(e.ts_us);
                    }
                }
            }
        }
        let first_created_id = events.iter().find(|e| e.kind == L3Kind::Created).map(|e| e.id);
        Lives { by_id, first_created_id }
    }

    /// Whether only a snapshot can tell us about `id` once the first `j` events are applied: the
    /// stream never creates it and has not mentioned it yet.
    fn untouched(&self, id: u64, j: usize) -> bool {
        self.by_id.get(&id).is_none_or(|l| l.created.is_none() && l.first >= j)
    }

    /// An order as JSON, with its stream times relative to `ts` and optionally (ours, theirs).
    fn json(&self, id: u64, ts: i64, values: Option<(i64, i64)>) -> String {
        let l = self.by_id.get(&id);
        let rel = |t: Option<i64>| t.map_or("null".to_string(), |t| (t - ts).to_string());
        let values = values.map_or(String::new(), |(ours, theirs)| format!(",\"ours\":{ours},\"theirs\":{theirs}"));
        let changed = l.map_or(String::new(), |l| l.changed.iter().map(|t| (t - ts).to_string()).collect::<Vec<_>>().join(","));
        format!(
            "{{\"id\":{id}{values},\"created_dt_us\":{},\"changed_dt_us\":[{changed}],\"deleted_dt_us\":{}}}",
            rel(l.and_then(|l| l.created)),
            rel(l.and_then(|l| l.deleted))
        )
    }
}

/// What a snapshot changed in a book.
#[derive(Debug, Default)]
struct Revealed {
    added: Vec<u64>,
    corrected: usize,
    retired: usize,
}

impl Revealed {
    fn any(&self) -> bool {
        !self.added.is_empty() || self.corrected > 0 || self.retired > 0
    }
}

/// Bring the orders only `snap` can tell us about into line with it. A listed order untouched by
/// the first `add_before` events (the book's state) is added or corrected. An order untouched
/// through `retire_before` (the end of the events the snapshot is compared against) that lies
/// inside the listed range but is not listed is retired: if the stream deletes it within that
/// span, its own event does that instead.
fn reveal(book: &mut L3Book, snap: &L3Snapshot, depth: usize, lives: &Lives, add_before: usize, retire_before: usize) -> Revealed {
    let mut r = Revealed::default();
    for (side, listed) in [(Side::Buy, &snap.bids), (Side::Sell, &snap.asks)] {
        for &(p, a, id) in listed.iter() {
            if !lives.untouched(id, add_before) {
                continue;
            }
            match book.orders.get(&id) {
                Some(&o) if o == (side, p, a) => {}
                Some(_) => {
                    book.set(id, side, p, a);
                    r.corrected += 1;
                }
                None => {
                    book.set(id, side, p, a);
                    r.added.push(id);
                }
            }
        }
        let shown: HashSet<u64> = listed.iter().map(|o| o.2).collect();
        let stale: Vec<u64> = book.queue[idx(side)]
            .keys()
            .map(|&(_, id)| id)
            .take_while(|&id| inside(side, book.orders[&id].1, id, listed, depth))
            .filter(|&id| !shown.contains(&id) && lives.untouched(id, retire_before))
            .collect();
        for id in stale {
            book.remove(id);
            r.retired += 1;
        }
    }
    r
}

/// The rebuilt book and what is tracked alongside it.
struct Rebuild {
    book: L3Book,
    /// Ids the book has ever held.
    held: HashSet<u64>,
    last_ts: i64,
    /// Whether crossings are tracked yet (from the first snapshot on), and since when the book is
    /// crossed.
    track_crossing: bool,
    crossed_since: Option<i64>,
}

impl Rebuild {
    fn apply(&mut self, e: &L3Event, v: &mut L3Validation) {
        v.out_of_order_events += usize::from(e.ts_us < self.last_ts);
        self.last_ts = self.last_ts.max(e.ts_us);
        if e.kind != L3Kind::Created {
            v.unknown_order_events += usize::from(!self.held.contains(&e.id));
        }
        self.book.apply(e);
        self.held.insert(e.id);
        v.events += 1;
        self.note_crossing(e.ts_us, v);
    }

    fn note_crossing(&mut self, ts: i64, v: &mut L3Validation) {
        if !self.track_crossing {
            return;
        }
        match (self.book.crossed(), self.crossed_since) {
            (true, None) => {
                self.crossed_since = Some(ts);
                v.crossed_book_episodes += 1;
            }
            (false, Some(since)) => {
                v.crossed_book_us += ts - since;
                v.crossed_book_longest_us = v.crossed_book_longest_us.max(ts - since);
                self.crossed_since = None;
            }
            _ => {}
        }
    }
}

/// Differences between the rebuilt book and one snapshot.
#[derive(Debug, Default)]
struct Differences {
    missing: Vec<u64>,
    extra: Vec<u64>,
    /// (id, ours, theirs).
    amount: Vec<(u64, i64, i64)>,
    price: Vec<(u64, i64, i64)>,
    order: usize,
}

fn differences(book: &L3Book, snap: &L3Snapshot, depth: usize) -> Differences {
    let mut d = Differences::default();
    for (side, listed) in [(Side::Buy, &snap.bids), (Side::Sell, &snap.asks)] {
        if side_matches(book, side, listed, depth) {
            continue;
        }
        let before = d.missing.len() + d.extra.len() + d.amount.len() + d.price.len();
        for &(p, a, id) in listed.iter() {
            match book.orders.get(&id) {
                None => d.missing.push(id),
                Some(&(_, op, _)) if op != p => d.price.push((id, op, p)),
                Some(&(_, _, oa)) if oa != a => d.amount.push((id, oa, a)),
                Some(_) => {}
            }
        }
        let shown: HashSet<u64> = listed.iter().map(|o| o.2).collect();
        for &(k, id) in book.queue[idx(side)].keys().take(depth) {
            let price = book.orders[&id].1;
            debug_assert_eq!(key(side, price, id), (k, id));
            if inside(side, price, id, listed, depth) && !shown.contains(&id) {
                d.extra.push(id);
            }
        }
        // Nothing missing, extra or different: the same orders in another priority order.
        d.order += usize::from(d.missing.len() + d.extra.len() + d.amount.len() + d.price.len() == before);
    }
    d
}

/// Rebuild the book from `events` and compare it with each of `snapshots` (both in time order);
/// see the module documentation for the classes. Event stamps are truncated to multiples of
/// `event_stamp_us` (1 for true microseconds; Bitstamp's order events: 1000), so an event stamped
/// `t` happened in `[t, t + event_stamp_us)`. Events that certainly precede a snapshot (stamped at
/// or before its time − `event_stamp_us`) are applied first; the snapshot matches at its own time
/// if the book equals it before, part-way through or after the events that cannot be ordered
/// against it (stamped later than that, up to its time). Otherwise the book states within
/// ±`tolerance_us` (0 disables the search; it starts no earlier than the events already applied
/// for the previous snapshot) are searched. `depth` is the vendor's list length per side (100
/// for Bitstamp).
pub fn validate_l3(events: &[L3Event], snapshots: &[L3Snapshot], depth: usize, tolerance_us: i64, event_stamp_us: i64) -> L3Validation {
    let lives = Lives::new(events);
    let tol = tolerance_us.max(0);
    let stamp = event_stamp_us.max(1);
    let mut v = L3Validation { event_stamp_us: stamp, ..L3Validation::default() };
    let mut rb = Rebuild { book: L3Book::default(), held: HashSet::new(), last_ts: i64::MIN, track_crossing: false, crossed_since: None };
    // Ids revealed by snapshots.
    let mut revealed: HashSet<u64> = HashSet::new();
    let mut k = 0usize;
    for (n, snap) in snapshots.iter().enumerate() {
        let (from, to) = (snap.ts_us.saturating_sub(tol), snap.ts_us.saturating_add(tol));
        // Events stamped at or before `certain` happened before the snapshot.
        let certain = snap.ts_us.saturating_sub(stamp);
        while k < events.len() && events[k].ts_us < from.min(certain.saturating_add(1)) {
            rb.apply(&events[k], &mut v);
            k += 1;
        }
        // The book at the start of the tolerance window, kept for the offset search.
        let window = (tol > 0).then(|| (rb.book.clone(), k));
        while k < events.len() && events[k].ts_us <= certain {
            rb.apply(&events[k], &mut v);
            k += 1;
        }
        // The snapshot is compared against the events up to `horizon` (its own time, or the end
        // of the tolerance window).
        let mut horizon = k;
        while horizon < events.len() && events[horizon].ts_us <= to {
            horizon += 1;
        }
        let r = reveal(&mut rb.book, snap, depth, &lives, k, horizon);
        for &id in &r.added {
            rb.held.insert(id);
            if revealed.insert(id) {
                v.revealed_above_first_created_id += usize::from(lives.first_created_id.is_some_and(|f| id > f));
            }
        }
        v.corrected += r.corrected;
        v.retired += r.retired;
        rb.track_crossing = true;
        rb.note_crossing(snap.ts_us, &mut v);
        let mut matched = book_matches(&rb.book, snap, depth);
        let mut midway = false;
        while k < events.len() && events[k].ts_us <= snap.ts_us {
            rb.apply(&events[k], &mut v);
            k += 1;
            let now = book_matches(&rb.book, snap, depth);
            midway |= matched && !now;
            matched |= now;
        }
        v.snapshots += 1;
        if let (Some(b), Some(a)) = (snap.bids.first(), snap.asks.first()) {
            v.crossed_snapshots += usize::from(b.0 >= a.0);
        }
        if matched {
            // The book agreed with the vendor at some point among the events that cannot be
            // ordered against it. If it no longer does after all of them, some are past it.
            if r.any() {
                v.explained += 1;
            } else {
                v.exact += 1;
            }
            v.exact_within_stamp += usize::from(midway || !book_matches(&rb.book, snap, depth));
            continue;
        }
        if let Some((mut w, j0)) = window {
            // Search a copy: the state at the window's start (revealed the same way), then after
            // each event up to the horizon. A state that matches began at its last event.
            reveal(&mut w, snap, depth, &lives, j0, horizon);
            let began = j0.checked_sub(1).map_or(from, |j| events[j].ts_us.max(from));
            let mut found = book_matches(&w, snap, depth).then_some(began);
            let mut j = j0;
            while found.is_none() && j < horizon {
                w.apply(&events[j]);
                if book_matches(&w, snap, depth) {
                    found = Some(events[j].ts_us);
                }
                j += 1;
            }
            if let Some(t) = found {
                // The matching state lasted until the first later event that broke the match.
                let mut ended = to;
                while j < horizon {
                    w.apply(&events[j]);
                    if !book_matches(&w, snap, depth) {
                        ended = events[j].ts_us;
                        break;
                    }
                    j += 1;
                }
                v.matched_with_offset += 1;
                v.offsets_us.push(t - snap.ts_us);
                v.offset_ends_us.push(ended - snap.ts_us);
                continue;
            }
        }
        let d = differences(&rb.book, snap, depth);
        v.unexplained += 1;
        v.first_unexplained.get_or_insert(n + 1);
        v.missing += d.missing.len();
        v.extra += d.extra.len();
        v.amount_differs += d.amount.len();
        v.price_differs += d.price.len();
        v.order_differs += d.order;
        if v.examples.len() < 10 {
            let ids = |ids: &[u64]| ids.iter().take(5).map(|&id| lives.json(id, snap.ts_us, None)).collect::<Vec<_>>().join(",");
            let values = |vals: &[(u64, i64, i64)]| vals.iter().take(5).map(|&(id, o, t)| lives.json(id, snap.ts_us, Some((o, t)))).collect::<Vec<_>>().join(",");
            v.examples.push(format!(
                "{{\"snapshot\":{},\"ts_us\":{},\"missing\":[{}],\"extra\":[{}],\"amount\":[{}],\"price\":[{}],\"priority_order_sides\":{}}}",
                n + 1,
                snap.ts_us,
                ids(&d.missing),
                ids(&d.extra),
                values(&d.amount),
                values(&d.price),
                d.order
            ));
        }
    }
    // Events after the last snapshot still count (unknown orders, time order, crossings).
    while k < events.len() {
        rb.apply(&events[k], &mut v);
        k += 1;
    }
    if let Some(since) = rb.crossed_since {
        v.crossed_book_us += rb.last_ts - since;
        v.crossed_book_longest_us = v.crossed_book_longest_us.max(rb.last_ts - since);
    }
    v.revealed = revealed.len();
    v.offsets_us.sort_unstable();
    v.offset_ends_us.sort_unstable();
    v
}

/// A malformed row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct L3ParseError {
    /// 1-based line.
    pub line: usize,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for L3ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

fn field<T: std::str::FromStr>(f: &[&str], k: usize, line: usize, name: &str) -> Result<T, L3ParseError> {
    f.get(k).and_then(|s| s.trim().parse().ok()).ok_or_else(|| L3ParseError { line, message: format!("bad or missing {name}") })
}

fn side_of(s: &str, line: usize) -> Result<Side, L3ParseError> {
    match s.trim() {
        "buy" => Ok(Side::Buy),
        "sell" => Ok(Side::Sell),
        other => Err(L3ParseError { line, message: format!("side `{other}`") }),
    }
}

/// Parse an events file (a header line is skipped).
pub fn parse_events(text: &str) -> Result<Vec<L3Event>, L3ParseError> {
    let mut out = Vec::new();
    for (k, row) in text.lines().enumerate() {
        let line = k + 1;
        if row.trim().is_empty() || row.starts_with("ts_us") {
            continue;
        }
        let f: Vec<&str> = row.split(',').collect();
        let kind = match f.get(1).map(|s| s.trim()) {
            Some("created") => L3Kind::Created,
            Some("changed") => L3Kind::Changed,
            Some("deleted") => L3Kind::Deleted,
            other => return Err(L3ParseError { line, message: format!("kind {other:?}") }),
        };
        out.push(L3Event {
            ts_us: field(&f, 0, line, "time")?,
            kind,
            id: field(&f, 2, line, "order id")?,
            side: side_of(f.get(3).copied().unwrap_or(""), line)?,
            price: field(&f, 4, line, "price")?,
            amount: field(&f, 5, line, "amount")?,
        });
    }
    Ok(out)
}

/// Parse a snapshots file (a header line is skipped); rows of one snapshot share its number.
pub fn parse_snapshots(text: &str) -> Result<Vec<L3Snapshot>, L3ParseError> {
    let mut out: Vec<L3Snapshot> = Vec::new();
    let mut current: Option<u64> = None;
    for (k, row) in text.lines().enumerate() {
        let line = k + 1;
        if row.trim().is_empty() || row.starts_with("ts_us") {
            continue;
        }
        let f: Vec<&str> = row.split(',').collect();
        let (ts, n): (i64, u64) = (field(&f, 0, line, "time")?, field(&f, 1, line, "snapshot number")?);
        if current != Some(n) {
            out.push(L3Snapshot { ts_us: ts, ..L3Snapshot::default() });
            current = Some(n);
        }
        let entry = (field(&f, 5, line, "price")?, field(&f, 6, line, "amount")?, field(&f, 4, line, "order id")?);
        let s = out.last_mut().expect("pushed");
        match side_of(f.get(2).copied().unwrap_or(""), line)? {
            Side::Buy => s.bids.push(entry),
            Side::Sell => s.asks.push(entry),
        }
    }
    Ok(out)
}

impl L3Validation {
    /// As a JSON object.
    pub fn to_json(&self) -> String {
        let quantiles = |xs: &[i64]| -> String {
            if xs.is_empty() {
                return "null".into();
            }
            let q = |f: f64| xs[((xs.len() - 1) as f64 * f).round() as usize];
            format!("{{\"min\":{},\"p10\":{},\"median\":{},\"p90\":{},\"max\":{}}}", q(0.0), q(0.1), q(0.5), q(0.9), q(1.0))
        };
        format!(
            "{{\"events\":{},\"event_stamp_us\":{},\"snapshots\":{},\"exact\":{},\"explained\":{},\"exact_within_stamp\":{},\"matched_with_offset\":{},\
             \"offset_us\":{{\"began\":{},\"ended\":{}}},\"unexplained\":{},\"first_unexplained\":{},\
             \"revealed_orders\":{},\"revealed_above_first_created_id\":{},\"corrected_orders\":{},\"retired_orders\":{},\
             \"unexplained_differences\":{{\"missing\":{},\"extra\":{},\"amount\":{},\"price\":{},\"priority_order\":{}}},\
             \"unknown_order_events\":{},\"crossed_snapshots\":{},\
             \"crossed_book\":{{\"episodes\":{},\"total_us\":{},\"longest_us\":{}}},\"out_of_order_events\":{},\"examples\":[{}]}}",
            self.events,
            self.event_stamp_us,
            self.snapshots,
            self.exact,
            self.explained,
            self.exact_within_stamp,
            self.matched_with_offset,
            quantiles(&self.offsets_us),
            quantiles(&self.offset_ends_us),
            self.unexplained,
            self.first_unexplained.map_or("null".into(), |n| n.to_string()),
            self.revealed,
            self.revealed_above_first_created_id,
            self.corrected,
            self.retired,
            self.missing,
            self.extra,
            self.amount_differs,
            self.price_differs,
            self.order_differs,
            self.unknown_order_events,
            self.crossed_snapshots,
            self.crossed_book_episodes,
            self.crossed_book_us,
            self.crossed_book_longest_us,
            self.out_of_order_events,
            self.examples.join(",")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: i64, kind: L3Kind, id: u64, side: Side, price: i64, amount: i64) -> L3Event {
        L3Event { ts_us: ts, kind, id, side, price, amount }
    }

    #[test]
    fn a_stream_with_a_hidden_opening_book_is_explained_then_exact() {
        // Before the recording, bid 7 (price 100, amount 5) rests. The stream creates bid 10 at
        // 101 and ask 11 at 103, partly fills 10, deletes the unknown 7, then creates ask 12.
        let events = [
            ev(1, L3Kind::Created, 10, Side::Buy, 101, 4),
            ev(2, L3Kind::Created, 11, Side::Sell, 103, 6),
            ev(4, L3Kind::Changed, 10, Side::Buy, 101, 1),
            ev(5, L3Kind::Deleted, 7, Side::Buy, 100, 5),
            ev(6, L3Kind::Created, 12, Side::Sell, 103, 2),
        ];
        let snaps = [
            // t = 3: the vendor shows the unknown bid 7 too (explained: revealed).
            L3Snapshot { ts_us: 3, bids: vec![(101, 4, 10), (100, 5, 7)], asks: vec![(103, 6, 11)] },
            // t = 4 and 6: exact.
            L3Snapshot { ts_us: 4, bids: vec![(101, 1, 10), (100, 5, 7)], asks: vec![(103, 6, 11)] },
            L3Snapshot { ts_us: 6, bids: vec![(101, 1, 10)], asks: vec![(103, 6, 11), (103, 2, 12)] },
        ];
        let v = validate_l3(&events, &snaps, 100, 0, 1);
        assert_eq!((v.snapshots, v.exact, v.explained, v.unexplained, v.revealed), (3, 2, 1, 0, 1));
        assert_eq!((v.events, v.unknown_order_events, v.crossed_snapshots), (5, 0, 0), "the deletion of 7 came after it was revealed");
        assert_eq!((v.revealed_above_first_created_id, v.corrected, v.retired), (0, 0, 0));
    }

    #[test]
    fn differences_on_orders_the_stream_created_are_unexplained() {
        let events = [ev(1, L3Kind::Created, 10, Side::Buy, 101, 4), ev(2, L3Kind::Created, 11, Side::Buy, 100, 3)];
        // Wrong amount on 10; 11 missing from the vendor's full list of 1.
        let snaps = [L3Snapshot { ts_us: 3, bids: vec![(101, 2, 10)], asks: vec![] }];
        let v = validate_l3(&events, &snaps, 1, 0, 1);
        assert_eq!((v.unexplained, v.amount_differs, v.extra, v.first_unexplained), (1, 1, 0, Some(1)), "11 lies outside a full list of depth 1");
        assert_eq!(
            v.examples[0],
            "{\"snapshot\":1,\"ts_us\":3,\"missing\":[],\"extra\":[],\"amount\":[{\"id\":10,\"ours\":4,\"theirs\":2,\"created_dt_us\":-2,\"changed_dt_us\":[],\"deleted_dt_us\":null}],\"price\":[],\"priority_order_sides\":0}"
        );
        let v = validate_l3(&events, &[L3Snapshot { ts_us: 3, bids: vec![(101, 4, 10)], asks: vec![] }], 100, 0, 1);
        assert_eq!((v.unexplained, v.extra), (1, 1), "with room for more, the absent 11 is an extra order");
        let v = validate_l3(&events, &[L3Snapshot { ts_us: 3, bids: vec![(102, 4, 10), (100, 3, 11)], asks: vec![] }], 100, 0, 1);
        assert_eq!((v.unexplained, v.price_differs, v.amount_differs, v.order_differs), (1, 1, 0, 0), "10 at another price");
        // A crossed snapshot showing an unknown ask: the ask is revealed, the snapshot explained.
        let crossed = [L3Snapshot { ts_us: 3, bids: vec![(101, 4, 10), (100, 3, 11)], asks: vec![(100, 1, 99)] }];
        let v = validate_l3(&events, &crossed, 100, 0, 1);
        assert_eq!((v.crossed_snapshots, v.explained, v.revealed, v.revealed_above_first_created_id), (1, 1, 1, 1), "99 is above the first created id, 10");
    }

    #[test]
    fn a_snapshot_inside_its_own_microsecond_counts_as_exact() {
        // Two events share the snapshot's stamp (t = 5); the vendor's snapshot reflects only the
        // first. That is a timing ambiguity, not a reconstruction error.
        let events = [ev(1, L3Kind::Created, 10, Side::Buy, 101, 4), ev(5, L3Kind::Created, 11, Side::Buy, 100, 3), ev(5, L3Kind::Deleted, 10, Side::Buy, 101, 0)];
        let snaps = [L3Snapshot { ts_us: 5, bids: vec![(101, 4, 10), (100, 3, 11)], asks: vec![] }, L3Snapshot { ts_us: 6, bids: vec![(100, 3, 11)], asks: vec![] }];
        let v = validate_l3(&events, &snaps, 100, 0, 1);
        assert_eq!((v.exact, v.exact_within_stamp, v.unexplained), (2, 1, 0));
    }

    #[test]
    fn events_stamped_in_the_snapshots_last_stamp_unit_cannot_be_ordered_against_it() {
        // Order events carry whole milliseconds; the snapshot at 10_500 µs shows the book before
        // the deletion stamped 10_000, which happened somewhere in [10_000, 11_000). With 1 µs
        // stamps that needs an offset; with 1 ms stamps it is exact.
        let events = [ev(1_000, L3Kind::Created, 10, Side::Buy, 101, 4), ev(10_000, L3Kind::Deleted, 10, Side::Buy, 101, 0)];
        let snaps = [L3Snapshot { ts_us: 10_500, bids: vec![(101, 4, 10)], asks: vec![] }];
        let micro = validate_l3(&events, &snaps, 100, 5_000, 1);
        assert_eq!((micro.exact, micro.matched_with_offset, micro.offsets_us.clone(), micro.offset_ends_us.clone()), (0, 1, vec![-5_000], vec![-500]), "began is clamped to the window");
        let milli = validate_l3(&events, &snaps, 100, 5_000, 1_000);
        assert_eq!((milli.exact, milli.exact_within_stamp, milli.matched_with_offset, milli.event_stamp_us), (1, 1, 0, 1_000));
        // A deletion stamped a full unit before the snapshot certainly preceded it.
        let late = [L3Snapshot { ts_us: 11_000, bids: vec![(101, 4, 10)], asks: vec![] }];
        let v = validate_l3(&events, &late, 100, 5_000, 1_000);
        assert_eq!((v.exact, v.matched_with_offset, v.offset_ends_us.clone()), (0, 1, vec![-1_000]));
    }

    #[test]
    fn a_snapshot_stamped_after_its_book_is_found_by_the_offset_search() {
        // The vendor's snapshot at t = 10 shows the book as it was before the deletion at t = 8:
        // its stamp lags the order channel. Exact at its own time: no. Within ±5: yes, offset −3
        // (the book at t = 7, after the last event the snapshot reflects).
        let events = [ev(1, L3Kind::Created, 10, Side::Buy, 101, 4), ev(7, L3Kind::Created, 11, Side::Buy, 100, 3), ev(8, L3Kind::Deleted, 10, Side::Buy, 101, 0)];
        let snaps = [L3Snapshot { ts_us: 10, bids: vec![(101, 4, 10), (100, 3, 11)], asks: vec![] }];
        let without = validate_l3(&events, &snaps, 100, 0, 1);
        assert_eq!((without.exact, without.unexplained), (0, 1));
        let with = validate_l3(&events, &snaps, 100, 5, 1);
        assert_eq!((with.exact, with.matched_with_offset, with.offsets_us.clone(), with.unexplained), (0, 1, vec![-3], 0));
        assert_eq!(with.offset_ends_us, vec![-2], "the matching state ended with the deletion at t = 8");
    }

    #[test]
    fn a_lagging_snapshot_that_also_reveals_an_order_is_matched_with_offset() {
        // The snapshot at t = 10 lags (it shows 10, deleted at t = 8) and is the first to show the
        // pre-recording bid 7. The offset search must reveal 7 in its copy too.
        let events = [ev(1, L3Kind::Created, 10, Side::Buy, 101, 4), ev(7, L3Kind::Created, 11, Side::Buy, 99, 3), ev(8, L3Kind::Deleted, 10, Side::Buy, 101, 0)];
        let snaps = [
            L3Snapshot { ts_us: 10, bids: vec![(101, 4, 10), (100, 5, 7), (99, 3, 11)], asks: vec![] },
            L3Snapshot { ts_us: 12, bids: vec![(100, 5, 7), (99, 3, 11)], asks: vec![] },
        ];
        let v = validate_l3(&events, &snaps, 100, 5, 1);
        assert_eq!((v.exact, v.explained, v.matched_with_offset, v.offsets_us.clone(), v.unexplained, v.revealed), (1, 0, 1, vec![-3], 0, 1));
        assert_eq!(v.offset_ends_us, vec![-2]);
        let v = validate_l3(&events, &snaps, 100, 0, 1);
        assert_eq!((v.exact, v.unexplained, v.missing, v.revealed), (1, 1, 1, 1), "10 is missing: the stream deleted it");
    }

    #[test]
    fn a_revealed_order_is_retired_only_when_the_stream_says_nothing_about_it() {
        // Ask 8 is revealed at t = 3; the snapshot at t = 6 no longer shows it; the stream deletes
        // it at t = 9. Order 30 is deleted without ever being shown (an unknown order).
        let events = [ev(1, L3Kind::Created, 10, Side::Buy, 101, 4), ev(9, L3Kind::Deleted, 8, Side::Sell, 104, 0), ev(20, L3Kind::Deleted, 30, Side::Sell, 110, 0)];
        let snaps = [
            L3Snapshot { ts_us: 3, bids: vec![(101, 4, 10)], asks: vec![(104, 1, 8)] },
            L3Snapshot { ts_us: 6, bids: vec![(101, 4, 10)], asks: vec![] },
        ];
        // Without a tolerance the deletion at t = 9 is outside the comparison: 8 is retired.
        let v = validate_l3(&events, &snaps, 100, 0, 1);
        assert_eq!((v.explained, v.retired, v.unknown_order_events, v.revealed, v.events), (2, 1, 1, 1, 3));
        // Within ±5 the stream's own deletion explains it: matched 3 µs later, nothing retired.
        let v = validate_l3(&events, &snaps, 100, 5, 1);
        assert_eq!((v.explained, v.matched_with_offset, v.offsets_us.clone(), v.retired, v.unknown_order_events), (1, 1, vec![3], 0, 1));
        assert_eq!(v.offset_ends_us, vec![5], "still matching at the window's end (+tolerance)");
    }

    #[test]
    fn crossed_books_are_timed_from_the_first_snapshot() {
        // A sell at 100 rests against the bid at 101 until its trade (same stamp: 0 µs); a sell
        // at 101 locks the book from t = 8 to t = 9. A crossing before the first snapshot is not
        // tracked (the book may lack pre-recording orders then).
        let events = [
            ev(0, L3Kind::Created, 9, Side::Sell, 90, 1),
            ev(1, L3Kind::Created, 10, Side::Buy, 101, 4),
            ev(1, L3Kind::Deleted, 9, Side::Sell, 90, 0),
            ev(2, L3Kind::Created, 11, Side::Sell, 103, 6),
            ev(5, L3Kind::Created, 12, Side::Sell, 100, 1),
            ev(5, L3Kind::Changed, 10, Side::Buy, 101, 3),
            ev(5, L3Kind::Deleted, 12, Side::Sell, 100, 0),
            ev(8, L3Kind::Created, 13, Side::Sell, 101, 1),
            ev(9, L3Kind::Changed, 10, Side::Buy, 101, 2),
            ev(9, L3Kind::Deleted, 13, Side::Sell, 101, 0),
        ];
        let snaps = [
            L3Snapshot { ts_us: 3, bids: vec![(101, 4, 10)], asks: vec![(103, 6, 11)] },
            L3Snapshot { ts_us: 10, bids: vec![(101, 2, 10)], asks: vec![(103, 6, 11)] },
        ];
        let v = validate_l3(&events, &snaps, 100, 0, 1);
        assert_eq!((v.exact, v.crossed_snapshots), (2, 0));
        assert_eq!((v.crossed_book_episodes, v.crossed_book_us, v.crossed_book_longest_us), (2, 1, 1));
        assert!(v.to_json().contains("\"crossed_book\":{\"episodes\":2,\"total_us\":1,\"longest_us\":1}"));
    }

    #[test]
    fn a_snapshot_never_reveals_an_order_the_stream_creates_later() {
        // The snapshot at t = 3 shows bid 10, which the stream creates only at t = 5: the snapshot
        // is ahead of the order channel, or the creation is late. A difference, not a reveal.
        let events = [ev(5, L3Kind::Created, 10, Side::Buy, 101, 4)];
        let snaps = [L3Snapshot { ts_us: 3, bids: vec![(101, 4, 10)], asks: vec![] }];
        let v = validate_l3(&events, &snaps, 100, 0, 1);
        assert_eq!((v.unexplained, v.missing, v.revealed), (1, 1, 0));
        // Within ±5 it matches the book after the creation: offset +2.
        let v = validate_l3(&events, &snaps, 100, 5, 1);
        assert_eq!((v.matched_with_offset, v.offsets_us.clone(), v.revealed), (1, vec![2], 0));
    }

    #[test]
    fn a_revealed_order_changed_without_an_event_is_corrected() {
        let events = [ev(1, L3Kind::Created, 10, Side::Buy, 101, 4)];
        let snaps = [
            L3Snapshot { ts_us: 3, bids: vec![(101, 4, 10), (100, 5, 7)], asks: vec![] },
            L3Snapshot { ts_us: 4, bids: vec![(101, 4, 10), (100, 3, 7)], asks: vec![] },
            L3Snapshot { ts_us: 5, bids: vec![(101, 4, 10), (100, 3, 7)], asks: vec![] },
        ];
        let v = validate_l3(&events, &snaps, 100, 0, 1);
        assert_eq!((v.explained, v.exact, v.corrected, v.revealed), (2, 1, 1, 1));
    }

    #[test]
    fn priority_order_within_a_price_is_checked() {
        let events = [ev(1, L3Kind::Created, 10, Side::Sell, 103, 1), ev(2, L3Kind::Created, 11, Side::Sell, 103, 1)];
        let v = validate_l3(&events, &[L3Snapshot { ts_us: 3, bids: vec![], asks: vec![(103, 1, 11), (103, 1, 10)] }], 100, 0, 1);
        assert_eq!((v.unexplained, v.order_differs, v.missing, v.extra), (1, 1, 0, 0));
    }

    #[test]
    fn files_parse() {
        let e = parse_events("ts_us,kind,order_id,side,price,amount\n5,created,10,buy,6452973,7748366\n6,deleted,10,buy,6452973,0\n").expect("events");
        assert_eq!(e[0], ev(5, L3Kind::Created, 10, Side::Buy, 6_452_973, 7_748_366));
        assert_eq!(e[1].kind, L3Kind::Deleted);
        let s = parse_snapshots("ts_us,snapshot,side,rank,order_id,price,amount\n9,1,buy,1,10,101,4\n9,1,sell,1,11,103,6\n12,2,buy,1,10,101,1\n").expect("snapshots");
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].bids.clone(), s[0].asks.clone(), s[1].ts_us), (vec![(101, 4, 10)], vec![(103, 6, 11)], 12));
        assert!(parse_events("1,opened,1,buy,1,1").is_err());
    }
}
