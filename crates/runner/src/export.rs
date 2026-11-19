//! Columnar export of one run (the engine's event log, the top of book, the participants) in the
//! layout of the Arrow columnar format. The Python bindings pass these buffers to pyarrow as they
//! are:
//! * integers are little-endian `i64`;
//! * categorical columns are `i8` codes into a fixed list of names (Arrow dictionary arrays);
//! * booleans and validity bitmaps are bit-packed, least-significant bit first, with 1 meaning
//!   set or valid.
//!
//! The event table is "wide": one row per engine event, with a column for every field any event
//! kind has. Columns that do not apply to a row's kind are null. `side` is filled in for every
//! event about one order, from the order's acceptance, because a side never changes.

use analysis::BookPath;
use analysis::metrics::OrderIndex;
use lob::{CancelReason, Event, EventKind, Liquidity, Phase, RejectReason, RequestKind, Side, TimeInForce};

/// Event kinds, in code order.
pub const KINDS: &[&str] = &[
    "accepted",
    "rejected",
    "fill",
    "cancelled",
    "replaced",
    "request_rejected",
    "book_add",
    "book_update",
    "book_remove",
    "trade",
    "auction_uncross",
    "phase_changed",
];
/// Order sides.
pub const SIDES: &[&str] = &["buy", "sell"];
/// Liquidity roles of fills.
pub const LIQUIDITY: &[&str] = &["maker", "taker", "auction"];
/// Times in force.
pub const TIFS: &[&str] = &["gtc", "ioc", "fok"];
/// Reject reasons (rejected orders and refused requests), then cancel reasons.
pub const REASONS: &[&str] = &[
    "market_closed",
    "trading_halted",
    "invalid_qty",
    "lot_size",
    "max_qty",
    "tick_size",
    "price_bounds",
    "tif_not_allowed_in_phase",
    "invalid_post_only",
    "post_only_would_cross",
    "unknown_order",
    "not_owner",
    "market_order_not_modifiable",
    "requested",
    "mass_cancel",
    "ioc_remainder",
    "fok_unfilled",
    "self_trade",
    "modified_below_filled",
    "auction_market_remainder",
];
/// Request kinds of refused requests.
pub const REQUESTS: &[&str] = &["cancel", "modify"];
/// Trading phases.
pub const PHASES: &[&str] = &["continuous", "auction", "halted", "closed"];

/// One column's values.
#[derive(Clone, Debug, PartialEq)]
pub enum Values {
    /// 64-bit signed integers.
    I64(Vec<i64>),
    /// Codes into a list of names.
    Codes(Vec<i8>, &'static [&'static str]),
    /// Booleans, bit-packed.
    Bool(Vec<u8>),
}

/// One column.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    /// Name.
    pub name: &'static str,
    /// Rows.
    pub len: usize,
    /// Values (null rows hold 0 or false).
    pub values: Values,
    /// Validity bitmap, `None` when no row is null.
    pub validity: Option<Vec<u8>>,
}

impl Column {
    /// The values as little-endian bytes, as Arrow lays them out.
    pub fn value_bytes(&self) -> Vec<u8> {
        match &self.values {
            Values::I64(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            Values::Codes(v, _) => v.iter().map(|&x| x as u8).collect(),
            Values::Bool(bits) => bits.clone(),
        }
    }

    /// Whether row `k` is valid (not null).
    pub fn is_valid(&self, k: usize) -> bool {
        self.validity.as_ref().is_none_or(|b| b[k / 8] >> (k % 8) & 1 == 1)
    }
}

/// Every table of one run.
#[derive(Clone, Debug, PartialEq)]
pub struct Tables {
    /// The engine's event log, one row per event.
    pub events: Vec<Column>,
    /// The top of book after every public-event time.
    pub book: Vec<Column>,
    /// Participants: (owner id, role, label), by id.
    pub owners: Vec<(u32, &'static str, String)>,
}

#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    len: usize,
    ones: usize,
}

impl Bits {
    fn push(&mut self, bit: bool) {
        if self.len % 8 == 0 {
            self.bytes.push(0);
        }
        if bit {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 1 << (self.len % 8);
            self.ones += 1;
        }
        self.len += 1;
    }

    fn validity(self) -> Option<Vec<u8>> {
        (self.ones < self.len).then_some(self.bytes)
    }
}

struct Ints {
    name: &'static str,
    values: Vec<i64>,
    valid: Bits,
}

impl Ints {
    fn new(name: &'static str) -> Self {
        Ints { name, values: Vec::new(), valid: Bits::default() }
    }

    fn push(&mut self, v: Option<i64>) {
        self.values.push(v.unwrap_or(0));
        self.valid.push(v.is_some());
    }

    fn finish(self) -> Column {
        Column { name: self.name, len: self.values.len(), values: Values::I64(self.values), validity: self.valid.validity() }
    }
}

struct Codes {
    name: &'static str,
    names: &'static [&'static str],
    values: Vec<i8>,
    valid: Bits,
}

impl Codes {
    fn new(name: &'static str, names: &'static [&'static str]) -> Self {
        Codes { name, names, values: Vec::new(), valid: Bits::default() }
    }

    fn push(&mut self, v: Option<usize>) {
        debug_assert!(v.is_none_or(|c| c < self.names.len()));
        self.values.push(v.map_or(0, |c| c as i8));
        self.valid.push(v.is_some());
    }

    fn finish(self) -> Column {
        Column { name: self.name, len: self.values.len(), values: Values::Codes(self.values, self.names), validity: self.valid.validity() }
    }
}

struct Flags {
    name: &'static str,
    values: Bits,
    valid: Bits,
}

impl Flags {
    fn new(name: &'static str) -> Self {
        Flags { name, values: Bits::default(), valid: Bits::default() }
    }

    fn push(&mut self, v: Option<bool>) {
        self.values.push(v.unwrap_or(false));
        self.valid.push(v.is_some());
    }

    fn finish(self) -> Column {
        Column { name: self.name, len: self.values.len, values: Values::Bool(self.values.bytes), validity: self.valid.validity() }
    }
}

fn side(s: Side) -> usize {
    match s {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

fn liquidity(l: Liquidity) -> usize {
    match l {
        Liquidity::Maker => 0,
        Liquidity::Taker => 1,
        Liquidity::Auction => 2,
    }
}

fn tif(t: TimeInForce) -> usize {
    match t {
        TimeInForce::Gtc => 0,
        TimeInForce::Ioc => 1,
        TimeInForce::Fok => 2,
    }
}

fn reject(r: RejectReason) -> usize {
    match r {
        RejectReason::MarketClosed => 0,
        RejectReason::TradingHalted => 1,
        RejectReason::InvalidQty => 2,
        RejectReason::LotSize => 3,
        RejectReason::MaxQty => 4,
        RejectReason::TickSize => 5,
        RejectReason::PriceBounds => 6,
        RejectReason::TifNotAllowedInPhase => 7,
        RejectReason::InvalidPostOnly => 8,
        RejectReason::PostOnlyWouldCross => 9,
        RejectReason::UnknownOrder => 10,
        RejectReason::NotOwner => 11,
        RejectReason::MarketOrderNotModifiable => 12,
    }
}

fn cancel(r: CancelReason) -> usize {
    13 + match r {
        CancelReason::Requested => 0,
        CancelReason::MassCancel => 1,
        CancelReason::IocRemainder => 2,
        CancelReason::FokUnfilled => 3,
        CancelReason::SelfTrade => 4,
        CancelReason::ModifiedBelowFilled => 5,
        CancelReason::AuctionMarketRemainder => 6,
    }
}

fn request(r: RequestKind) -> usize {
    match r {
        RequestKind::Cancel => 0,
        RequestKind::Modify => 1,
    }
}

fn phase(p: Phase) -> usize {
    match p {
        Phase::Continuous => 0,
        Phase::Auction => 1,
        Phase::Halted => 2,
        Phase::Closed => 3,
    }
}

/// The fields of one event row; `None` where the field does not apply.
#[derive(Default)]
struct Row {
    kind: usize,
    order_id: Option<i64>,
    owner: Option<i64>,
    side: Option<usize>,
    price: Option<i64>,
    qty: Option<i64>,
    leaves: Option<i64>,
    liquidity: Option<usize>,
    trade_id: Option<i64>,
    aggressor: Option<usize>,
    buy_order_id: Option<i64>,
    sell_order_id: Option<i64>,
    client_tag: Option<i64>,
    tif: Option<usize>,
    reason: Option<usize>,
    request: Option<usize>,
    priority_kept: Option<bool>,
    phase: Option<usize>,
}

fn row(e: &Event, ix: &OrderIndex) -> Row {
    let side_of = |id: u64| ix.by_id.get(&id).map(|o| side(o.side));
    // Ids are u64 in the engine; they are far below 2^63 in any run, so they fit Arrow's int64.
    let id = |x: u64| Some(x as i64);
    match e.kind {
        EventKind::Accepted { order_id, owner, client_tag, side: s, price, qty, tif: t } => Row {
            kind: 0,
            order_id: id(order_id),
            owner: Some(i64::from(owner)),
            side: Some(side(s)),
            price,
            qty: Some(qty),
            client_tag: id(client_tag),
            tif: Some(tif(t)),
            ..Row::default()
        },
        EventKind::Rejected { order_id, owner, client_tag, reason } => {
            Row { kind: 1, order_id: id(order_id), owner: Some(i64::from(owner)), client_tag: id(client_tag), reason: Some(reject(reason)), ..Row::default() }
        }
        EventKind::Fill { order_id, owner, trade_id, price, qty, leaves, liquidity: l } => Row {
            kind: 2,
            order_id: id(order_id),
            owner: Some(i64::from(owner)),
            side: side_of(order_id),
            price: Some(price),
            qty: Some(qty),
            leaves: Some(leaves),
            liquidity: Some(liquidity(l)),
            trade_id: id(trade_id),
            ..Row::default()
        },
        EventKind::Cancelled { order_id, owner, qty, reason } => {
            Row { kind: 3, order_id: id(order_id), owner: Some(i64::from(owner)), side: side_of(order_id), qty: Some(qty), reason: Some(cancel(reason)), ..Row::default() }
        }
        EventKind::Replaced { order_id, owner, price, qty, priority_kept } => Row {
            kind: 4,
            order_id: id(order_id),
            owner: Some(i64::from(owner)),
            side: side_of(order_id),
            price: Some(price),
            qty: Some(qty),
            priority_kept: Some(priority_kept),
            ..Row::default()
        },
        EventKind::RequestRejected { order_id, owner, request: r, reason } => Row {
            kind: 5,
            order_id: id(order_id),
            owner: Some(i64::from(owner)),
            side: side_of(order_id),
            request: Some(request(r)),
            reason: Some(reject(reason)),
            ..Row::default()
        },
        EventKind::BookAdd { order_id, side: s, price, qty } => Row { kind: 6, order_id: id(order_id), side: Some(side(s)), price, qty: Some(qty), ..Row::default() },
        EventKind::BookUpdate { order_id, qty } => Row { kind: 7, order_id: id(order_id), side: side_of(order_id), qty: Some(qty), ..Row::default() },
        EventKind::BookRemove { order_id } => Row { kind: 8, order_id: id(order_id), side: side_of(order_id), ..Row::default() },
        EventKind::Trade { trade_id, price, qty, aggressor, buy_order_id, sell_order_id } => Row {
            kind: 9,
            price: Some(price),
            qty: Some(qty),
            trade_id: id(trade_id),
            aggressor: aggressor.map(side),
            buy_order_id: id(buy_order_id),
            sell_order_id: id(sell_order_id),
            ..Row::default()
        },
        EventKind::AuctionUncross { price, volume } => Row { kind: 10, price, qty: Some(volume), ..Row::default() },
        EventKind::PhaseChanged { phase: p } => Row { kind: 11, phase: Some(phase(p)), ..Row::default() },
    }
}

/// The event table: `seq`, `ts_ns`, `kind`, then every event field.
pub fn event_columns(events: &[Event], ix: &OrderIndex) -> Vec<Column> {
    let (mut seq, mut ts) = (Ints::new("seq"), Ints::new("ts_ns"));
    let mut kind = Codes::new("kind", KINDS);
    let (mut order_id, mut owner) = (Ints::new("order_id"), Ints::new("owner"));
    let mut side_c = Codes::new("side", SIDES);
    let (mut price, mut qty, mut leaves) = (Ints::new("price"), Ints::new("qty"), Ints::new("leaves"));
    let mut liquidity_c = Codes::new("liquidity", LIQUIDITY);
    let mut trade_id = Ints::new("trade_id");
    let mut aggressor = Codes::new("aggressor", SIDES);
    let (mut buy_order_id, mut sell_order_id, mut client_tag) = (Ints::new("buy_order_id"), Ints::new("sell_order_id"), Ints::new("client_tag"));
    let mut tif_c = Codes::new("tif", TIFS);
    let mut reason = Codes::new("reason", REASONS);
    let mut request_c = Codes::new("request", REQUESTS);
    let mut priority_kept = Flags::new("priority_kept");
    let mut phase_c = Codes::new("phase", PHASES);
    for e in events {
        let r = row(e, ix);
        seq.push(Some(e.seq as i64));
        ts.push(Some(e.ts));
        kind.push(Some(r.kind));
        order_id.push(r.order_id);
        owner.push(r.owner);
        side_c.push(r.side);
        price.push(r.price);
        qty.push(r.qty);
        leaves.push(r.leaves);
        liquidity_c.push(r.liquidity);
        trade_id.push(r.trade_id);
        aggressor.push(r.aggressor);
        buy_order_id.push(r.buy_order_id);
        sell_order_id.push(r.sell_order_id);
        client_tag.push(r.client_tag);
        tif_c.push(r.tif);
        reason.push(r.reason);
        request_c.push(r.request);
        priority_kept.push(r.priority_kept);
        phase_c.push(r.phase);
    }
    vec![
        seq.finish(),
        ts.finish(),
        kind.finish(),
        order_id.finish(),
        owner.finish(),
        side_c.finish(),
        price.finish(),
        qty.finish(),
        leaves.finish(),
        liquidity_c.finish(),
        trade_id.finish(),
        aggressor.finish(),
        buy_order_id.finish(),
        sell_order_id.finish(),
        client_tag.finish(),
        tif_c.finish(),
        reason.finish(),
        request_c.finish(),
        priority_kept.finish(),
        phase_c.finish(),
    ]
}

/// The top-of-book table: `ts_ns`, `bid`, `bid_qty`, `ask`, `ask_qty` (prices null when that side
/// is empty).
pub fn book_columns(path: &BookPath) -> Vec<Column> {
    let (mut ts, mut bid, mut bid_qty, mut ask, mut ask_qty) = (Ints::new("ts_ns"), Ints::new("bid"), Ints::new("bid_qty"), Ints::new("ask"), Ints::new("ask_qty"));
    for k in 0..path.times.len() {
        ts.push(Some(path.times[k]));
        bid.push(path.bids[k]);
        bid_qty.push(Some(path.bid_qty[k]));
        ask.push(path.asks[k]);
        ask_qty.push(Some(path.ask_qty[k]));
    }
    vec![ts.finish(), bid.finish(), bid_qty.finish(), ask.finish(), ask_qty.finish()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use lob::{Command, InstrumentSpec, MatchingEngine, NewOrder};

    fn events(cmds: &[(i64, Command)]) -> Vec<Event> {
        let mut eng = MatchingEngine::new(InstrumentSpec::example(), Phase::Continuous).expect("engine");
        let mut out = Vec::new();
        for (t, c) in cmds {
            eng.process(*t, c, &mut out).expect("valid command");
        }
        out
    }

    fn col<'a>(cols: &'a [Column], name: &str) -> &'a Column {
        cols.iter().find(|c| c.name == name).expect("column")
    }

    fn ints(c: &Column) -> Vec<Option<i64>> {
        let Values::I64(v) = &c.values else { panic!("{} is not int64", c.name) };
        (0..c.len).map(|k| c.is_valid(k).then_some(v[k])).collect()
    }

    fn names(c: &Column) -> Vec<Option<&'static str>> {
        let Values::Codes(v, n) = &c.values else { panic!("{} is not coded", c.name) };
        (0..c.len).map(|k| c.is_valid(k).then_some(n[v[k] as usize])).collect()
    }

    #[test]
    fn every_event_becomes_one_row_with_its_own_fields() {
        // Owner 1 rests a 10-share ask at 101; owner 2 buys 4 with an IOC; owner 1 cancels the rest.
        // Event order per MATCHING_RULES ("Event order per command").
        let ev = events(&[
            (1, Command::New(NewOrder::limit(1, Side::Sell, 101, 10))),
            (2, Command::New(NewOrder::limit(2, Side::Buy, 101, 4).with_tif(TimeInForce::Ioc))),
            (3, Command::Cancel { owner: 1, order_id: 1 }),
        ]);
        let ix = OrderIndex::from_events(&ev);
        let cols = event_columns(&ev, &ix);
        assert!(cols.iter().all(|c| c.len == ev.len()), "one row per event in every column");
        let kinds = names(col(&cols, "kind"));
        assert_eq!(
            kinds.iter().map(|k| k.expect("kind is never null")).collect::<Vec<_>>(),
            ["accepted", "book_add", "accepted", "trade", "fill", "fill", "cancelled", "book_remove"],
        );
        let (price, qty, owner) = (ints(col(&cols, "price")), ints(col(&cols, "qty")), ints(col(&cols, "owner")));
        let trade = kinds.iter().position(|k| *k == Some("trade")).expect("trade row");
        assert_eq!((price[trade], qty[trade], owner[trade]), (Some(101), Some(4), None), "a trade print is anonymous");
        assert_eq!(names(col(&cols, "aggressor"))[trade], Some("buy"));
        // Both fills carry their order's side (from its acceptance) and liquidity role.
        let fills: Vec<usize> = (0..ev.len()).filter(|&k| kinds[k] == Some("fill")).collect();
        let (sides, liq) = (names(col(&cols, "side")), names(col(&cols, "liquidity")));
        let mut got: Vec<_> = fills.iter().map(|&k| (owner[k], sides[k], liq[k])).collect();
        got.sort();
        assert_eq!(got, [(Some(1), Some("sell"), Some("maker")), (Some(2), Some("buy"), Some("taker"))]);
        let cancelled = kinds.iter().position(|k| *k == Some("cancelled")).expect("cancel row");
        assert_eq!((qty[cancelled], names(col(&cols, "reason"))[cancelled], sides[cancelled]), (Some(6), Some("requested"), Some("sell")));
        assert_eq!(ints(col(&cols, "seq")), (1..=ev.len() as i64).map(Some).collect::<Vec<_>>());
        // A column that never applies is all null; one that always applies has no bitmap.
        assert!(col(&cols, "ts_ns").validity.is_none());
        assert!(names(col(&cols, "phase")).iter().all(Option::is_none));
    }

    #[test]
    fn bitmaps_are_least_significant_bit_first() {
        let mut b = Ints::new("x");
        for v in [Some(1), None, Some(3), None, None, None, None, None, Some(9)] {
            b.push(v);
        }
        let c = b.finish();
        assert_eq!(c.validity, Some(vec![0b0000_0101, 0b0000_0001]));
        assert_eq!(c.value_bytes()[..8], 1i64.to_le_bytes());
        let mut f = Flags::new("f");
        for v in [Some(true), Some(false), Some(true)] {
            f.push(v);
        }
        let f = f.finish();
        assert_eq!((f.values.clone(), f.validity.clone()), (Values::Bool(vec![0b101]), None));
    }
}
