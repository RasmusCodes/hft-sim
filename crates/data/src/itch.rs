//! Nasdaq TotalView-ITCH 5.0, as Nasdaq publishes sample days of it (emi.nasdaq.com/ITCH/): a
//! binary file in which every message is preceded by its length as a 2-byte big-endian integer
//! (docs/HISTORICAL_DATA.md §7).
//!
//! **What is verified, and how.** The field layouts below are written from memory of the ITCH 5.0
//! specification [M]: Nasdaq's specification page could not be loaded. Two checks on the data
//! itself stand in for it:
//! * every message carries its own length, so each type's length is compared with the layout's
//!   ([`ItchCheck::length_mismatches`]);
//! * wrong field offsets would show as order references that were never added, executions or
//!   cancellations larger than the order, orders that change stock, or crossed books. The book
//!   rebuild counts all of them.
//!
//! A day's file starts with an empty book, so no order is unknown at the start (unlike LOBSTER
//! files or a live recording). Prices have 4 implied decimals, the same scale as LOBSTER's.
//!
//! **Displayed price and ranked price.** An order can be displayed at one price and ranked at a
//! better, non-displayed one: a Price to Comply order that would lock or cross a protected
//! quotation is displayed one increment away and ranked at the quotation's price (BX and Nasdaq
//! Rule 4702(b)(1)(A); Post-Only orders that are not attributable are treated the same way). Its
//! executions there arrive as `C` messages ("executed with price"). In LOBSTER form they carry
//! the price they traded at (the rebuild still reduces the order at its own level), and the
//! arrival-order check counts them apart, since they trade ahead of the displayed queue by price.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{self, Read};

use lob::Side;

use crate::lobster::{Kind, Message, Rebuilder, Snapshot};

/// Bytes before every message body: type, stock locate, tracking number, timestamp.
pub const HEADER: usize = 11;

/// The length of each message type, by the ITCH 5.0 layout as implemented ([M]; checked against
/// the data by [`ItchCheck::length_mismatches`]).
pub fn expected_len(kind: u8) -> Option<usize> {
    Some(match kind {
        b'S' | b'W' => 12,
        b'R' => 39,
        b'H' => 25,
        b'Y' | b'N' => 20,
        b'L' => 26,
        b'V' | b'J' | b'U' => 35,
        b'K' => 28,
        b'h' => 21,
        b'A' | b'C' => 36,
        b'F' | b'Q' => 40,
        b'E' => 31,
        b'X' => 23,
        b'D' | b'B' => 19,
        b'P' => 44,
        b'I' => 50,
        b'O' => 48,
        _ => return None,
    })
}

fn be(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |acc, &x| (acc << 8) | u64::from(x))
}

fn side_of(b: u8) -> Option<Side> {
    match b {
        b'B' => Some(Side::Buy),
        b'S' => Some(Side::Sell),
        _ => None,
    }
}

/// The fields of one message that the book and the checks use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Itch {
    /// System event: `O` start of messages, `S` start of system hours, `Q` start of market hours,
    /// `M` end of market hours, `E` end of system hours, `C` end of messages.
    System { code: u8 },
    /// Stock directory: one per stock, before its orders.
    Directory { stock: [u8; 8], round_lot: u32 },
    /// Trading action: `T` trading, `H` halted, `P` paused, `Q` quotation only.
    Trading { stock: [u8; 8], state: u8 },
    /// Add order (`A`, or `F` with an attribution).
    Add { oref: u64, side: Side, shares: u32, stock: [u8; 8], price: u32 },
    /// Order executed at its own price.
    Executed { oref: u64, shares: u32, match_no: u64 },
    /// Order executed at another price (crosses), `printable` if it counts toward volume.
    ExecutedPrice { oref: u64, shares: u32, match_no: u64, printable: bool, price: u32 },
    /// Part of an order cancelled.
    Cancel { oref: u64, shares: u32 },
    /// Order deleted.
    Delete { oref: u64 },
    /// Order replaced: the old reference is gone, the new one carries shares and price (a new
    /// order for priority).
    Replace { old: u64, new: u64, shares: u32, price: u32 },
    /// Execution against a non-displayed order.
    Trade { side: Side, shares: u32, stock: [u8; 8], price: u32, match_no: u64 },
    /// Opening, closing or other cross.
    Cross { shares: u64, stock: [u8; 8], price: u32, match_no: u64, cross_type: u8 },
    /// A trade broken after the fact.
    Broken { match_no: u64 },
    /// Any other message (directory details, Reg SHO, imbalances and so on).
    Other,
}

/// One message with its header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    /// Message type byte.
    pub kind: u8,
    /// Stock locate code (the day's index of the stock).
    pub locate: u16,
    /// Nanoseconds after midnight.
    pub ts_ns: i64,
    /// Fields.
    pub msg: Itch,
}

fn stock(b: &[u8]) -> [u8; 8] {
    let mut s = [b' '; 8];
    s.copy_from_slice(&b[..8]);
    s
}

/// The symbol, without its padding.
pub fn symbol(s: &[u8; 8]) -> String {
    String::from_utf8_lossy(s).trim_end().to_string()
}

/// Parse one message body (without its length prefix). `None` if it is shorter than its type
/// needs or a side byte is not `B` or `S`.
pub fn parse_record(b: &[u8]) -> Option<Record> {
    if b.len() < HEADER {
        return None;
    }
    let kind = b[0];
    let need = expected_len(kind).unwrap_or(HEADER);
    if b.len() < need {
        return None;
    }
    let (locate, ts_ns) = (be(&b[1..3]) as u16, be(&b[5..11]) as i64);
    let u32_at = |k: usize| be(&b[k..k + 4]) as u32;
    let u64_at = |k: usize| be(&b[k..k + 8]);
    let msg = match kind {
        b'S' => Itch::System { code: b[11] },
        b'R' => Itch::Directory { stock: stock(&b[11..19]), round_lot: u32_at(21) },
        b'H' => Itch::Trading { stock: stock(&b[11..19]), state: b[19] },
        b'A' | b'F' => Itch::Add { oref: u64_at(11), side: side_of(b[19])?, shares: u32_at(20), stock: stock(&b[24..32]), price: u32_at(32) },
        b'E' => Itch::Executed { oref: u64_at(11), shares: u32_at(19), match_no: u64_at(23) },
        b'C' => Itch::ExecutedPrice { oref: u64_at(11), shares: u32_at(19), match_no: u64_at(23), printable: b[31] == b'Y', price: u32_at(32) },
        b'X' => Itch::Cancel { oref: u64_at(11), shares: u32_at(19) },
        b'D' => Itch::Delete { oref: u64_at(11) },
        b'U' => Itch::Replace { old: u64_at(11), new: u64_at(19), shares: u32_at(27), price: u32_at(31) },
        b'P' => Itch::Trade { side: side_of(b[19])?, shares: u32_at(20), stock: stock(&b[24..32]), price: u32_at(32), match_no: u64_at(36) },
        b'Q' => Itch::Cross { shares: u64_at(11), stock: stock(&b[19..27]), price: u32_at(27), match_no: u64_at(31), cross_type: b[39] },
        b'B' => Itch::Broken { match_no: u64_at(11) },
        _ => Itch::Other,
    };
    Some(Record { kind, locate, ts_ns, msg })
}

/// Reads length-prefixed messages one at a time.
pub struct Reader<R: Read> {
    inner: R,
    buf: Vec<u8>,
}

impl<R: Read> Reader<R> {
    /// Wrap a reader (buffer it: messages are small).
    pub fn new(inner: R) -> Self {
        Reader { inner, buf: Vec::with_capacity(64) }
    }

    /// The next message body, or `None` at a clean end of file.
    pub fn next_message(&mut self) -> io::Result<Option<&[u8]>> {
        let mut len = [0u8; 2];
        match self.inner.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        self.buf.resize(usize::from(u16::from_be_bytes(len)), 0);
        self.inner.read_exact(&mut self.buf)?;
        Ok(Some(&self.buf))
    }
}

/// How an order came to be: added, or the new side of a replacement (relative to the order it
/// replaced: same price with fewer shares, same price otherwise, or a new price).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Added,
    SamePriceSmaller,
    SamePriceNotSmaller,
    NewPrice,
}

#[derive(Clone, Copy, Debug)]
struct Order {
    locate: u16,
    side: Side,
    price: u32,
    shares: u32,
    /// The message number of the add this order descends from through replacements.
    origin: u64,
    how: Origin,
    /// When it was added, and whether it carried a market participant id (`F`).
    added_ns: i64,
    attributed: bool,
}

/// Which order the exchange executed at a price level, against arrival order. A replacement is
/// a new order in [`read_file`]'s LOBSTER form; if the exchange kept the replaced order's place
/// instead, executions of replacements would jump the queue, and each such jump would be
/// explained by the replaced order's older origin. Executions at a price other than the displayed
/// one are counted apart (see the module documentation).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Priority {
    /// Executions at the displayed price of the order at the front of its level (by arrival,
    /// replacements counted as new).
    pub at_front: u64,
    /// Executions at a price other than the displayed one (`C`), and of those, the ones made
    /// while an older order was displayed at the same price.
    pub off_display: u64,
    /// See `off_display`.
    pub off_display_ahead_of_older: u64,
    /// Executions of an order behind others, by how it came to be: added, replaced at the same
    /// price with fewer shares, replaced at the same price otherwise, replaced at a new price.
    pub jumps: [u64; 4],
    /// Of those, jumps the order's origin explains: it descends from an add older than the front
    /// order's.
    pub explained: [u64; 4],
    /// Jumps where the order passed over (the front) was an odd lot (fewer shares than the
    /// stock's round lot), where the executed order was, and where both carried an attribution.
    pub front_odd_lot: u64,
    /// See `front_odd_lot`.
    pub jumper_odd_lot: u64,
    /// See `front_odd_lot`.
    pub both_attributed: u64,
    /// Up to 20 jumps as JSON, for inspection.
    pub examples: Vec<String>,
}

/// The displayed book of one stock, by price level.
#[derive(Clone, Debug, Default)]
struct Levels {
    sides: [BTreeMap<u32, u64>; 2],
}

fn idx(side: Side) -> usize {
    match side {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

impl Levels {
    fn change(&mut self, side: Side, price: u32, delta: i64) {
        let m = &mut self.sides[idx(side)];
        let q = m.entry(price).or_insert(0);
        *q = (*q as i64 + delta).max(0) as u64;
        if *q == 0 {
            m.remove(&price);
        }
    }

    fn best(&self) -> (Option<u32>, Option<u32>) {
        (self.sides[0].keys().next_back().copied(), self.sides[1].keys().next().copied())
    }

    fn crossed(&self) -> bool {
        matches!(self.best(), (Some(b), Some(a)) if b >= a)
    }

    fn top(&self, n: usize) -> Snapshot {
        Snapshot {
            asks: self.sides[1].iter().take(n).map(|(&p, &q)| (i64::from(p), q as i64)).collect(),
            bids: self.sides[0].iter().rev().take(n).map(|(&p, &q)| (i64::from(p), q as i64)).collect(),
        }
    }
}

/// Per-stock counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StockStats {
    /// Symbol.
    pub symbol: String,
    /// Messages about it.
    pub messages: u64,
    /// Orders added (including the new side of replacements).
    pub adds: u64,
    /// Visible executions (`E`, `C`).
    pub executions: u64,
    /// Shares in them.
    pub executed_shares: u64,
    /// Lowest and highest execution price (4 decimals).
    pub price_range: (u32, u32),
    /// Times its book became crossed or locked.
    pub crossed_episodes: u64,
}

/// What reading a whole file found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItchCheck {
    /// Messages read.
    pub messages: u64,
    /// Messages by type.
    pub by_type: BTreeMap<char, u64>,
    /// Types whose length differs from the layout: (count, first length seen).
    pub length_mismatches: BTreeMap<char, (u64, usize)>,
    /// Messages of a type the layout does not know.
    pub unknown_types: u64,
    /// Messages too short for their type, or with a side other than `B`/`S`.
    pub malformed: u64,
    /// Stock directory entries.
    pub stocks: usize,
    /// Orders added (`A`, `F`, and the new side of `U`).
    pub orders_added: u64,
    /// Executions, cancellations, deletions and replacements of a reference never added.
    pub unknown_refs: u64,
    /// Adds of a reference already live.
    pub duplicate_refs: u64,
    /// Messages whose stock locate differs from their order's.
    pub locate_mismatches: u64,
    /// Executions or cancellations larger than the order's remaining shares.
    pub oversize: u64,
    /// Messages stamped earlier than the one before.
    pub out_of_order: u64,
    /// Times a stock's displayed book became crossed or locked, by session: before, during and
    /// after market hours (system events `Q` and `M`).
    pub crossed_episodes: [u64; 3],
    /// Shares executed visibly (`E`, `C`), against non-displayed orders (`P`), and in crosses (`Q`).
    pub executed_shares: u64,
    /// See `executed_shares`.
    pub hidden_shares: u64,
    /// See `executed_shares`.
    pub cross_shares: u64,
    /// Broken trades.
    pub broken: u64,
    /// Orders still live at the end of the file.
    pub live_at_end: usize,
    /// Executions against the arrival order of their price level.
    pub priority: Priority,
    /// First and last timestamp; market open and close (`Q`, `M`).
    pub first_ns: i64,
    /// See `first_ns`.
    pub last_ns: i64,
    /// See `first_ns`.
    pub market_open_ns: Option<i64>,
    /// See `first_ns`.
    pub market_close_ns: Option<i64>,
    /// Per stock, by locate.
    pub per_stock: HashMap<u16, StockStats>,
}

/// One pass over a file: the book of every stock, every check, and (for the stocks in `export`)
/// the messages in LOBSTER form, checked against an independent rebuild (LOBSTER's
/// [`Rebuilder`]) after every message: `cross_mismatches` counts the times their best
/// `levels` levels differed.
pub struct Pass {
    /// The checks.
    pub check: ItchCheck,
    /// Messages per exported stock, in LOBSTER form.
    pub exported: HashMap<u16, Vec<Message>>,
    /// Comparisons with the independent rebuild, and how many differed.
    pub cross_compared: u64,
    /// See `cross_compared`.
    pub cross_mismatches: u64,
}

/// Read a whole file (see [`Pass`]).
pub fn read_file<R: Read>(reader: R, export: &HashSet<u16>, levels: usize) -> io::Result<Pass> {
    let mut r = Reader::new(reader);
    let mut c = ItchCheck { first_ns: i64::MIN, ..ItchCheck::default() };
    let mut orders: HashMap<u64, Order> = HashMap::with_capacity(1 << 20);
    let mut books: HashMap<u16, Levels> = HashMap::new();
    let mut crossed: HashSet<u16> = HashSet::new();
    let mut exported: HashMap<u16, Vec<Message>> = export.iter().map(|&l| (l, Vec::new())).collect();
    let mut rebuilders: HashMap<u16, Rebuilder> = export.iter().map(|&l| (l, Rebuilder::new())).collect();
    let (mut cross_compared, mut cross_mismatches) = (0u64, 0u64);
    let mut last_ts = i64::MIN;
    let mut session = 0usize;
    // Arrival order per price level, with orders that left still in it until they reach the
    // front; a level's queue goes when the level empties.
    let mut fifo: HashMap<(u16, usize, u32), VecDeque<u64>> = HashMap::new();
    let mut round_lots: HashMap<u16, u32> = HashMap::new();
    while let Some(b) = r.next_message()? {
        c.messages += 1;
        let kind = b[0];
        *c.by_type.entry(char::from(kind)).or_default() += 1;
        match expected_len(kind) {
            None => c.unknown_types += 1,
            Some(n) if n != b.len() => {
                let e = c.length_mismatches.entry(char::from(kind)).or_insert((0, b.len()));
                e.0 += 1;
            }
            _ => {}
        }
        let Some(rec) = parse_record(b) else {
            c.malformed += 1;
            continue;
        };
        if c.first_ns == i64::MIN {
            c.first_ns = rec.ts_ns;
        }
        c.out_of_order += u64::from(rec.ts_ns < last_ts);
        last_ts = last_ts.max(rec.ts_ns);
        c.last_ns = last_ts;
        let ts = rec.ts_ns;
        let mut out: Vec<Message> = Vec::new();
        let mut touched: Option<u16> = None;
        // The displayed level an order left or shrank at, if any.
        let mut left_level: Option<(Side, u32)> = None;
        // Look up a live order, checking that it belongs to this message's stock.
        let known = |oref: u64, c: &mut ItchCheck| -> Option<Order> {
            match orders.get(&oref) {
                None => {
                    c.unknown_refs += 1;
                    None
                }
                Some(o) => {
                    c.locate_mismatches += u64::from(o.locate != rec.locate);
                    Some(*o)
                }
            }
        };
        match rec.msg {
            Itch::System { code } => match code {
                b'Q' => {
                    c.market_open_ns = Some(ts);
                    session = 1;
                }
                b'M' => {
                    c.market_close_ns = Some(ts);
                    session = 2;
                }
                _ => {}
            },
            Itch::Directory { stock, round_lot } => {
                c.stocks += 1;
                c.per_stock.entry(rec.locate).or_default().symbol = symbol(&stock);
                round_lots.insert(rec.locate, round_lot);
            }
            Itch::Add { oref, side, shares, price, .. } => {
                c.orders_added += 1;
                let order = Order { locate: rec.locate, side, price, shares, origin: c.messages, how: Origin::Added, added_ns: ts, attributed: rec.kind == b'F' };
                if orders.insert(oref, order).is_some() {
                    c.duplicate_refs += 1;
                }
                books.entry(rec.locate).or_default().change(side, price, i64::from(shares));
                fifo.entry((rec.locate, idx(side), price)).or_default().push_back(oref);
                out.push(Message { ts_ns: ts, kind: Kind::Submit, order_id: oref as i64, size: i64::from(shares), price: i64::from(price), side });
                touched = Some(rec.locate);
                c.per_stock.entry(rec.locate).or_default().adds += 1;
            }
            Itch::Executed { oref, shares, .. } | Itch::ExecutedPrice { oref, shares, .. } => {
                if let Some(o) = known(oref, &mut c) {
                    c.oversize += u64::from(shares > o.shares);
                    let take = shares.min(o.shares);
                    c.executed_shares += u64::from(shares);
                    let s = c.per_stock.entry(o.locate).or_default();
                    s.executions += 1;
                    s.executed_shares += u64::from(shares);
                    let px = if let Itch::ExecutedPrice { price, .. } = rec.msg { price } else { o.price };
                    s.price_range = if s.executions == 1 { (px, px) } else { (s.price_range.0.min(px), s.price_range.1.max(px)) };
                    let off_display = px != o.price;
                    c.priority.off_display += u64::from(off_display);
                    // Was it the first order at its level, by arrival?
                    if let Some(q) = fifo.get_mut(&(o.locate, idx(o.side), o.price)) {
                        while q.front().is_some_and(|f| orders.get(f).is_none_or(|x| x.price != o.price || x.locate != o.locate)) {
                            q.pop_front();
                        }
                        match q.front() {
                            Some(&f) if f != oref && off_display => c.priority.off_display_ahead_of_older += 1,
                            Some(&f) if f != oref => {
                                let (k, front) = (o.how as usize, orders[&f]);
                                let pr = &mut c.priority;
                                pr.jumps[k] += 1;
                                pr.explained[k] += u64::from(o.origin < front.origin);
                                let lot = round_lots.get(&o.locate).copied().unwrap_or(100);
                                pr.front_odd_lot += u64::from(front.shares < lot);
                                pr.jumper_odd_lot += u64::from(o.shares < lot);
                                pr.both_attributed += u64::from(front.attributed && o.attributed);
                                if pr.examples.len() < 20 {
                                    pr.examples.push(format!(
                                        "{{\"stock\":\"{}\",\"ts_ns\":{ts},\"side\":\"{}\",\"price\":{},\"executed\":{shares},\
                                         \"front\":{{\"id\":{f},\"shares\":{},\"age_ns\":{},\"attributed\":{},\"how\":\"{:?}\"}},\
                                         \"jumper\":{{\"id\":{oref},\"shares\":{},\"age_ns\":{},\"attributed\":{},\"how\":\"{:?}\"}}}}",
                                        c.per_stock.get(&o.locate).map_or("?", |s| s.symbol.as_str()),
                                        if o.side == Side::Buy { "buy" } else { "sell" },
                                        o.price,
                                        front.shares,
                                        ts - front.added_ns,
                                        front.attributed,
                                        front.how,
                                        o.shares,
                                        ts - o.added_ns,
                                        o.attributed,
                                        o.how
                                    ));
                                }
                            }
                            _ if off_display => {}
                            _ => c.priority.at_front += 1,
                        }
                    }
                    books.entry(o.locate).or_default().change(o.side, o.price, -i64::from(take));
                    // In LOBSTER form an execution carries the price it traded at; LOBSTER's
                    // rebuild reduces the order at its own level, and the replay model reads an
                    // execution away from that level as one at a non-displayed price.
                    out.push(Message { ts_ns: ts, kind: Kind::ExecuteVisible, order_id: oref as i64, size: i64::from(take), price: i64::from(px), side: o.side });
                    reduce(&mut orders, oref, take);
                    touched = Some(o.locate);
                    left_level = Some((o.side, o.price));
                }
            }
            Itch::Cancel { oref, shares } => {
                if let Some(o) = known(oref, &mut c) {
                    c.oversize += u64::from(shares > o.shares);
                    let take = shares.min(o.shares);
                    books.entry(o.locate).or_default().change(o.side, o.price, -i64::from(take));
                    out.push(Message { ts_ns: ts, kind: Kind::Cancel, order_id: oref as i64, size: i64::from(take), price: i64::from(o.price), side: o.side });
                    reduce(&mut orders, oref, take);
                    touched = Some(o.locate);
                    left_level = Some((o.side, o.price));
                }
            }
            Itch::Delete { oref } => {
                if let Some(o) = known(oref, &mut c) {
                    books.entry(o.locate).or_default().change(o.side, o.price, -i64::from(o.shares));
                    out.push(Message { ts_ns: ts, kind: Kind::Delete, order_id: oref as i64, size: i64::from(o.shares), price: i64::from(o.price), side: o.side });
                    orders.remove(&oref);
                    touched = Some(o.locate);
                    left_level = Some((o.side, o.price));
                }
            }
            Itch::Replace { old, new, shares, price } => {
                if let Some(o) = known(old, &mut c) {
                    let book = books.entry(o.locate).or_default();
                    book.change(o.side, o.price, -i64::from(o.shares));
                    book.change(o.side, price, i64::from(shares));
                    orders.remove(&old);
                    c.orders_added += 1;
                    let how = match (price == o.price, shares < o.shares) {
                        (true, true) => Origin::SamePriceSmaller,
                        (true, false) => Origin::SamePriceNotSmaller,
                        (false, _) => Origin::NewPrice,
                    };
                    if orders.insert(new, Order { locate: o.locate, side: o.side, price, shares, origin: o.origin, how, added_ns: ts, attributed: o.attributed }).is_some() {
                        c.duplicate_refs += 1;
                    }
                    fifo.entry((o.locate, idx(o.side), price)).or_default().push_back(new);
                    out.push(Message { ts_ns: ts, kind: Kind::Delete, order_id: old as i64, size: i64::from(o.shares), price: i64::from(o.price), side: o.side });
                    out.push(Message { ts_ns: ts, kind: Kind::Submit, order_id: new as i64, size: i64::from(shares), price: i64::from(price), side: o.side });
                    c.per_stock.entry(o.locate).or_default().adds += 1;
                    touched = Some(o.locate);
                    left_level = Some((o.side, o.price));
                }
            }
            Itch::Trade { side, shares, price, .. } => {
                c.hidden_shares += u64::from(shares);
                out.push(Message { ts_ns: ts, kind: Kind::ExecuteHidden, order_id: 0, size: i64::from(shares), price: i64::from(price), side });
            }
            Itch::Cross { shares, price, .. } => {
                c.cross_shares += shares;
                out.push(Message { ts_ns: ts, kind: Kind::Cross, order_id: 0, size: shares as i64, price: i64::from(price), side: Side::Buy });
            }
            Itch::Broken { .. } => c.broken += 1,
            Itch::Trading { .. } | Itch::Other => {}
        }
        c.per_stock.entry(rec.locate).or_default().messages += 1;
        // A level that emptied takes its arrival queue with it.
        if let (Some(l), Some((side, p))) = (touched, left_level) {
            if !books.get(&l).is_some_and(|b| b.sides[idx(side)].contains_key(&p)) {
                fifo.remove(&(l, idx(side), p));
            }
        }
        if let Some(l) = touched {
            if books.get(&l).is_some_and(Levels::crossed) {
                if crossed.insert(l) {
                    c.crossed_episodes[session] += 1;
                    c.per_stock.entry(l).or_default().crossed_episodes += 1;
                }
            } else {
                crossed.remove(&l);
            }
        }
        let target = touched.unwrap_or(rec.locate);
        if let Some(v) = exported.get_mut(&target) {
            let rb = rebuilders.get_mut(&target).expect("one per exported stock");
            for m in &out {
                rb.apply(m);
            }
            v.extend(out);
            if touched.is_some() {
                cross_compared += 1;
                let ours = books.get(&target).map(|b| b.top(levels)).unwrap_or_default();
                cross_mismatches += u64::from(rb.top(levels) != ours);
            }
        }
    }
    c.live_at_end = orders.len();
    if c.first_ns == i64::MIN {
        c.first_ns = 0;
    }
    Ok(Pass { check: c, exported, cross_compared, cross_mismatches })
}

fn reduce(orders: &mut HashMap<u64, Order>, oref: u64, take: u32) {
    if let Some(o) = orders.get_mut(&oref) {
        o.shares -= take;
        if o.shares == 0 {
            orders.remove(&oref);
        }
    }
}

impl ItchCheck {
    /// As a JSON object (`top` stocks by visible executions listed).
    pub fn to_json(&self, top: usize) -> String {
        let chars = |m: &BTreeMap<char, u64>| m.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect::<Vec<_>>().join(",");
        let lengths = self.length_mismatches.iter().map(|(k, (n, l))| format!("\"{k}\":{{\"count\":{n},\"first_length\":{l},\"expected\":{}}}", expected_len(*k as u8).unwrap_or(0))).collect::<Vec<_>>().join(",");
        let mut stocks: Vec<&StockStats> = self.per_stock.values().filter(|s| s.executions > 0).collect();
        stocks.sort_by(|a, b| b.executions.cmp(&a.executions).then(a.symbol.cmp(&b.symbol)));
        let listed = stocks
            .iter()
            .take(top)
            .map(|s| {
                format!(
                    "{{\"symbol\":\"{}\",\"messages\":{},\"adds\":{},\"executions\":{},\"executed_shares\":{},\"price_min\":{},\"price_max\":{},\"crossed_episodes\":{}}}",
                    s.symbol, s.messages, s.adds, s.executions, s.executed_shares, s.price_range.0, s.price_range.1, s.crossed_episodes
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let opt = |x: Option<i64>| x.map_or("null".into(), |v| v.to_string());
        let kinds = ["added", "replaced_same_price_fewer_shares", "replaced_same_price_otherwise", "replaced_new_price"];
        let per = |v: &[u64; 4]| kinds.iter().zip(v).map(|(k, n)| format!("\"{k}\":{n}")).collect::<Vec<_>>().join(",");
        let pr = &self.priority;
        let priority = format!(
            "{{\"executions_at_front\":{},\"executions_off_display\":{},\"off_display_ahead_of_older\":{},\"queue_jumps\":{{{}}},\"jumps_explained_by_origin\":{{{}}},\
             \"front_odd_lot\":{},\"jumper_odd_lot\":{},\"both_attributed\":{},\"examples\":[{}]}}",
            pr.at_front,
            pr.off_display,
            pr.off_display_ahead_of_older,
            per(&pr.jumps),
            per(&pr.explained),
            pr.front_odd_lot,
            pr.jumper_odd_lot,
            pr.both_attributed,
            pr.examples.join(",")
        );
        format!(
            "{{\"messages\":{},\"by_type\":{{{}}},\"length_mismatches\":{{{lengths}}},\"unknown_types\":{},\"malformed\":{},\"stocks\":{},\
             \"stocks_traded\":{},\"orders_added\":{},\"unknown_refs\":{},\"duplicate_refs\":{},\"locate_mismatches\":{},\"oversize\":{},\
             \"out_of_order\":{},\"crossed_episodes\":{{\"before_market\":{},\"market_hours\":{},\"after_market\":{}}},\
             \"shares\":{{\"executed_visible\":{},\"executed_hidden\":{},\"crosses\":{}}},\"broken_trades\":{},\"live_at_end\":{},\"priority\":{priority},\
             \"first_ns\":{},\"last_ns\":{},\"market_open_ns\":{},\"market_close_ns\":{},\"top_stocks\":[{listed}]}}",
            self.messages,
            chars(&self.by_type),
            self.unknown_types,
            self.malformed,
            self.stocks,
            stocks.len(),
            self.orders_added,
            self.unknown_refs,
            self.duplicate_refs,
            self.locate_mismatches,
            self.oversize,
            self.out_of_order,
            self.crossed_episodes[0],
            self.crossed_episodes[1],
            self.crossed_episodes[2],
            self.executed_shares,
            self.hidden_shares,
            self.cross_shares,
            self.broken,
            self.live_at_end,
            self.first_ns,
            self.last_ns,
            opt(self.market_open_ns),
            opt(self.market_close_ns),
        )
    }

    /// The `n` stocks with the most visible executions, by locate.
    pub fn most_traded(&self, n: usize) -> Vec<u16> {
        let mut v: Vec<(&u16, &StockStats)> = self.per_stock.iter().filter(|(_, s)| s.executions > 0).collect();
        v.sort_by(|a, b| b.1.executions.cmp(&a.1.executions).then(a.1.symbol.cmp(&b.1.symbol)));
        v.into_iter().take(n).map(|(&l, _)| l).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: u8, locate: u16, ts: u64) -> Vec<u8> {
        let mut v = vec![kind];
        v.extend_from_slice(&locate.to_be_bytes());
        v.extend_from_slice(&0u16.to_be_bytes());
        v.extend_from_slice(&ts.to_be_bytes()[2..]);
        v
    }

    fn sym(s: &str) -> [u8; 8] {
        let mut b = [b' '; 8];
        b[..s.len()].copy_from_slice(s.as_bytes());
        b
    }

    fn add(locate: u16, ts: u64, oref: u64, side: u8, shares: u32, s: &str, price: u32) -> Vec<u8> {
        let mut v = header(b'A', locate, ts);
        v.extend_from_slice(&oref.to_be_bytes());
        v.push(side);
        v.extend_from_slice(&shares.to_be_bytes());
        v.extend_from_slice(&sym(s));
        v.extend_from_slice(&price.to_be_bytes());
        v
    }

    fn exec(locate: u16, ts: u64, oref: u64, shares: u32, m: u64) -> Vec<u8> {
        let mut v = header(b'E', locate, ts);
        v.extend_from_slice(&oref.to_be_bytes());
        v.extend_from_slice(&shares.to_be_bytes());
        v.extend_from_slice(&m.to_be_bytes());
        v
    }

    fn exec_at(locate: u16, ts: u64, oref: u64, shares: u32, m: u64, price: u32) -> Vec<u8> {
        let mut v = header(b'C', locate, ts);
        v.extend_from_slice(&oref.to_be_bytes());
        v.extend_from_slice(&shares.to_be_bytes());
        v.extend_from_slice(&m.to_be_bytes());
        v.push(b'Y');
        v.extend_from_slice(&price.to_be_bytes());
        v
    }

    fn file(msgs: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for m in msgs {
            out.extend_from_slice(&(m.len() as u16).to_be_bytes());
            out.extend_from_slice(m);
        }
        out
    }

    #[test]
    fn layouts_parse_to_the_right_fields() {
        let a = add(7, 34_200_000_000_123, 42, b'S', 300, "AAPL", 2_915_200);
        assert_eq!(a.len(), expected_len(b'A').unwrap());
        let r = parse_record(&a).expect("parses");
        assert_eq!((r.kind, r.locate, r.ts_ns), (b'A', 7, 34_200_000_000_123));
        assert_eq!(r.msg, Itch::Add { oref: 42, side: Side::Sell, shares: 300, stock: sym("AAPL"), price: 2_915_200 });
        assert_eq!(symbol(&sym("AAPL")), "AAPL");
        let mut u = header(b'U', 7, 5);
        for x in [42u64.to_be_bytes().to_vec(), 43u64.to_be_bytes().to_vec(), 200u32.to_be_bytes().to_vec(), 2_915_100u32.to_be_bytes().to_vec()] {
            u.extend_from_slice(&x);
        }
        assert_eq!(u.len(), expected_len(b'U').unwrap());
        assert_eq!(parse_record(&u).unwrap().msg, Itch::Replace { old: 42, new: 43, shares: 200, price: 2_915_100 });
        let mut q = header(b'Q', 7, 6);
        for x in [1000u64.to_be_bytes().to_vec(), sym("AAPL").to_vec(), 2_915_000u32.to_be_bytes().to_vec(), 99u64.to_be_bytes().to_vec(), vec![b'C']] {
            q.extend_from_slice(&x);
        }
        assert_eq!(parse_record(&q).unwrap().msg, Itch::Cross { shares: 1000, stock: sym("AAPL"), price: 2_915_000, match_no: 99, cross_type: b'C' });
        assert!(parse_record(&a[..20]).is_none(), "too short for an add");
        let mut bad = a.clone();
        bad[19] = b'Z';
        assert!(parse_record(&bad).is_none(), "side must be B or S");
    }

    #[test]
    fn a_day_rebuilds_with_every_check_and_matches_lobsters_rebuild() {
        let mut x = header(b'X', 1, 40);
        x.extend_from_slice(&10u64.to_be_bytes());
        x.extend_from_slice(&50u32.to_be_bytes());
        let mut d = header(b'D', 1, 50);
        d.extend_from_slice(&11u64.to_be_bytes());
        let mut u = header(b'U', 1, 60);
        for v in [10u64.to_be_bytes().to_vec(), 12u64.to_be_bytes().to_vec(), 100u32.to_be_bytes().to_vec(), 1_000_100u32.to_be_bytes().to_vec()] {
            u.extend_from_slice(&v);
        }
        let mut s = header(b'S', 0, 5);
        s.push(b'Q');
        let mut unknown = header(b'D', 1, 70);
        unknown.extend_from_slice(&999u64.to_be_bytes());
        let mut odd = header(b'Z', 0, 80);
        odd.push(0);
        let msgs = vec![
            s,
            add(1, 10, 10, b'B', 200, "ABC", 1_000_000),
            add(1, 20, 11, b'S', 100, "ABC", 1_000_200),
            add(2, 25, 20, b'B', 10, "XYZ", 500_000),
            exec(1, 30, 10, 50, 1),
            x,
            d,
            u,
            exec(1, 65, 12, 150, 2),
            unknown,
            odd,
        ];
        let p = read_file(&file(&msgs)[..], &HashSet::from([1u16]), 10).expect("reads");
        let c = &p.check;
        assert_eq!((c.messages, c.unknown_types, c.malformed, c.orders_added), (11, 1, 0, 4), "an add, an add, an add, the replacement");
        assert_eq!((c.unknown_refs, c.oversize, c.duplicate_refs, c.locate_mismatches), (1, 1, 0, 0), "999 never added; 150 > the replacement's 100");
        assert_eq!((c.executed_shares, c.live_at_end, c.market_open_ns), (200, 1, Some(5)), "XYZ's order is left");
        assert!(c.length_mismatches.is_empty());
        assert_eq!(c.per_stock[&1].executions, 2);
        // Stock 1 exported in LOBSTER form, and LOBSTER's rebuild agrees after every message.
        let kinds: Vec<Kind> = p.exported[&1].iter().map(|m| m.kind).collect();
        assert_eq!(kinds, vec![Kind::Submit, Kind::Submit, Kind::ExecuteVisible, Kind::Cancel, Kind::Delete, Kind::Delete, Kind::Submit, Kind::ExecuteVisible]);
        assert_eq!((p.cross_compared, p.cross_mismatches), (7, 0));
        assert!(!p.exported.contains_key(&2));
    }

    #[test]
    fn a_wrong_length_is_reported_with_its_type() {
        let mut short = exec(1, 1, 5, 1, 1);
        short.pop();
        let p = read_file(&file(&[add(1, 1, 5, b'B', 1, "A", 1), short])[..], &HashSet::new(), 10).expect("reads");
        assert_eq!(p.check.length_mismatches[&'E'], (1, 30));
        assert_eq!(p.check.malformed, 1, "too short to parse");
    }

    #[test]
    fn every_inconsistency_is_counted_and_executions_carry_their_traded_price() {
        // An execution with a price (C) on order 1, a cent above its displayed bid (it was ranked
        // there); an execution sent under stock 1 for stock 2's order, stamped before the message
        // ahead of it; a second add reusing order 2's reference while order 2 still rests; a
        // non-displayed trade (P).
        let c_msg = exec_at(1, 30, 1, 40, 7, 1_000_100);
        let mut p_msg = header(b'P', 1, 50);
        for v in [0u64.to_be_bytes().to_vec(), vec![b'B'], 300u32.to_be_bytes().to_vec(), sym("A").to_vec(), 1_000_000u32.to_be_bytes().to_vec(), 9u64.to_be_bytes().to_vec()] {
            p_msg.extend_from_slice(&v);
        }
        let msgs = vec![
            add(1, 10, 1, b'B', 100, "A", 1_000_000),
            add(2, 20, 2, b'S', 100, "B", 2_000_000),
            c_msg,
            exec(1, 25, 2, 10, 8),
            add(1, 40, 2, b'B', 10, "A", 1_000_000),
            p_msg,
        ];
        let p = read_file(&file(&msgs)[..], &HashSet::from([1u16]), 10).expect("reads");
        let c = &p.check;
        assert_eq!((c.messages, c.malformed, c.orders_added, c.unknown_refs, c.oversize), (6, 0, 3, 0, 0));
        assert_eq!((c.duplicate_refs, c.locate_mismatches, c.out_of_order), (1, 1, 1));
        assert_eq!((c.executed_shares, c.hidden_shares, c.live_at_end), (50, 300, 2));
        assert_eq!(c.per_stock[&1].price_range, (1_000_100, 1_000_100), "stock statistics use the printed price");
        assert_eq!((c.priority.at_front, c.priority.off_display, c.priority.off_display_ahead_of_older), (1, 1, 0), "order 2 at the front; order 1 traded at a price it did not display");
        // In LOBSTER form the execution carries the price it traded at, and LOBSTER's rebuild
        // still reduces order 1 at its own level (the cross-check).
        let rows: Vec<(Kind, i64, i64, i64)> = p.exported[&1].iter().map(|m| (m.kind, m.order_id, m.size, m.price)).collect();
        assert_eq!(
            rows,
            vec![(Kind::Submit, 1, 100, 1_000_000), (Kind::ExecuteVisible, 1, 40, 1_000_100), (Kind::Submit, 2, 10, 1_000_000), (Kind::ExecuteHidden, 0, 300, 1_000_000)]
        );
        assert_eq!((p.cross_compared, p.cross_mismatches), (3, 0));
    }

    #[test]
    fn executions_are_checked_against_arrival_order() {
        // Bids 1, 2 and later 4 at $10.00. Order 1 is replaced by 3 (same price, fewer shares),
        // which then executes while 2 still rests: a jump its origin (order 1) explains. Then 4,
        // a plain add, executes while 2 still rests: a jump nothing explains. Then 2 executes at
        // the front. Last, 5 and 6 rest at $100.00 and 6 trades at $100.01, a price it was ranked
        // at but did not display: ahead of 5 by price, so not a jump; then 5 trades there too.
        let mut u = header(b'U', 1, 30);
        for v in [1u64.to_be_bytes().to_vec(), 3u64.to_be_bytes().to_vec(), 60u32.to_be_bytes().to_vec(), 1_000_000u32.to_be_bytes().to_vec()] {
            u.extend_from_slice(&v);
        }
        let msgs = vec![
            add(1, 10, 1, b'B', 100, "A", 1_000_000),
            add(1, 20, 2, b'B', 100, "A", 1_000_000),
            u,
            exec(1, 40, 3, 60, 1),
            add(1, 50, 4, b'B', 10, "A", 1_000_000),
            exec(1, 60, 4, 10, 2),
            exec(1, 70, 2, 100, 3),
            add(1, 80, 5, b'B', 100, "A", 1_000_000),
            add(1, 85, 6, b'B', 100, "A", 1_000_000),
            exec_at(1, 90, 6, 100, 4, 1_000_100),
            exec_at(1, 95, 5, 100, 5, 1_000_100),
        ];
        let p = read_file(&file(&msgs)[..], &HashSet::new(), 10).expect("reads");
        let pr = &p.check.priority;
        assert_eq!((pr.at_front, pr.jumps, pr.explained), (1, [1, 1, 0, 0], [0, 1, 0, 0]));
        assert_eq!((pr.off_display, pr.off_display_ahead_of_older), (2, 1));
        assert_eq!((pr.front_odd_lot, pr.jumper_odd_lot, pr.both_attributed, pr.examples.len()), (0, 2, 0, 2), "both jumpers below a 100-share lot");
        assert!(p.check.to_json(1).contains("\"queue_jumps\":{\"added\":1,\"replaced_same_price_fewer_shares\":1,"));
    }

    #[test]
    fn crossed_books_are_counted_by_session() {
        let mut open = header(b'S', 0, 2);
        open.push(b'Q');
        let msgs = vec![
            add(1, 1, 1, b'B', 10, "A", 1_000_100),
            add(1, 1, 2, b'S', 10, "A", 1_000_100), // locked before the open
            open,
            add(1, 3, 3, b'S', 10, "A", 1_000_000), // still crossed: same episode
        ];
        let p = read_file(&file(&msgs)[..], &HashSet::new(), 10).expect("reads");
        assert_eq!(p.check.crossed_episodes, [1, 0, 0]);
    }
}
