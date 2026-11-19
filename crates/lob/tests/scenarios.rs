//! Rulebook scenarios (`docs/MATCHING_RULES.md`). Expected outcomes are worked out by hand
//! from the rules; every command also passes through the reference model and all
//! cross-checks of the harness.

mod common;

use common::{H, spec, trades};
use lob::EventKind as K;
use lob::{
    AuctionQuote, CancelReason, Command, EngineError, InstrumentSpec, LevelSnapshot, Liquidity,
    MatchingEngine, NewOrder, OrderId, Phase, Price, RejectReason, RequestKind, Side, SpecError,
    StpMode, TimeInForce,
};

fn reject_reason(events: &[K]) -> RejectReason {
    match events {
        [K::Rejected { reason, .. }] | [K::RequestRejected { reason, .. }] => *reason,
        other => panic!("expected a single rejection, got {other:?}"),
    }
}

fn auction(reference: Price) -> H {
    H::with(InstrumentSpec { reference_price: reference, ..spec() }, Phase::Auction)
}

// ---- book basics ----------------------------------------------------------------------

#[test]
fn empty_book_has_no_quotes() {
    let h = H::new();
    assert_eq!(h.eng.best_bid(), None);
    assert_eq!(h.eng.best_ask(), None);
    assert_eq!(h.eng.spread(), None);
    assert_eq!(h.eng.mid(), None);
    assert!(h.eng.depth(Side::Buy, 5).is_empty());
}

#[test]
fn passive_limit_order_rests_and_sets_the_bid() {
    let mut h = H::new();
    let (id, ev) = h.submit(NewOrder::limit(1, Side::Buy, 99, 10).with_tag(7));
    assert_eq!(id, 1);
    assert_eq!(
        ev,
        vec![
            K::Accepted { order_id: 1, owner: 1, client_tag: 7, side: Side::Buy, price: Some(99), qty: 10, tif: TimeInForce::Gtc },
            K::BookAdd { order_id: 1, side: Side::Buy, price: Some(99), qty: 10 },
        ]
    );
    assert_eq!(h.eng.best_bid(), Some((99, 10)));
    assert_eq!(h.eng.best_ask(), None);
}

#[test]
fn spread_mid_and_depth() {
    let mut h = H::new();
    h.buy(1, 99, 10);
    h.buy(2, 99, 5);
    h.buy(1, 98, 7);
    h.buy(3, 95, 1);
    h.sell(2, 101, 4);
    h.sell(3, 102, 6);
    assert_eq!(h.eng.spread(), Some(2));
    assert_eq!(h.eng.mid(), Some(100.0));
    assert_eq!(
        h.eng.depth(Side::Buy, 2),
        vec![LevelSnapshot { price: 99, qty: 15, count: 2 }, LevelSnapshot { price: 98, qty: 7, count: 1 }]
    );
    assert_eq!(
        h.eng.depth(Side::Sell, 10),
        vec![LevelSnapshot { price: 101, qty: 4, count: 1 }, LevelSnapshot { price: 102, qty: 6, count: 1 }]
    );
    assert_eq!(h.mbo.depth(Side::Buy, 3), h.eng.depth(Side::Buy, 3));
}

// ---- continuous matching --------------------------------------------------------------

#[test]
fn marketable_limit_trades_at_the_resting_price() {
    let mut h = H::new();
    let s = h.sell(1, 100, 10);
    let (b, ev) = h.submit(NewOrder::limit(2, Side::Buy, 102, 4));
    assert_eq!(
        ev,
        vec![
            K::Accepted { order_id: b, owner: 2, client_tag: 0, side: Side::Buy, price: Some(102), qty: 4, tif: TimeInForce::Gtc },
            K::Trade { trade_id: 1, price: 100, qty: 4, aggressor: Some(Side::Buy), buy_order_id: b, sell_order_id: s },
            K::Fill { order_id: s, owner: 1, trade_id: 1, price: 100, qty: 4, leaves: 6, liquidity: Liquidity::Maker },
            K::Fill { order_id: b, owner: 2, trade_id: 1, price: 100, qty: 4, leaves: 0, liquidity: Liquidity::Taker },
        ]
    );
    assert_eq!(h.eng.best_ask(), Some((100, 6)));
    assert_eq!(h.eng.best_bid(), None);
    assert_eq!(h.eng.reference_price(), 100);
}

#[test]
fn price_priority_then_time_priority() {
    let mut h = H::new();
    let a = h.sell(1, 101, 5); // worse price, earliest
    let b = h.sell(2, 100, 5); // best price
    let c = h.sell(3, 100, 5); // best price, later
    let (t, ev) = h.submit(NewOrder::limit(4, Side::Buy, 101, 12));
    assert_eq!(trades(&ev), vec![(100, 5, t, b), (100, 5, t, c), (101, 2, t, a)]);
    assert_eq!(h.eng.best_ask(), Some((101, 3)));
}

#[test]
fn sweep_levels_then_rest_the_remainder() {
    let mut h = H::new();
    h.sell(1, 100, 3);
    h.sell(1, 101, 3);
    h.sell(1, 103, 3);
    let (t, ev) = h.submit(NewOrder::limit(2, Side::Buy, 102, 10));
    let prints: Vec<_> = trades(&ev).iter().map(|x| (x.0, x.1)).collect();
    assert_eq!(prints, vec![(100, 3), (101, 3)]);
    assert_eq!(ev.last(), Some(&K::BookAdd { order_id: t, side: Side::Buy, price: Some(102), qty: 4 }));
    assert_eq!(h.eng.best_bid(), Some((102, 4)));
    assert_eq!(h.eng.best_ask(), Some((103, 3)));
    let o = h.eng.order(t).expect("resting");
    assert_eq!((o.leaves, o.cum_qty, o.order_qty), (4, 6, 10));
}

#[test]
fn market_order_sweeps_the_book_and_cancels_the_rest() {
    let mut h = H::new();
    h.sell(1, 100, 2);
    h.sell(1, 105, 3);
    let (m, ev) = h.submit(NewOrder::market(2, Side::Buy, 10));
    let prints: Vec<_> = trades(&ev).iter().map(|x| (x.0, x.1)).collect();
    assert_eq!(prints, vec![(100, 2), (105, 3)]);
    assert_eq!(ev.last(), Some(&K::Cancelled { order_id: m, owner: 2, qty: 5, reason: CancelReason::IocRemainder }));
    assert_eq!(h.eng.live_orders(), 0);
}

#[test]
fn market_order_against_an_empty_side_is_cancelled_in_full() {
    let mut h = H::new();
    h.buy(1, 99, 5);
    let (m, ev) = h.submit(NewOrder::market(2, Side::Buy, 10));
    assert_eq!(
        ev,
        vec![
            K::Accepted { order_id: m, owner: 2, client_tag: 0, side: Side::Buy, price: None, qty: 10, tif: TimeInForce::Ioc },
            K::Cancelled { order_id: m, owner: 2, qty: 10, reason: CancelReason::IocRemainder },
        ]
    );
}

#[test]
fn gtc_market_order_never_rests_in_continuous_trading() {
    let mut h = H::new();
    h.sell(1, 100, 2);
    let (m, ev) = h.submit(NewOrder::market(2, Side::Buy, 5).with_tif(TimeInForce::Gtc));
    assert_eq!(ev.last(), Some(&K::Cancelled { order_id: m, owner: 2, qty: 3, reason: CancelReason::IocRemainder }));
    assert_eq!(h.eng.live_orders(), 0);
}

#[test]
fn ioc_fills_what_it_can_and_cancels_the_rest() {
    let mut h = H::new();
    h.sell(1, 100, 4);
    h.sell(1, 102, 4);
    let (t, ev) = h.submit(NewOrder::limit(2, Side::Buy, 101, 10).with_tif(TimeInForce::Ioc));
    assert_eq!(trades(&ev).len(), 1);
    assert_eq!(ev.last(), Some(&K::Cancelled { order_id: t, owner: 2, qty: 6, reason: CancelReason::IocRemainder }));
    assert_eq!(h.eng.best_bid(), None);
    assert_eq!(h.eng.best_ask(), Some((102, 4)));
}

#[test]
fn ioc_that_cannot_trade_is_cancelled_without_resting() {
    let mut h = H::new();
    h.sell(1, 100, 4);
    let (t, ev) = h.submit(NewOrder::limit(2, Side::Buy, 99, 3).with_tif(TimeInForce::Ioc));
    assert_eq!(ev.len(), 2);
    assert_eq!(ev[1], K::Cancelled { order_id: t, owner: 2, qty: 3, reason: CancelReason::IocRemainder });
    assert_eq!(h.eng.best_bid(), None);
}

#[test]
fn fok_fills_when_exactly_enough_liquidity_is_available() {
    let mut h = H::new();
    h.sell(1, 100, 4);
    h.sell(3, 101, 6);
    h.sell(1, 102, 100);
    let (t, ev) = h.submit(NewOrder::limit(2, Side::Buy, 101, 10).with_tif(TimeInForce::Fok));
    let prints: Vec<_> = trades(&ev).iter().map(|x| (x.0, x.1)).collect();
    assert_eq!(prints, vec![(100, 4), (101, 6)]);
    assert!(matches!(ev.last(), Some(K::Fill { order_id, leaves: 0, .. }) if *order_id == t));
}

#[test]
fn fok_short_by_one_lot_is_killed_without_side_effects() {
    let mut h = H::new();
    h.sell(1, 100, 4);
    h.sell(3, 101, 5);
    h.sell(1, 102, 100);
    let before = h.eng.snapshot();
    let (t, ev) = h.submit(NewOrder::limit(2, Side::Buy, 101, 10).with_tif(TimeInForce::Fok));
    assert_eq!(ev[1..], [K::Cancelled { order_id: t, owner: 2, qty: 10, reason: CancelReason::FokUnfilled }]);
    assert_eq!(h.eng.snapshot(), before);
    assert_eq!(h.eng.reference_price(), 100);
}

#[test]
fn market_fok_ignores_price_but_needs_the_full_size() {
    let mut h = H::new();
    h.sell(1, 100, 4);
    h.sell(1, 150, 6);
    let (_, ev) = h.submit(NewOrder::market(2, Side::Buy, 10).with_tif(TimeInForce::Fok));
    assert_eq!(trades(&ev).len(), 2);
    let (t, ev) = h.submit(NewOrder::market(2, Side::Buy, 1).with_tif(TimeInForce::Fok));
    assert_eq!(ev[1], K::Cancelled { order_id: t, owner: 2, qty: 1, reason: CancelReason::FokUnfilled });
}

// ---- cancel ---------------------------------------------------------------------------

#[test]
fn cancel_removes_the_order_and_its_empty_level() {
    let mut h = H::new();
    let a = h.buy(1, 99, 5);
    h.buy(1, 98, 5);
    assert_eq!(
        h.cancel(1, a),
        vec![K::Cancelled { order_id: a, owner: 1, qty: 5, reason: CancelReason::Requested }, K::BookRemove { order_id: a }]
    );
    assert_eq!(h.eng.best_bid(), Some((98, 5)));
    assert_eq!(h.eng.level_count(Side::Buy), 1);
}

#[test]
fn cancel_of_dead_or_unknown_orders_is_rejected() {
    let mut h = H::new();
    let a = h.buy(1, 99, 5);
    h.cancel(1, a);
    let unknown = |id: OrderId| K::RequestRejected { order_id: id, owner: 1, request: RequestKind::Cancel, reason: RejectReason::UnknownOrder };
    assert_eq!(h.cancel(1, a), vec![unknown(a)]);
    assert_eq!(h.cancel(1, 999), vec![unknown(999)]);
    let s = h.sell(1, 100, 2);
    h.submit(NewOrder::market(2, Side::Buy, 2));
    assert_eq!(h.cancel(1, s), vec![unknown(s)]);
}

#[test]
fn only_the_owner_can_cancel() {
    let mut h = H::new();
    let a = h.buy(1, 99, 5);
    assert_eq!(
        h.cancel(2, a),
        vec![K::RequestRejected { order_id: a, owner: 2, request: RequestKind::Cancel, reason: RejectReason::NotOwner }]
    );
    assert_eq!(h.eng.best_bid(), Some((99, 5)));
}

#[test]
fn cancel_all_cancels_every_order_of_the_owner_in_id_order() {
    let mut h = H::new();
    let a = h.buy(1, 99, 5);
    h.buy(2, 99, 5);
    let b = h.sell(1, 105, 5);
    let c = h.buy(1, 97, 1);
    let ev = h.run(Command::CancelAll { owner: 1 });
    let ids: Vec<OrderId> = ev
        .iter()
        .filter_map(|e| match *e {
            K::Cancelled { order_id, reason: CancelReason::MassCancel, .. } => Some(order_id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![a, b, c]);
    assert_eq!(h.eng.live_orders(), 1);
    assert!(h.run(Command::CancelAll { owner: 1 }).is_empty());
}

// ---- modify ---------------------------------------------------------------------------

#[test]
fn size_reduction_keeps_time_priority() {
    let mut h = H::new();
    let a = h.buy(1, 99, 10);
    let b = h.buy(2, 99, 10);
    assert_eq!(
        h.modify(1, a, 99, 4),
        vec![
            K::Replaced { order_id: a, owner: 1, price: 99, qty: 4, priority_kept: true },
            K::BookUpdate { order_id: a, qty: 4 },
        ]
    );
    let (s, ev) = h.submit(NewOrder::market(3, Side::Sell, 5));
    assert_eq!(trades(&ev), vec![(99, 4, a, s), (99, 1, b, s)]);
}

#[test]
fn size_increase_loses_time_priority() {
    let mut h = H::new();
    let a = h.buy(1, 99, 10);
    let b = h.buy(2, 99, 10);
    assert_eq!(
        h.modify(1, a, 99, 12),
        vec![
            K::Replaced { order_id: a, owner: 1, price: 99, qty: 12, priority_kept: false },
            K::BookRemove { order_id: a },
            K::BookAdd { order_id: a, side: Side::Buy, price: Some(99), qty: 12 },
        ]
    );
    let (s, ev) = h.submit(NewOrder::market(3, Side::Sell, 11));
    assert_eq!(trades(&ev), vec![(99, 10, b, s), (99, 1, a, s)]);
}

#[test]
fn price_change_loses_priority_and_can_trade_as_aggressor() {
    let mut h = H::new();
    let ask = h.sell(2, 101, 3);
    let a = h.buy(1, 99, 10);
    assert_eq!(
        h.modify(1, a, 101, 10),
        vec![
            K::Replaced { order_id: a, owner: 1, price: 101, qty: 10, priority_kept: false },
            K::BookRemove { order_id: a },
            K::Trade { trade_id: 1, price: 101, qty: 3, aggressor: Some(Side::Buy), buy_order_id: a, sell_order_id: ask },
            K::Fill { order_id: ask, owner: 2, trade_id: 1, price: 101, qty: 3, leaves: 0, liquidity: Liquidity::Maker },
            K::Fill { order_id: a, owner: 1, trade_id: 1, price: 101, qty: 3, leaves: 7, liquidity: Liquidity::Taker },
            K::BookAdd { order_id: a, side: Side::Buy, price: Some(101), qty: 7 },
        ]
    );
    let o = h.eng.order(a).expect("resting");
    assert_eq!((o.leaves, o.cum_qty, o.order_qty), (7, 3, 10));
}

#[test]
fn modify_quantity_is_the_total_order_quantity_including_fills() {
    let mut h = H::new();
    let a = h.buy(1, 99, 10);
    h.submit(NewOrder::market(2, Side::Sell, 4)); // a: 4 filled, 6 open
    // New total 8 => 8 - 4 = 4 open: a reduction, so priority is kept.
    assert_eq!(h.modify(1, a, 99, 8)[0], K::Replaced { order_id: a, owner: 1, price: 99, qty: 4, priority_kept: true });
    // New total 4 = filled quantity => nothing left open: the order is done.
    assert_eq!(
        h.modify(1, a, 99, 4),
        vec![
            K::Cancelled { order_id: a, owner: 1, qty: 4, reason: CancelReason::ModifiedBelowFilled },
            K::BookRemove { order_id: a },
        ]
    );
    assert_eq!(h.eng.live_orders(), 0);
}

#[test]
fn rejected_modify_leaves_the_order_untouched() {
    let mut h = H::with(InstrumentSpec { tick_size: 5, lot_size: 10, min_price: 5, ..spec() }, Phase::Continuous);
    let a = h.buy(1, 100, 20);
    let before = h.eng.snapshot();
    assert_eq!(reject_reason(&h.modify(1, a, 101, 20)), RejectReason::TickSize);
    assert_eq!(reject_reason(&h.modify(1, a, 100, 25)), RejectReason::LotSize);
    assert_eq!(reject_reason(&h.modify(1, a, 100, 0)), RejectReason::InvalidQty);
    assert_eq!(reject_reason(&h.modify(1, a, 0, 20)), RejectReason::PriceBounds);
    assert_eq!(reject_reason(&h.modify(2, a, 100, 10)), RejectReason::NotOwner);
    assert_eq!(reject_reason(&h.modify(1, 77, 100, 10)), RejectReason::UnknownOrder);
    assert_eq!(h.eng.snapshot(), before);
}

#[test]
fn queue_position_is_visible_to_the_engine_and_the_feed() {
    let mut h = H::new();
    let a = h.buy(1, 99, 5);
    let b = h.buy(2, 99, 7);
    let c = h.buy(3, 99, 1);
    assert_eq!(h.eng.queue_ahead(c), Some(12));
    assert_eq!(h.mbo.queue_ahead(c), Some(12));
    h.modify(1, a, 99, 2);
    assert_eq!(h.eng.queue_ahead(c), Some(9));
    h.cancel(2, b);
    assert_eq!(h.eng.queue_ahead(c), Some(2));
    assert_eq!(h.mbo.queue_ahead(c), Some(2));
}

// ---- validation -----------------------------------------------------------------------

#[test]
fn tick_lot_bound_and_size_validation() {
    let s = InstrumentSpec { tick_size: 5, lot_size: 10, min_price: 5, max_price: 1_000, max_order_qty: 500, reference_price: 100 };
    let mut h = H::with(s, Phase::Continuous);
    let mut reason = |o: NewOrder| reject_reason(&h.submit(o).1);
    assert_eq!(reason(NewOrder::limit(1, Side::Buy, 101, 10)), RejectReason::TickSize);
    assert_eq!(reason(NewOrder::limit(1, Side::Buy, 100, 15)), RejectReason::LotSize);
    assert_eq!(reason(NewOrder::limit(1, Side::Buy, 100, 0)), RejectReason::InvalidQty);
    assert_eq!(reason(NewOrder::limit(1, Side::Buy, 100, -10)), RejectReason::InvalidQty);
    assert_eq!(reason(NewOrder::limit(1, Side::Buy, 100, 510)), RejectReason::MaxQty);
    assert_eq!(reason(NewOrder::limit(1, Side::Buy, 1_005, 10)), RejectReason::PriceBounds);
    assert_eq!(reason(NewOrder::limit(1, Side::Buy, 0, 10)), RejectReason::PriceBounds);
    // Quantity is validated before price.
    assert_eq!(reason(NewOrder::limit(1, Side::Buy, 101, 15)), RejectReason::LotSize);
    // Rejected orders consume ids too.
    let (id, ev) = h.submit(NewOrder::limit(1, Side::Buy, 100, 10));
    assert_eq!(id, 9);
    assert!(matches!(ev[0], K::Accepted { .. }));
}

#[test]
fn post_only_rests_when_passive_and_is_rejected_when_marketable() {
    let mut h = H::new();
    h.sell(1, 101, 5);
    let (_, ev) = h.submit(NewOrder::limit(2, Side::Buy, 100, 5).with_post_only());
    assert!(matches!(ev[1], K::BookAdd { .. }));
    let (_, ev) = h.submit(NewOrder::limit(2, Side::Buy, 101, 5).with_post_only());
    assert_eq!(reject_reason(&ev), RejectReason::PostOnlyWouldCross);
    assert_eq!(h.eng.best_ask(), Some((101, 5)));
}

#[test]
fn post_only_requires_a_gtc_limit_order() {
    let mut h = H::new();
    for o in [
        NewOrder::market(1, Side::Buy, 5).with_post_only(),
        NewOrder::limit(1, Side::Buy, 100, 5).with_tif(TimeInForce::Ioc).with_post_only(),
    ] {
        assert_eq!(reject_reason(&h.submit(o).1), RejectReason::InvalidPostOnly);
    }
}

#[test]
fn post_only_modify_that_would_cross_is_rejected() {
    let mut h = H::new();
    h.sell(2, 101, 5);
    let (a, _) = h.submit(NewOrder::limit(1, Side::Buy, 100, 5).with_post_only());
    assert_eq!(
        h.modify(1, a, 101, 5),
        vec![K::RequestRejected { order_id: a, owner: 1, request: RequestKind::Modify, reason: RejectReason::PostOnlyWouldCross }]
    );
    assert_eq!(h.eng.best_bid(), Some((100, 5)));
}

#[test]
fn negative_prices_work_when_the_instrument_allows_them() {
    let s = InstrumentSpec { tick_size: 1, lot_size: 1, min_price: -500, max_price: 500, max_order_qty: 1_000, reference_price: -20 };
    let mut h = H::with(s, Phase::Continuous);
    let a = h.sell(1, -40, 5);
    let (b, ev) = h.submit(NewOrder::limit(2, Side::Buy, -30, 8));
    assert_eq!(trades(&ev), vec![(-40, 5, b, a)]);
    assert_eq!(h.eng.best_bid(), Some((-30, 3)));
    h.phase(Phase::Auction);
    h.sell(3, -35, 3);
    // V = 3 with zero surplus on [-35, -30]; reference (last trade) -40 => closest is -35.
    assert_eq!(h.eng.indicative_auction(), Some(AuctionQuote { price: -35, volume: 3, imbalance: 0 }));

    let s5 = InstrumentSpec { tick_size: 5, min_price: -500, reference_price: -20, ..s };
    let mut h = H::with(s5, Phase::Continuous);
    assert_eq!(reject_reason(&h.submit(NewOrder::limit(1, Side::Buy, -37, 1)).1), RejectReason::TickSize);
    assert!(matches!(h.submit(NewOrder::limit(1, Side::Buy, -35, 1)).1[1], K::BookAdd { .. }));
}

#[test]
fn invalid_instrument_specs_are_refused() {
    let ok = spec();
    assert!(MatchingEngine::new(InstrumentSpec { tick_size: 0, ..ok }, Phase::Continuous).is_err());
    assert_eq!(InstrumentSpec { lot_size: 0, ..ok }.validate(), Err(SpecError::LotSize));
    assert_eq!(InstrumentSpec { min_price: 10, max_price: 5, ..ok }.validate(), Err(SpecError::PriceBounds));
    assert_eq!(
        InstrumentSpec { tick_size: 5, min_price: 5, reference_price: 101, ..ok }.validate(),
        Err(SpecError::ReferencePrice)
    );
    assert_eq!(InstrumentSpec { max_order_qty: 0, ..ok }.validate(), Err(SpecError::MaxOrderQty));
    assert_eq!(ok.validate(), Ok(()));
}

// ---- self-trade prevention ------------------------------------------------------------

#[test]
fn stp_cancel_resting_skips_own_orders_and_keeps_matching() {
    let mut h = H::new();
    let own = h.sell(1, 100, 5);
    let other = h.sell(2, 100, 5);
    let (t, ev) = h.submit(NewOrder::limit(1, Side::Buy, 100, 5).with_stp(StpMode::CancelResting));
    assert_eq!(ev[1], K::Cancelled { order_id: own, owner: 1, qty: 5, reason: CancelReason::SelfTrade });
    assert_eq!(ev[2], K::BookRemove { order_id: own });
    assert_eq!(trades(&ev), vec![(100, 5, t, other)]);
    assert_eq!(h.eng.live_orders(), 0);
}

#[test]
fn stp_cancel_aggressor_cancels_the_incoming_remainder() {
    let mut h = H::new();
    let other = h.sell(2, 100, 2);
    let own = h.sell(1, 100, 5);
    let (t, ev) = h.submit(NewOrder::limit(1, Side::Buy, 101, 10).with_stp(StpMode::CancelAggressor));
    assert_eq!(trades(&ev), vec![(100, 2, t, other)]);
    assert_eq!(ev.last(), Some(&K::Cancelled { order_id: t, owner: 1, qty: 8, reason: CancelReason::SelfTrade }));
    assert_eq!(h.eng.order(own).map(|o| o.leaves), Some(5));
    assert_eq!(h.eng.best_bid(), None, "the GTC remainder must not rest");
}

#[test]
fn stp_cancel_both() {
    let mut h = H::new();
    let own = h.sell(1, 100, 5);
    let (t, ev) = h.submit(NewOrder::limit(1, Side::Buy, 100, 3).with_stp(StpMode::CancelBoth));
    assert_eq!(
        ev[1..],
        [
            K::Cancelled { order_id: own, owner: 1, qty: 5, reason: CancelReason::SelfTrade },
            K::BookRemove { order_id: own },
            K::Cancelled { order_id: t, owner: 1, qty: 3, reason: CancelReason::SelfTrade },
        ]
    );
    assert_eq!(h.eng.live_orders(), 0);
}

#[test]
fn self_trades_are_allowed_without_stp() {
    let mut h = H::new();
    let own = h.sell(1, 100, 5);
    let (t, ev) = h.submit(NewOrder::limit(1, Side::Buy, 100, 5));
    assert_eq!(trades(&ev), vec![(100, 5, t, own)]);
}

#[test]
fn fok_counts_only_liquidity_that_survives_self_trade_prevention() {
    let mut h = H::new();
    let own = h.sell(1, 100, 5);
    let other = h.sell(2, 100, 5);
    // Only 5 units are not self-owned: a FOK for 10 is killed and the own order survives.
    let fok = |qty, stp| NewOrder::limit(1, Side::Buy, 100, qty).with_tif(TimeInForce::Fok).with_stp(stp);
    let (t, ev) = h.submit(fok(10, StpMode::CancelResting));
    assert_eq!(ev[1..], [K::Cancelled { order_id: t, owner: 1, qty: 10, reason: CancelReason::FokUnfilled }]);
    assert!(h.eng.order(own).is_some());
    // A FOK for 5 fills against the other participant after cancelling the own order.
    let (t, ev) = h.submit(fok(5, StpMode::CancelResting));
    assert_eq!(trades(&ev), vec![(100, 5, t, other)]);
    assert!(h.eng.order(own).is_none());
    // With cancel-aggressor, an own order ahead in the queue blocks the FOK entirely.
    let own2 = h.sell(1, 100, 5);
    h.sell(2, 100, 5);
    let (t, ev) = h.submit(fok(5, StpMode::CancelAggressor));
    assert_eq!(ev[1..], [K::Cancelled { order_id: t, owner: 1, qty: 5, reason: CancelReason::FokUnfilled }]);
    assert!(h.eng.order(own2).is_some());
}

// ---- phases: halt, close, auction -----------------------------------------------------

#[test]
fn halt_accepts_only_cancels() {
    let mut h = H::new();
    let a = h.buy(1, 99, 5);
    assert_eq!(h.phase(Phase::Halted), vec![K::PhaseChanged { phase: Phase::Halted }]);
    assert_eq!(reject_reason(&h.submit(NewOrder::limit(2, Side::Sell, 99, 5)).1), RejectReason::TradingHalted);
    assert_eq!(reject_reason(&h.modify(1, a, 98, 5)), RejectReason::TradingHalted);
    assert!(matches!(h.cancel(1, a).as_slice(), [K::Cancelled { .. }, K::BookRemove { .. }]));
    h.phase(Phase::Continuous);
    assert!(matches!(h.submit(NewOrder::limit(2, Side::Sell, 99, 5)).1[1], K::BookAdd { .. }));
}

#[test]
fn closed_market_rejects_new_orders() {
    let mut h = H::with(spec(), Phase::Closed);
    assert_eq!(reject_reason(&h.submit(NewOrder::limit(1, Side::Buy, 99, 5)).1), RejectReason::MarketClosed);
}

#[test]
fn call_phase_accumulates_a_crossed_book_without_trading() {
    let mut h = auction(100);
    let (_, ev) = h.submit(NewOrder::limit(1, Side::Buy, 102, 5));
    assert!(matches!(ev.as_slice(), [K::Accepted { .. }, K::BookAdd { .. }]));
    let (_, ev) = h.submit(NewOrder::limit(2, Side::Sell, 98, 3));
    assert!(matches!(ev.as_slice(), [K::Accepted { .. }, K::BookAdd { .. }]));
    assert_eq!(h.eng.spread(), Some(-4));
    // B = 5, S = 3 at every price in 98..=102: buy surplus everywhere -> highest price.
    assert_eq!(h.eng.indicative_auction(), Some(AuctionQuote { price: 102, volume: 3, imbalance: 2 }));
}

#[test]
fn uncross_maximises_executable_volume() {
    let mut h = auction(100);
    let b1 = h.buy(1, 101, 10);
    let b2 = h.buy(2, 100, 10);
    let s1 = h.sell(3, 99, 5);
    let s2 = h.sell(4, 100, 10);
    h.sell(5, 102, 10);
    // V(99)=5, V(100)=15, V(101)=10, V(102)=0.
    let ev = h.phase(Phase::Continuous);
    assert_eq!(ev[0], K::AuctionUncross { price: Some(100), volume: 15 });
    assert_eq!(trades(&ev), vec![(100, 5, b1, s1), (100, 5, b1, s2), (100, 5, b2, s2)]);
    assert_eq!(ev.last(), Some(&K::PhaseChanged { phase: Phase::Continuous }));
    assert_eq!(h.eng.best_bid(), Some((100, 5)));
    assert_eq!(h.eng.best_ask(), Some((102, 10)));
    assert_eq!(h.eng.reference_price(), 100);
}

#[test]
fn uncross_breaks_volume_ties_by_minimum_surplus() {
    let mut h = auction(100);
    let b = h.buy(1, 103, 10);
    let s1 = h.sell(2, 101, 10);
    h.sell(3, 102, 5);
    // V = 10 on 101..=103; surplus 0 at 101, 5 at 102 and 103.
    assert_eq!(h.eng.indicative_auction(), Some(AuctionQuote { price: 101, volume: 10, imbalance: 0 }));
    let ev = h.phase(Phase::Continuous);
    assert_eq!(trades(&ev), vec![(101, 10, b, s1)]);
    assert_eq!(h.eng.best_ask(), Some((102, 5)));
}

#[test]
fn uncross_follows_market_pressure() {
    let mut h = auction(100);
    h.buy(1, 105, 20);
    h.sell(2, 100, 10);
    assert_eq!(h.eng.indicative_auction(), Some(AuctionQuote { price: 105, volume: 10, imbalance: 10 }));
    let mut h = auction(100);
    h.buy(1, 105, 10);
    h.sell(2, 100, 20);
    assert_eq!(h.eng.indicative_auction(), Some(AuctionQuote { price: 100, volume: 10, imbalance: -10 }));
}

#[test]
fn uncross_uses_the_reference_price_when_surplus_does_not_decide() {
    // V = 10 with zero surplus at every price in 100..=105.
    for (reference, expected) in [(102, 102), (110, 105), (90, 100)] {
        let mut h = auction(reference);
        h.buy(1, 105, 10);
        h.sell(2, 100, 10);
        assert_eq!(h.eng.indicative_auction().map(|q| q.price), Some(expected), "reference {reference}");
    }
}

#[test]
fn uncross_can_price_strictly_between_limit_prices() {
    // Buy surplus 3 at 101, sell surplus 3 at 104; strictly between, eligibility does not
    // change and the surplus is zero, so the interior prices win on step 2.
    let mut h = auction(100);
    let b1 = h.buy(1, 104, 10);
    h.buy(2, 101, 3);
    let s1 = h.sell(3, 101, 10);
    h.sell(4, 104, 3);
    assert_eq!(h.eng.indicative_auction(), Some(AuctionQuote { price: 102, volume: 10, imbalance: 0 }));
    let ev = h.phase(Phase::Continuous);
    assert_eq!(trades(&ev), vec![(102, 10, b1, s1)]);
    assert_eq!(h.eng.best_bid(), Some((101, 3)));
    assert_eq!(h.eng.best_ask(), Some((104, 3)));

    let mut h = auction(110);
    h.buy(1, 104, 10);
    h.buy(2, 101, 3);
    h.sell(3, 101, 10);
    h.sell(4, 104, 3);
    assert_eq!(h.eng.indicative_auction().map(|q| q.price), Some(103));
}

#[test]
fn market_orders_have_auction_priority_and_unfilled_ones_are_cancelled() {
    let mut h = auction(100);
    let (m, _) = h.submit(NewOrder::market(1, Side::Buy, 5).with_tif(TimeInForce::Gtc));
    let b = h.buy(2, 101, 10);
    let s = h.sell(3, 100, 8);
    assert_eq!(h.eng.indicative_auction(), Some(AuctionQuote { price: 101, volume: 8, imbalance: 7 }));
    let ev = h.phase(Phase::Continuous);
    assert_eq!(trades(&ev), vec![(101, 5, m, s), (101, 3, b, s)]);
    assert_eq!(h.eng.best_bid(), Some((101, 7)));

    let mut h = auction(100);
    let (m, _) = h.submit(NewOrder::market(1, Side::Buy, 10).with_tif(TimeInForce::Gtc));
    let s = h.sell(2, 100, 4);
    let ev = h.phase(Phase::Continuous);
    assert_eq!(trades(&ev), vec![(100, 4, m, s)]);
    assert!(ev.contains(&K::Cancelled { order_id: m, owner: 1, qty: 6, reason: CancelReason::AuctionMarketRemainder }));
    assert_eq!(h.eng.live_orders(), 0);
}

#[test]
fn only_market_orders_trade_at_the_reference_price() {
    let mut h = auction(250);
    let (mb, _) = h.submit(NewOrder::market(1, Side::Buy, 5).with_tif(TimeInForce::Gtc));
    let (ms, _) = h.submit(NewOrder::market(2, Side::Sell, 3).with_tif(TimeInForce::Gtc));
    assert_eq!(h.eng.indicative_auction(), Some(AuctionQuote { price: 250, volume: 3, imbalance: 2 }));
    let ev = h.phase(Phase::Continuous);
    assert_eq!(trades(&ev), vec![(250, 3, mb, ms)]);
    assert!(ev.contains(&K::Cancelled { order_id: mb, owner: 1, qty: 2, reason: CancelReason::AuctionMarketRemainder }));
}

#[test]
fn auction_without_crossing_interest_trades_nothing() {
    let mut h = auction(100);
    h.buy(1, 99, 5);
    h.sell(2, 101, 5);
    assert_eq!(h.eng.indicative_auction(), None);
    assert_eq!(
        h.phase(Phase::Continuous),
        vec![K::AuctionUncross { price: None, volume: 0 }, K::PhaseChanged { phase: Phase::Continuous }]
    );
    assert_eq!(h.eng.spread(), Some(2));
}

#[test]
fn call_phase_rejects_ioc_fok_and_post_only() {
    let mut h = auction(100);
    for (o, why) in [
        (NewOrder::limit(1, Side::Buy, 100, 5).with_tif(TimeInForce::Ioc), RejectReason::TifNotAllowedInPhase),
        (NewOrder::limit(1, Side::Buy, 100, 5).with_tif(TimeInForce::Fok), RejectReason::TifNotAllowedInPhase),
        (NewOrder::market(1, Side::Buy, 5), RejectReason::TifNotAllowedInPhase),
        (NewOrder::limit(1, Side::Buy, 100, 5).with_post_only(), RejectReason::InvalidPostOnly),
    ] {
        assert_eq!(reject_reason(&h.submit(o).1), why, "{o:?}");
    }
}

#[test]
fn self_trade_prevention_does_not_apply_in_auctions() {
    let mut h = auction(100);
    let (b, _) = h.submit(NewOrder::limit(1, Side::Buy, 100, 5).with_stp(StpMode::CancelBoth));
    let s = h.sell(1, 100, 5);
    assert_eq!(trades(&h.phase(Phase::Continuous)), vec![(100, 5, b, s)]);
}

#[test]
fn closing_auction_uncrosses_then_closes() {
    let mut h = H::new();
    h.phase(Phase::Auction);
    h.buy(1, 101, 5);
    h.sell(2, 100, 5);
    let ev = h.phase(Phase::Closed);
    assert_eq!(trades(&ev).len(), 1);
    assert_eq!(ev.last(), Some(&K::PhaseChanged { phase: Phase::Closed }));
    assert_eq!(h.eng.phase(), Phase::Closed);
}

#[test]
fn phase_transition_rules() {
    let mut h = H::new();
    let ts = h.ts + 1;
    assert_eq!(
        h.try_run(ts, Command::SetPhase { phase: Phase::Continuous }),
        Err(EngineError::InvalidPhaseTransition { from: Phase::Continuous, to: Phase::Continuous })
    );
    // Halted during a call phase with a crossed book: reopening must go through an auction.
    h.phase(Phase::Auction);
    h.buy(1, 101, 5);
    h.sell(2, 99, 5);
    h.phase(Phase::Halted);
    let ts = h.ts + 1;
    for to in [Phase::Continuous, Phase::Closed] {
        assert_eq!(
            h.try_run(ts, Command::SetPhase { phase: to }),
            Err(EngineError::InvalidPhaseTransition { from: Phase::Halted, to })
        );
    }
    h.phase(Phase::Auction);
    assert_eq!(trades(&h.phase(Phase::Continuous)).len(), 1);
    // A halt from continuous trading leaves a clean book: direct resumption is allowed.
    h.phase(Phase::Halted);
    h.phase(Phase::Continuous);
    h.phase(Phase::Closed);
    h.phase(Phase::Halted);
    h.phase(Phase::Continuous);
}

// ---- determinism and event plumbing ---------------------------------------------------

#[test]
fn non_monotonic_timestamps_are_refused_without_side_effects() {
    let mut h = H::new();
    h.buy(1, 99, 5);
    let ts = h.ts;
    let next_id = h.eng.next_order_id();
    assert_eq!(
        h.try_run(ts - 1, Command::New(NewOrder::limit(1, Side::Sell, 99, 5))),
        Err(EngineError::NonMonotonicTime { last: ts, got: ts - 1 })
    );
    assert_eq!(h.eng.next_order_id(), next_id);
    assert!(h.try_run(ts, Command::New(NewOrder::limit(1, Side::Sell, 100, 5))).is_ok(), "equal timestamps are fine");
}

#[test]
fn events_carry_contiguous_sequence_numbers_and_the_command_timestamp() {
    let mut eng = MatchingEngine::new(spec(), Phase::Continuous).expect("valid spec");
    let mut out = Vec::new();
    eng.process(1_000, &Command::New(NewOrder::limit(1, Side::Sell, 100, 5)), &mut out).expect("ok");
    eng.process(2_000, &Command::New(NewOrder::limit(2, Side::Buy, 100, 5)), &mut out).expect("ok");
    let seqs: Vec<u64> = out.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=out.len() as u64).collect::<Vec<_>>());
    assert!(out[..2].iter().all(|e| e.ts == 1_000));
    assert!(out[2..].iter().all(|e| e.ts == 2_000));
    assert!(out.iter().all(|e| e.kind.is_public() != e.kind.owner().is_some()));
}
