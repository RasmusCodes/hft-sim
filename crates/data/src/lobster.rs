//! LOBSTER files (lobsterdata.com): a message file and an order-book file per stock-day, read and
//! written, with the book rebuilt from the messages and checked against the vendor's snapshots.
//!
//! Format as implemented (docs/HISTORICAL_DATA.md §1 lists the sources and which details are
//! verified):
//! * **message file**, one row per event: time (seconds after midnight, decimal), type
//!   (1 submission, 2 partial cancellation, 3 deletion, 4 execution of a visible order,
//!   5 execution of a hidden order, 6 cross trade, 7 trading halt), order id, size, price (dollars
//!   × 10,000) and direction (1 buy order, −1 sell order). Direction is the side of the order the
//!   message is about, so an executed sell order is a buyer-initiated trade;
//! * **order-book file**, one row per message, the visible book after it: for levels 1..N, ask
//!   price, ask size, bid price, bid size. A level that does not exist has size 0 (and a dummy
//!   price, which the reader ignores).
//!
//! Messages start at the requested start time, but the book already holds orders placed before
//! it. Those orders appear only through later cancellations and executions of ids never
//! submitted in the file. [`Rebuilder`] therefore carries, per price level, a *seed* quantity for
//! such unknown orders, and [`validate`] tops it up from the vendor's snapshot whenever the
//! rebuilt book falls short. The report counts how often and by how much, so a perfect
//! reconstruction is distinguishable from a reconciled one.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::{self, Write};

use lob::{Event, EventKind, Phase, Side};

/// Message type (LOBSTER codes 1–7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// 1: a new limit order.
    Submit,
    /// 2: part of an order cancelled.
    Cancel,
    /// 3: the rest of an order deleted.
    Delete,
    /// 4: a visible order executed (partly or fully).
    ExecuteVisible,
    /// 5: a hidden order executed.
    ExecuteHidden,
    /// 6: a cross trade (auction).
    Cross,
    /// 7: a trading halt indicator.
    Halt,
}

impl Kind {
    /// LOBSTER code.
    pub fn code(self) -> u8 {
        match self {
            Kind::Submit => 1,
            Kind::Cancel => 2,
            Kind::Delete => 3,
            Kind::ExecuteVisible => 4,
            Kind::ExecuteHidden => 5,
            Kind::Cross => 6,
            Kind::Halt => 7,
        }
    }

    /// From a LOBSTER code.
    pub fn from_code(c: i64) -> Option<Kind> {
        Some(match c {
            1 => Kind::Submit,
            2 => Kind::Cancel,
            3 => Kind::Delete,
            4 => Kind::ExecuteVisible,
            5 => Kind::ExecuteHidden,
            6 => Kind::Cross,
            7 => Kind::Halt,
            _ => return None,
        })
    }
}

/// One message-file row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    /// Nanoseconds after midnight.
    pub ts_ns: i64,
    /// Type.
    pub kind: Kind,
    /// Order id.
    pub order_id: i64,
    /// Shares.
    pub size: i64,
    /// Price, dollars × 10,000.
    pub price: i64,
    /// Side of the order the message is about.
    pub side: Side,
}

/// One order-book-file row: the occupied levels, best first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// (price, size) of the ask levels, lowest price first.
    pub asks: Vec<(i64, i64)>,
    /// (price, size) of the bid levels, highest price first.
    pub bids: Vec<(i64, i64)>,
}

/// A malformed row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line number.
    pub line: usize,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

fn err<T>(line: usize, message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError { line, message: message.into() })
}

/// Seconds after midnight with up to 9 decimals, exactly, as nanoseconds.
pub fn parse_time_ns(s: &str) -> Option<i64> {
    let s = s.trim();
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let secs: i64 = int.parse().ok()?;
    if secs < 0 || frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut ns = 0i64;
    for k in 0..9 {
        ns = ns * 10 + frac.as_bytes().get(k).map_or(0, |b| i64::from(b - b'0'));
    }
    secs.checked_mul(1_000_000_000)?.checked_add(ns)
}

/// Nanoseconds after midnight as seconds with 9 decimals.
pub fn format_time_ns(ns: i64) -> String {
    format!("{}.{:09}", ns / 1_000_000_000, ns % 1_000_000_000)
}

fn int(field: &str, line: usize, name: &str) -> Result<i64, ParseError> {
    field.trim().parse::<i64>().or_else(|_| err(line, format!("{name}: `{}` is not an integer", field.trim())))
}

/// Parse one message-file row.
pub fn parse_message(row: &str, line: usize) -> Result<Message, ParseError> {
    let f: Vec<&str> = row.trim_end_matches(['\r', '\n']).split(',').collect();
    if f.len() < 6 {
        return err(line, format!("expected 6 fields, found {}", f.len()));
    }
    let ts_ns = parse_time_ns(f[0]).map_or_else(|| err(line, format!("time `{}` is not seconds after midnight", f[0].trim())), Ok)?;
    let code = int(f[1], line, "type")?;
    let kind = Kind::from_code(code).map_or_else(|| err(line, format!("unknown type {code}")), Ok)?;
    let side = match int(f[5], line, "direction")? {
        1 => Side::Buy,
        -1 => Side::Sell,
        d => return err(line, format!("direction {d} (expected 1 or -1)")),
    };
    Ok(Message { ts_ns, kind, order_id: int(f[2], line, "order id")?, size: int(f[3], line, "size")?, price: int(f[4], line, "price")?, side })
}

/// Parse one order-book-file row with `levels` levels (4 fields per level).
pub fn parse_snapshot(row: &str, levels: usize, line: usize) -> Result<Snapshot, ParseError> {
    let f: Vec<&str> = row.trim_end_matches(['\r', '\n']).split(',').collect();
    if f.len() < 4 * levels {
        return err(line, format!("expected {} fields for {levels} levels, found {}", 4 * levels, f.len()));
    }
    let mut s = Snapshot::default();
    for k in 0..levels {
        let (ap, asz, bp, bsz) = (int(f[4 * k], line, "ask price")?, int(f[4 * k + 1], line, "ask size")?, int(f[4 * k + 2], line, "bid price")?, int(f[4 * k + 3], line, "bid size")?);
        if asz > 0 {
            s.asks.push((ap, asz));
        }
        if bsz > 0 {
            s.bids.push((bp, bsz));
        }
    }
    Ok(s)
}

/// Number of levels in an order-book row (its field count / 4).
pub fn levels_in(row: &str) -> usize {
    row.trim_end_matches(['\r', '\n']).split(',').count() / 4
}

/// Parse a whole message file.
pub fn parse_messages(text: &str) -> Result<Vec<Message>, ParseError> {
    text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()).map(|(k, l)| parse_message(l, k + 1)).collect()
}

/// Parse a whole order-book file.
pub fn parse_snapshots(text: &str, levels: usize) -> Result<Vec<Snapshot>, ParseError> {
    text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()).map(|(k, l)| parse_snapshot(l, levels, k + 1)).collect()
}

/// The parts of a LOBSTER file name: `TICKER_YYYY-MM-DD_STARTms_ENDms_message_N.csv` or
/// `..._orderbook_N.csv`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileName {
    /// Ticker.
    pub symbol: String,
    /// Trading date as written.
    pub date: String,
    /// Start, milliseconds after midnight.
    pub start_ms: i64,
    /// End, milliseconds after midnight.
    pub end_ms: i64,
    /// Message (true) or order-book (false) file.
    pub message: bool,
    /// Levels.
    pub levels: usize,
}

/// LOBSTER files found under a directory: message and order-book files paired by name, and the
/// LOBSTER-named files left without a partner.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilePairs {
    /// (message file, order-book file, their name parts), ordered by symbol, date and levels.
    pub pairs: Vec<(std::path::PathBuf, std::path::PathBuf, FileName)>,
    /// LOBSTER-named files without a partner.
    pub unpaired: Vec<std::path::PathBuf>,
}

/// Search `dir` recursively for LOBSTER files and pair each message file with the order-book file
/// of the same symbol, date, start, end and levels.
pub fn find_pairs(dir: &std::path::Path) -> io::Result<FilePairs> {
    let mut files: Vec<(std::path::PathBuf, FileName)> = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let p = entry?.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Some(n) = p.file_name().and_then(|s| s.to_str()).and_then(parse_file_name) {
                files.push((p, n));
            }
        }
    }
    let key = |n: &FileName| (n.symbol.clone(), n.date.clone(), n.start_ms, n.end_ms, n.levels);
    let books: HashMap<_, std::path::PathBuf> = files.iter().filter(|(_, n)| !n.message).map(|(p, n)| (key(n), p.clone())).collect();
    let messages: HashMap<_, std::path::PathBuf> = files.iter().filter(|(_, n)| n.message).map(|(p, n)| (key(n), p.clone())).collect();
    let mut out = FilePairs::default();
    for (p, n) in &files {
        let partner = if n.message { books.get(&key(n)) } else { messages.get(&key(n)) };
        match (partner, n.message) {
            (Some(b), true) => out.pairs.push((p.clone(), b.clone(), n.clone())),
            (Some(_), false) => {}
            (None, _) => out.unpaired.push(p.clone()),
        }
    }
    out.pairs.sort_by(|a, b| (&a.2.symbol, &a.2.date, a.2.start_ms, a.2.levels).cmp(&(&b.2.symbol, &b.2.date, b.2.start_ms, b.2.levels)));
    out.unpaired.sort();
    Ok(out)
}

/// Validate every pair under `dir` ([`find_pairs`], [`validate_files`]). One bad pair does not
/// stop the others: its error is reported in its place. Returns a JSON report with a summary.
pub fn validate_dir(dir: &std::path::Path) -> Result<String, String> {
    use crate::provenance::json_string;
    let found = find_pairs(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let (mut rows, mut matched, mut unexplained, mut failed) = (0usize, 0usize, 0usize, 0usize);
    let mut items = Vec::new();
    for (m, b, n) in &found.pairs {
        let head = format!("\"symbol\":{},\"date\":{},\"levels\":{}", json_string(&n.symbol), json_string(&n.date), n.levels);
        match validate_files(m, b, Some(n.levels)) {
            Ok((v, p)) => {
                rows += v.rows;
                matched += v.matched;
                unexplained += v.unexplained_rows;
                items.push(format!("{{{head},\"provenance\":{},\"validation\":{}}}", p.to_json(), v.to_json()));
            }
            Err(e) => {
                failed += 1;
                items.push(format!("{{{head},\"message_file\":{},\"error\":{}}}", json_string(&m.display().to_string()), json_string(&e)));
            }
        }
    }
    let unpaired: Vec<String> = found.unpaired.iter().map(|p| json_string(&p.display().to_string())).collect();
    let share = if rows > 0 { matched as f64 / rows as f64 } else { 0.0 };
    Ok(format!(
        "{{\"directory\":{},\"summary\":{{\"pairs\":{},\"failed\":{},\"rows\":{rows},\"matched\":{matched},\"matched_share\":{share:.6},\"unexplained_rows\":{unexplained},\"unpaired_files\":{}}},\"pairs\":[{}],\"unpaired\":[{}]}}",
        json_string(&dir.display().to_string()),
        found.pairs.len(),
        failed,
        unpaired.len(),
        items.join(","),
        unpaired.join(",")
    ))
}

/// Split a LOBSTER file name into its parts (`None` if it does not follow the pattern).
pub fn parse_file_name(name: &str) -> Option<FileName> {
    let stem = name.rsplit(['/', '\\']).next()?.strip_suffix(".csv")?;
    let parts: Vec<&str> = stem.rsplitn(6, '_').collect();
    // Reversed: levels, kind, end, start, date, symbol (the symbol may itself contain '_').
    if parts.len() != 6 {
        return None;
    }
    let message = match parts[1] {
        "message" => true,
        "orderbook" => false,
        _ => return None,
    };
    Some(FileName { symbol: parts[5].to_string(), date: parts[4].to_string(), start_ms: parts[3].parse().ok()?, end_ms: parts[2].parse().ok()?, message, levels: parts[0].parse().ok()? })
}

// ---------------------------------------------------------------------------------------------
// Writing: an engine event log as LOBSTER files

/// How to write an engine log as LOBSTER files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportOptions {
    /// Book levels in the order-book file.
    pub levels: usize,
    /// Time of day of simulation time 0, nanoseconds after midnight (34,200 s = 9:30).
    pub start_ns: i64,
    /// LOBSTER price units per engine price unit (100 when the engine counts cents).
    pub price_scale: i64,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions { levels: 10, start_ns: 34_200 * 1_000_000_000, price_scale: 100 }
    }
}

/// What an export wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportStats {
    /// Rows written to each file.
    pub rows: usize,
    /// Rows by message type (index = code − 1).
    pub by_kind: [usize; 7],
}

/// Dummy price of an empty ask level in the order-book file.
pub const EMPTY_ASK_PRICE: i64 = 9_999_999_999;
/// Dummy price of an empty bid level in the order-book file.
pub const EMPTY_BID_PRICE: i64 = -9_999_999_999;

fn direction(s: Side) -> i64 {
    match s {
        Side::Buy => 1,
        Side::Sell => -1,
    }
}

fn idx(s: Side) -> usize {
    match s {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

/// Write the public part of an engine event log as a LOBSTER message file and order-book file.
///
/// `BookAdd` becomes a submission, a `BookUpdate` that shrinks an order a partial cancellation,
/// `BookRemove` a deletion of the remaining size, and a continuous-trading `Trade` an execution
/// of the resting order. Orders that never rest (filled or cancelled on arrival) do not appear,
/// as in exchange order-level feeds. A halt and the resumption after it become type-7 rows with
/// price −1 and 1. Auction trades and resting market orders have no LOBSTER equivalent here and
/// are an error.
pub fn write_lobster<M: Write, B: Write>(events: &[Event], opts: &ExportOptions, messages: &mut M, book: &mut B) -> io::Result<ExportStats> {
    let mut orders: HashMap<u64, (Side, i64, i64)> = HashMap::new();
    let mut levels: [BTreeMap<i64, i64>; 2] = [BTreeMap::new(), BTreeMap::new()];
    let mut stats = ExportStats::default();
    let mut halted = false;
    let unsupported = |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("no LOBSTER equivalent: {what}"));
    for e in events {
        let row: (Kind, i64, i64, i64, Side) = match e.kind {
            EventKind::BookAdd { order_id, side, price, qty } => {
                let price = price.ok_or_else(|| unsupported("a resting market order"))?;
                orders.insert(order_id, (side, price, qty));
                *levels[idx(side)].entry(price).or_insert(0) += qty;
                (Kind::Submit, order_id as i64, qty, price, side)
            }
            EventKind::BookUpdate { order_id, qty } => {
                let o = orders.get_mut(&order_id).ok_or_else(|| unsupported("an update of an unknown order"))?;
                let cut = o.2 - qty;
                if cut <= 0 {
                    return Err(unsupported("an update that does not reduce the order"));
                }
                o.2 = qty;
                let (side, price) = (o.0, o.1);
                change(&mut levels, side, price, -cut);
                (Kind::Cancel, order_id as i64, cut, price, side)
            }
            EventKind::BookRemove { order_id } => {
                let (side, price, qty) = orders.remove(&order_id).ok_or_else(|| unsupported("a removal of an unknown order"))?;
                change(&mut levels, side, price, -qty);
                (Kind::Delete, order_id as i64, qty, price, side)
            }
            EventKind::Trade { price, qty, aggressor, buy_order_id, sell_order_id, .. } => {
                let resting = match aggressor {
                    Some(Side::Buy) => sell_order_id,
                    Some(Side::Sell) => buy_order_id,
                    None => return Err(unsupported("an auction trade")),
                };
                let o = orders.get_mut(&resting).ok_or_else(|| unsupported("a trade against an unknown order"))?;
                o.2 -= qty;
                let (side, order_price, left) = *o;
                if left == 0 {
                    orders.remove(&resting);
                }
                change(&mut levels, side, order_price, -qty);
                (Kind::ExecuteVisible, resting as i64, qty, price, side)
            }
            EventKind::PhaseChanged { phase: Phase::Halted } => {
                halted = true;
                (Kind::Halt, 0, 0, -1, Side::Sell)
            }
            EventKind::PhaseChanged { phase: Phase::Continuous } if halted => {
                halted = false;
                (Kind::Halt, 0, 0, 1, Side::Sell)
            }
            EventKind::AuctionUncross { .. } => return Err(unsupported("an auction uncross")),
            _ => continue,
        };
        let (kind, id, size, price, side) = row;
        let price = if kind == Kind::Halt { price } else { price * opts.price_scale };
        writeln!(messages, "{},{},{},{},{},{}", format_time_ns(opts.start_ns + e.ts), kind.code(), id, size, price, direction(side))?;
        let mut line = String::with_capacity(16 * 4 * opts.levels);
        let mut asks = levels[1].iter();
        let mut bids = levels[0].iter().rev();
        for k in 0..opts.levels {
            let (ap, asz) = asks.next().map_or((EMPTY_ASK_PRICE, 0), |(&p, &q)| (p * opts.price_scale, q));
            let (bp, bsz) = bids.next().map_or((EMPTY_BID_PRICE, 0), |(&p, &q)| (p * opts.price_scale, q));
            if k > 0 {
                line.push(',');
            }
            line.push_str(&format!("{ap},{asz},{bp},{bsz}"));
        }
        writeln!(book, "{line}")?;
        stats.rows += 1;
        stats.by_kind[usize::from(kind.code() - 1)] += 1;
    }
    Ok(stats)
}

fn change(levels: &mut [BTreeMap<i64, i64>; 2], side: Side, price: i64, delta: i64) {
    let m = &mut levels[idx(side)];
    let q = m.entry(price).or_insert(0);
    *q += delta;
    if *q <= 0 {
        m.remove(&price);
    }
}

// ---------------------------------------------------------------------------------------------
// Reconstruction

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Level {
    /// Quantity of orders submitted in the file.
    known: i64,
    /// Quantity of orders placed before the file starts (unknown individually).
    seed: i64,
}

impl Level {
    fn total(self) -> i64 {
        self.known + self.seed
    }
}

/// What the rebuild met along the way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RebuildStats {
    /// Messages applied.
    pub messages: usize,
    /// Messages by type (index = code − 1).
    pub by_kind: [usize; 7],
    /// Cancellations, deletions and executions of order ids never submitted in the file.
    pub unknown_order_events: usize,
    /// Their shares.
    pub unknown_order_volume: i64,
    /// Submissions of an id already live.
    pub duplicate_submissions: usize,
    /// Deletions whose size differs from the order's remaining size, and cancellations or
    /// executions larger than it.
    pub size_mismatches: usize,
    /// Halt indicator rows by price −1 (halt), 0 (quote-only), 1 (resume); others counted as −1.
    pub halts: [usize; 3],
}

/// The visible book rebuilt from LOBSTER messages.
#[derive(Clone, Debug, Default)]
pub struct Rebuilder {
    orders: HashMap<i64, (Side, i64, i64)>,
    levels: [BTreeMap<i64, Level>; 2],
    /// Counters.
    pub stats: RebuildStats,
}

/// Seed changes made to agree with one snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reconciliation {
    /// Levels changed.
    pub levels: usize,
    /// Shares added to seeds.
    pub added: i64,
    /// Shares removed from seeds.
    pub removed: i64,
    /// Levels the snapshot contradicts by more than the seeds can absorb (known orders exceed the
    /// snapshot's size, or a known level is missing from it).
    pub unexplained: usize,
}

impl Rebuilder {
    /// Empty book.
    pub fn new() -> Self {
        Self::default()
    }

    fn level(&mut self, side: Side, price: i64) -> &mut Level {
        self.levels[idx(side)].entry(price).or_default()
    }

    fn tidy(&mut self, side: Side, price: i64) {
        if self.levels[idx(side)].get(&price).is_some_and(|l| l.total() <= 0 && l.known <= 0) {
            self.levels[idx(side)].remove(&price);
        }
    }

    /// Remove `size` from an order (all of it if `whole`), or from its level's seed if the order
    /// is unknown.
    fn reduce(&mut self, m: &Message, whole: bool) {
        match self.orders.get_mut(&m.order_id) {
            Some(o) => {
                let mismatch = if whole { o.2 != m.size } else { m.size > o.2 };
                let take = if whole { o.2 } else { m.size.min(o.2) };
                o.2 -= take;
                let (side, price, left) = *o;
                if left <= 0 {
                    self.orders.remove(&m.order_id);
                }
                self.stats.size_mismatches += usize::from(mismatch);
                self.level(side, price).known -= take;
                self.tidy(side, price);
            }
            None => {
                self.stats.unknown_order_events += 1;
                self.stats.unknown_order_volume += m.size;
                if let Some(l) = self.levels[idx(m.side)].get_mut(&m.price) {
                    l.seed -= m.size.min(l.seed);
                }
                self.tidy(m.side, m.price);
            }
        }
    }

    /// Apply one message.
    pub fn apply(&mut self, m: &Message) {
        self.stats.messages += 1;
        self.stats.by_kind[usize::from(m.kind.code() - 1)] += 1;
        match m.kind {
            Kind::Submit => {
                if let Some((side, price, qty)) = self.orders.remove(&m.order_id) {
                    self.stats.duplicate_submissions += 1;
                    self.level(side, price).known -= qty;
                    self.tidy(side, price);
                }
                self.orders.insert(m.order_id, (m.side, m.price, m.size));
                self.level(m.side, m.price).known += m.size;
            }
            Kind::Cancel | Kind::ExecuteVisible => self.reduce(m, false),
            Kind::Delete => self.reduce(m, true),
            Kind::ExecuteHidden | Kind::Cross => {}
            Kind::Halt => {
                let k = match m.price {
                    0 => 1,
                    1 => 2,
                    _ => 0,
                };
                self.stats.halts[k] += 1;
            }
        }
    }

    /// Known orders resting at one price: (order id, shares), in no particular order.
    pub fn orders_at(&self, side: Side, price: i64) -> Vec<(i64, i64)> {
        self.orders.iter().filter(|(_, o)| o.0 == side && o.1 == price).map(|(&id, o)| (id, o.2)).collect()
    }

    /// Shares of unknown (pre-existing) orders at one price.
    pub fn seed_at(&self, side: Side, price: i64) -> i64 {
        self.levels[idx(side)].get(&price).map_or(0, |l| l.seed)
    }

    /// Whether an order id is live and was submitted in the file.
    pub fn is_known(&self, order_id: i64) -> bool {
        self.orders.contains_key(&order_id)
    }

    /// The price a live order submitted in the file rests at.
    pub fn price_of(&self, order_id: i64) -> Option<i64> {
        self.orders.get(&order_id).map(|o| o.1)
    }

    /// Best bid and best ask prices.
    pub fn best(&self) -> (Option<i64>, Option<i64>) {
        let t = self.top(1);
        (t.bids.first().map(|l| l.0), t.asks.first().map(|l| l.0))
    }

    /// The best `levels` levels per side.
    pub fn top(&self, levels: usize) -> Snapshot {
        let occupied = |(p, l): (&i64, &Level)| (l.total() > 0).then_some((*p, l.total()));
        Snapshot {
            asks: self.levels[1].iter().filter_map(occupied).take(levels).collect(),
            bids: self.levels[0].iter().rev().filter_map(occupied).take(levels).collect(),
        }
    }

    /// Change seeds so the best `levels` levels agree with a vendor snapshot.
    pub fn reconcile(&mut self, snap: &Snapshot, levels: usize) -> Reconciliation {
        let mut r = Reconciliation::default();
        for (side, listed) in [(Side::Sell, &snap.asks), (Side::Buy, &snap.bids)] {
            let book = &mut self.levels[idx(side)];
            // Prices the snapshot is authoritative for: all of them if the side has fewer than
            // `levels` levels, otherwise those at least as good as its worst listed level.
            let limit = (listed.len() >= levels).then(|| listed.last().map(|l| l.0)).flatten();
            let within = |p: i64| match (limit, side) {
                (None, _) => true,
                (Some(w), Side::Sell) => p <= w,
                (Some(w), Side::Buy) => p >= w,
            };
            let wanted: HashMap<i64, i64> = listed.iter().copied().collect();
            let prices: Vec<i64> = book.keys().copied().filter(|&p| within(p) && !wanted.contains_key(&p)).collect();
            for p in prices {
                let l = book.get_mut(&p).expect("listed");
                if l.seed > 0 {
                    r.removed += l.seed;
                    l.seed = 0;
                    r.levels += 1;
                }
                if l.known > 0 {
                    r.unexplained += 1;
                } else {
                    book.remove(&p);
                }
            }
            for (&p, &size) in &wanted {
                let l = book.entry(p).or_default();
                if l.total() == size {
                    continue;
                }
                r.levels += 1;
                let seed = size - l.known;
                if seed < 0 {
                    r.unexplained += 1;
                    r.removed += l.seed;
                    l.seed = 0;
                } else if seed > l.seed {
                    r.added += seed - l.seed;
                    l.seed = seed;
                } else {
                    r.removed += l.seed - seed;
                    l.seed = seed;
                }
            }
        }
        r
    }
}

/// Result of rebuilding a stock-day and checking it row by row against the order-book file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Validation {
    /// Book levels compared.
    pub levels: usize,
    /// Rows (messages = snapshots).
    pub rows: usize,
    /// Rows whose rebuilt book matched the snapshot before any reconciliation.
    pub matched: usize,
    /// First row (1-based) that did not match, if any.
    pub first_mismatch: Option<usize>,
    /// Rows that needed seed changes.
    pub reconciled_rows: usize,
    /// Shares added to seeds in total (the pre-existing book revealed by the snapshots).
    pub seed_added: i64,
    /// Shares removed from seeds in total.
    pub seed_removed: i64,
    /// Rows with contradictions the seeds could not absorb.
    pub unexplained_rows: usize,
    /// What the rebuild met.
    pub stats: RebuildStats,
}

/// Why a validation could not finish.
#[derive(Debug)]
pub enum ValidateError {
    /// A malformed row in the message file.
    Messages(ParseError),
    /// A malformed row in the order-book file.
    Book(ParseError),
    /// The files have different numbers of rows.
    RowCount {
        /// Message rows.
        messages: usize,
        /// Order-book rows.
        books: usize,
    },
}

impl fmt::Display for ValidateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidateError::Messages(e) => write!(f, "message file, {e}"),
            ValidateError::Book(e) => write!(f, "order-book file, {e}"),
            ValidateError::RowCount { messages, books } => write!(f, "{messages} message rows but {books} order-book rows"),
        }
    }
}

impl std::error::Error for ValidateError {}

/// Rebuild the book message by message and compare it with each snapshot. A row that disagrees
/// is reconciled (seeds changed to match) before the next message, so later rows are checked
/// against a book that agrees with the vendor up to then.
pub fn validate<M, B>(messages: M, snapshots: B, levels: usize) -> Result<Validation, ValidateError>
where
    M: IntoIterator<Item = Result<Message, ParseError>>,
    B: IntoIterator<Item = Result<Snapshot, ParseError>>,
{
    let mut book = Rebuilder::new();
    let mut v = Validation { levels, ..Validation::default() };
    let mut snaps = snapshots.into_iter();
    for m in messages {
        let m = m.map_err(ValidateError::Messages)?;
        let s = match snaps.next() {
            Some(s) => s.map_err(ValidateError::Book)?,
            None => return Err(ValidateError::RowCount { messages: v.rows + 1, books: v.rows }),
        };
        book.apply(&m);
        v.rows += 1;
        if book.top(levels) == s {
            v.matched += 1;
            continue;
        }
        v.first_mismatch.get_or_insert(v.rows);
        let r = book.reconcile(&s, levels);
        v.reconciled_rows += usize::from(r.levels > 0);
        v.seed_added += r.added;
        v.seed_removed += r.removed;
        v.unexplained_rows += usize::from(r.unexplained > 0);
    }
    let extra = snaps.count();
    if extra > 0 {
        return Err(ValidateError::RowCount { messages: v.rows, books: v.rows + extra });
    }
    v.stats = book.stats;
    Ok(v)
}

/// [`validate`] on two files' text.
pub fn validate_text(messages: &str, books: &str, levels: usize) -> Result<Validation, ValidateError> {
    let msgs = messages.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()).map(|(k, l)| parse_message(l, k + 1));
    let snaps = books.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()).map(|(k, l)| parse_snapshot(l, levels, k + 1));
    validate(msgs, snaps, levels)
}

impl Validation {
    /// As a JSON object.
    pub fn to_json(&self) -> String {
        let share = if self.rows > 0 { self.matched as f64 / self.rows as f64 } else { 0.0 };
        let list = |v: &[usize]| v.iter().map(ToString::to_string).collect::<Vec<_>>().join(",");
        let s = &self.stats;
        format!(
            "{{\"levels\":{},\"rows\":{},\"matched\":{},\"matched_share\":{:.6},\"first_mismatch\":{},\"reconciled_rows\":{},\"seed_added\":{},\"seed_removed\":{},\
             \"unexplained_rows\":{},\"messages_by_type\":[{}],\"unknown_order_events\":{},\"unknown_order_volume\":{},\"duplicate_submissions\":{},\
             \"size_mismatches\":{},\"halts_by_price\":{{\"-1\":{},\"0\":{},\"1\":{}}}}}",
            self.levels,
            self.rows,
            self.matched,
            share,
            self.first_mismatch.map_or("null".to_string(), |r| r.to_string()),
            self.reconciled_rows,
            self.seed_added,
            self.seed_removed,
            self.unexplained_rows,
            list(&s.by_kind),
            s.unknown_order_events,
            s.unknown_order_volume,
            s.duplicate_submissions,
            s.size_mismatches,
            s.halts[0],
            s.halts[1],
            s.halts[2]
        )
    }
}

/// Validate a stock-day from its two files, reading them line by line. `levels` defaults to the
/// order-book file's name, or failing that its first row. Returns the validation and the data's
/// provenance.
pub fn validate_files(messages: &std::path::Path, book: &std::path::Path, levels: Option<usize>) -> Result<(Validation, crate::provenance::Provenance), String> {
    use std::io::BufRead;

    use crate::provenance::{DataLevel, Provenance, file_info};
    let open = |p: &std::path::Path| std::fs::File::open(p).map(io::BufReader::new).map_err(|e| format!("cannot open {}: {e}", p.display()));
    let file_name = |p: &std::path::Path| p.file_name().and_then(|n| n.to_str()).and_then(parse_file_name);
    let (msg_name, book_name) = (file_name(messages), file_name(book));
    let levels = match levels.or(book_name.as_ref().map(|n| n.levels)) {
        Some(l) => l,
        None => {
            let mut first = String::new();
            open(book)?.read_line(&mut first).map_err(|e| format!("cannot read {}: {e}", book.display()))?;
            levels_in(&first)
        }
    };
    if levels == 0 {
        return Err(format!("cannot tell how many levels {} has", book.display()));
    }
    let lines = |p: &std::path::Path| -> Result<_, String> {
        Ok(open(p)?.lines().enumerate().filter(|(_, l)| l.as_ref().map_or(true, |s| !s.trim().is_empty())).map(|(k, l)| (k + 1, l)))
    };
    let msgs = lines(messages)?.map(|(n, l)| l.map_err(|e| ParseError { line: n, message: e.to_string() }).and_then(|l| parse_message(&l, n)));
    let snaps = lines(book)?.map(|(n, l)| l.map_err(|e| ParseError { line: n, message: e.to_string() }).and_then(|l| parse_snapshot(&l, levels, n)));
    let v = validate(msgs, snaps, levels).map_err(|e| e.to_string())?;
    let named = msg_name.as_ref().or(book_name.as_ref());
    let files = [messages, book].iter().map(|p| file_info(p)).collect::<io::Result<Vec<_>>>().map_err(|e| e.to_string())?;
    let provenance = Provenance {
        source: match named {
            Some(n) if n.symbol == "HFTSIM" => "hft-sim simulation written in LOBSTER format (hftsim lobster-export)".into(),
            Some(_) => "LOBSTER format, files named in LOBSTER's pattern (the vendor itself is not verified from the files)".into(),
            None => "LOBSTER-format files (names not in LOBSTER's pattern)".into(),
        },
        symbol: named.map_or("unknown".into(), |n| n.symbol.clone()),
        date: named.map_or("unknown".into(), |n| n.date.clone()),
        data_level: DataLevel::Mbo,
        levels,
        files,
        notes: vec![
            "Messages start at the file's start time; orders placed earlier appear only through later events and are reconciled from the snapshots (seed_added).".into(),
            "Visible book only: hidden orders' resting state and participant identities are not in the data.".into(),
            "Prices in dollars x 10,000.".into(),
        ],
    };
    Ok((v, provenance))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(t: &str, kind: u8, id: i64, size: i64, price: i64, dir: i64) -> String {
        format!("{t},{kind},{id},{size},{price},{dir}")
    }

    #[test]
    fn times_parse_exactly_and_round_trip() {
        assert_eq!(parse_time_ns("34200.004241176"), Some(34_200_004_241_176));
        assert_eq!(parse_time_ns("34200.5"), Some(34_200_500_000_000));
        assert_eq!(parse_time_ns("34200"), Some(34_200_000_000_000));
        assert_eq!(parse_time_ns("34200.0000000001"), None, "more than nanosecond precision is refused");
        assert_eq!(parse_time_ns("-1.0"), None);
        assert_eq!(format_time_ns(34_200_004_241_176), "34200.004241176");
        assert_eq!(parse_time_ns(&format_time_ns(57_599_999_999_999)), Some(57_599_999_999_999));
    }

    #[test]
    fn rows_parse_and_mistakes_are_reported_with_their_line() {
        let msg = parse_message(&m("34200.01", 4, 16113575, 18, 5853300, -1), 1).expect("valid");
        assert_eq!((msg.kind, msg.order_id, msg.size, msg.price, msg.side), (Kind::ExecuteVisible, 16_113_575, 18, 5_853_300, Side::Sell));
        assert_eq!(parse_message("34200.01,8,1,1,1,1", 7).unwrap_err().line, 7);
        assert!(parse_message("34200.01,1,1,1,1,0", 1).is_err(), "direction must be 1 or -1");
        assert!(parse_message("34200.01,1,1,1", 1).is_err());
        let s = parse_snapshot("5859400,200,5853300,18,5859800,200,5853000,150,9999999999,0,-9999999999,0", 3, 1).expect("valid");
        assert_eq!(s.asks, vec![(5_859_400, 200), (5_859_800, 200)]);
        assert_eq!(s.bids, vec![(5_853_300, 18), (5_853_000, 150)]);
        assert_eq!(levels_in("1,2,3,4,5,6,7,8"), 2);
        let name = parse_file_name("data/AAPL_2012-06-21_34200000_57600000_message_10.csv").expect("LOBSTER name");
        assert_eq!((name.symbol.as_str(), name.date.as_str(), name.start_ms, name.end_ms, name.message, name.levels), ("AAPL", "2012-06-21", 34_200_000, 57_600_000, true, 10));
        assert!(parse_file_name("notes.csv").is_none());
    }

    #[test]
    fn the_writer_maps_every_book_event_and_reads_back() {
        // Real engine events: an ask of 10 rests; a modify cuts it to 6 in place (BookUpdate,
        // priority kept); an IOC buys 2 of it; the owner cancels the rest. A bid rests below.
        use lob::{Command, InstrumentSpec, MatchingEngine, NewOrder, TimeInForce};
        let mut eng = MatchingEngine::new(InstrumentSpec::example(), Phase::Continuous).expect("engine");
        let mut ev = Vec::new();
        let commands = [
            (1, Command::New(NewOrder::limit(1, Side::Sell, 10_010, 10))),
            (2, Command::New(NewOrder::limit(2, Side::Buy, 9_990, 5))),
            (3, Command::Modify { owner: 1, order_id: 1, price: 10_010, qty: 6 }),
            (4, Command::New(NewOrder::limit(3, Side::Buy, 10_010, 2).with_tif(TimeInForce::Ioc))),
            (5, Command::Cancel { owner: 1, order_id: 1 }),
        ];
        for (t, c) in &commands {
            eng.process(*t, c, &mut ev).expect("valid command");
        }
        let (mut m, mut b) = (Vec::new(), Vec::new());
        let stats = write_lobster(&ev, &ExportOptions { levels: 2, start_ns: 0, price_scale: 100 }, &mut m, &mut b).expect("written");
        let (m, b) = (String::from_utf8(m).expect("utf-8"), String::from_utf8(b).expect("utf-8"));
        let rows: Vec<_> = parse_messages(&m).expect("messages").iter().map(|x| (x.ts_ns, x.kind, x.order_id, x.size, x.price, x.side)).collect();
        assert_eq!(
            rows,
            [
                (1, Kind::Submit, 1, 10, 1_001_000, Side::Sell),
                (2, Kind::Submit, 2, 5, 999_000, Side::Buy),
                (3, Kind::Cancel, 1, 4, 1_001_000, Side::Sell),
                (4, Kind::ExecuteVisible, 1, 2, 1_001_000, Side::Sell),
                (5, Kind::Delete, 1, 4, 1_001_000, Side::Sell),
            ],
            "the cut is the size removed (10 → 6), the execution names the resting order, the deletion its remaining 4"
        );
        assert_eq!((stats.rows, stats.by_kind), (5, [2, 1, 1, 1, 0, 0, 0]));
        assert_eq!(parse_snapshots(&b, 2).expect("book")[2], Snapshot { asks: vec![(1_001_000, 6)], bids: vec![(999_000, 5)] });
        let v = validate_text(&m, &b, 2).expect("valid files");
        assert_eq!((v.matched, v.stats.size_mismatches), (5, 0));
    }

    #[test]
    fn a_hand_built_day_rebuilds_with_its_unknown_opening_book() {
        // Before the file starts, the book holds 100 shares bid at 99.00 and 50 offered at 99.10
        // (unknown orders 1 and 2). In the file: order 10 bids 30 at 99.00; unknown bid 1 is
        // executed for 40; order 10 is partly cancelled (10); unknown offer 2 is deleted; order 11
        // offers 70 at 99.20.
        let messages = [
            m("34200.000000001", 1, 10, 30, 990_000, 1),
            m("34200.000000002", 4, 1, 40, 990_000, 1),
            m("34200.000000003", 2, 10, 10, 990_000, 1),
            m("34200.000000004", 3, 2, 50, 991_000, -1),
            m("34200.000000005", 1, 11, 70, 992_000, -1),
        ]
        .join("\n");
        let books = [
            "991000,50,990000,130",
            "991000,50,990000,90",
            "991000,50,990000,80",
            "9999999999,0,990000,80",
            "992000,70,990000,80",
        ]
        .join("\n");
        let v = validate_text(&messages, &books, 1).expect("valid files");
        // Row 1 shows the unknown opening book (100 bid + 50 offered): one reconciliation adds
        // 150 shares of seeds; every later row then matches without help.
        assert_eq!((v.rows, v.matched, v.first_mismatch, v.reconciled_rows), (5, 4, Some(1), 1));
        assert_eq!((v.seed_added, v.seed_removed, v.unexplained_rows), (150, 0, 0));
        assert_eq!((v.stats.unknown_order_events, v.stats.unknown_order_volume), (2, 90));
        assert_eq!(v.stats.size_mismatches, 0);
    }

    #[test]
    fn contradictions_the_seeds_cannot_absorb_are_reported() {
        // A known order of 30 at 99.00, but the vendor shows only 20 there.
        let v = validate_text(&m("1.0", 1, 10, 30, 990_000, 1), "9999999999,0,990000,20", 1).expect("valid");
        assert_eq!((v.matched, v.unexplained_rows), (0, 1));
        assert!(matches!(validate_text(&m("1.0", 1, 10, 30, 990_000, 1), "", 1), Err(ValidateError::RowCount { messages: 1, books: 0 })));
    }

    #[test]
    fn deeper_levels_count_only_inside_the_snapshot_window() {
        // Three bid levels known; the vendor file has two levels, so the third is not checked.
        let messages = [m("1.0", 1, 1, 10, 100, 1), m("1.1", 1, 2, 10, 99, 1), m("1.2", 1, 3, 10, 98, 1)].join("\n");
        let books = ["9999999999,0,100,10,9999999999,0,-9999999999,0", "9999999999,0,100,10,9999999999,0,99,10", "9999999999,0,100,10,9999999999,0,99,10"].join("\n");
        let v = validate_text(&messages, &books, 2).expect("valid");
        assert_eq!((v.matched, v.unexplained_rows), (3, 0));
    }
}
