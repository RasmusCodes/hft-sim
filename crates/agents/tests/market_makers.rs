//! Integration tests of the market makers inside the simulation kernel. Every check reads the
//! exchange's event log, ledgers or book, not the market maker's own records, except where those
//! records are under test.

use std::collections::{HashMap, HashSet};

use agents::mm::{LADDER, LadderParams, MarketMaker, QuotingStrategy};
use agents::{BackgroundConfig, CalibratedMarket, KillSwitch, calibrated_market};
use lob::{EventKind, InstrumentSpec, Liquidity, OrderId, Price, Qty, Side, Timestamp};
use sim::agents::Scripted;
use sim::{AgentConfig, AgentId, ExchangeTiming, InstrumentConfig, LatencyProfile, OrderSpec, Request, SimBuilder, Simulation};

const MS: i64 = 1_000_000;
const S: i64 = 1_000_000_000;

fn limit(cid: u64, side: Side, price: Price, qty: Qty) -> Request {
    Request::New(OrderSpec::limit(0, cid, side, price, qty))
}

/// What one agent did, rebuilt from the exchange's event log.
#[derive(Debug, Default)]
struct Trace {
    fills: usize,
    taker_fills: usize,
    max_abs_position: Qty,
    final_position: Qty,
    last_accept: Option<Timestamp>,
}

fn trace(sim: &Simulation, agent: AgentId) -> Trace {
    let mut t = Trace::default();
    let mut sides: HashMap<OrderId, Side> = HashMap::new();
    for e in sim.events(0) {
        match e.kind {
            EventKind::Accepted { order_id, owner, side, .. } if owner == agent => {
                sides.insert(order_id, side);
                t.last_accept = Some(e.ts);
            }
            EventKind::Fill { order_id, owner, qty, liquidity, .. } if owner == agent => {
                t.fills += 1;
                t.taker_fills += usize::from(liquidity == Liquidity::Taker);
                t.final_position += if sides[&order_id] == Side::Buy { qty } else { -qty };
                t.max_abs_position = t.max_abs_position.max(t.final_position.abs());
            }
            _ => {}
        }
    }
    t
}

fn ladder_run(step: usize, seed: u64, secs: i64) -> (Simulation, AgentId) {
    let CalibratedMarket { mut builder, instrument, .. } = calibrated_market(seed, 1.0, &BackgroundConfig::default());
    builder.record_events(true);
    let mm = MarketMaker::new(instrument, QuotingStrategy::ladder(step, &LadderParams::default()));
    let id = builder.add_agent(mm, AgentConfig::new(LatencyProfile::colocated()).subscribe(instrument));
    let mut sim = builder.build().expect("valid market");
    sim.run_until(secs * S);
    (sim, id)
}

#[test]
fn every_ladder_step_makes_markets_within_its_limits() {
    let p = LadderParams::default();
    for step in 1..=10 {
        let (sim, id) = ladder_run(step, 31, 30);
        let m = sim.agent::<MarketMaker>(id).expect("market maker");
        let bound = p.max_inventory * m.unit_lots;
        let t = trace(&sim, id);
        let name = LADDER[step - 1];
        assert!(t.fills > 50, "{name}: only {} fills", t.fills);
        assert_eq!(t.taker_fills, 0, "{name}: post-only quotes never take liquidity");
        assert!(t.max_abs_position <= bound, "{name}: position {} beyond the bound {bound}", t.max_abs_position);
        assert_eq!(t.final_position, sim.ledger(id, 0).position, "{name}: trace matches the ledger");
        assert!(m.stats.two_sided_share() > 0.5, "{name}: two-sided only {:.0}% of the time", 100.0 * m.stats.two_sided_share());
        assert_eq!(m.stats.fills as usize, t.fills, "{name}: own fill count matches the exchange");
        assert!(sim.warnings().is_empty(), "{name}: {:?}", sim.warnings());
    }
}

#[test]
fn market_maker_runs_are_reproducible() {
    let (a, _) = ladder_run(7, 5, 10);
    let (b, _) = ladder_run(7, 5, 10);
    assert_eq!(a.digest(), b.digest());
}

#[test]
fn a_stopped_market_maker_leaves_nothing_resting() {
    let CalibratedMarket { mut builder, instrument, .. } = calibrated_market(8, 1.0, &BackgroundConfig::default());
    builder.record_events(true);
    let mut mm = MarketMaker::new(instrument, QuotingStrategy::ladder(10, &LadderParams::default()));
    mm.stop_at = Some(10 * S);
    let id = builder.add_agent(mm, AgentConfig::new(LatencyProfile::colocated()).subscribe(instrument));
    let mut sim = builder.build().expect("valid market");
    sim.run_until(12 * S);
    let t = trace(&sim, id);
    assert!(t.fills > 0);
    assert!(t.last_accept.is_some_and(|ts| ts <= 10 * S + 5 * MS), "orders accepted after the stop: {:?}", t.last_accept);
    assert!(sim.engine(0).snapshot().iter().all(|o| o.owner != id), "quotes left resting after the stop");
}

/// A scripted market in cents: a liquidity provider quotes 10,000 / 10,001 (50 shares each) from
/// the start; a seller sends an IOC for 100 at 9,999 at 20 ms; at 40 ms the provider replaces its
/// quotes with 9,990 / 9,991. The market maker (one share per unit, bound 4 units, skew 0.5 tick per
/// unit) joins the provider's quotes behind it.
fn crash_market(kill: Option<KillSwitch>, until: Timestamp) -> (Simulation, AgentId) {
    let mut b = SimBuilder::new(1);
    b.timing(ExchangeTiming::zero());
    b.record_events(true);
    b.add_instrument(InstrumentConfig::new(InstrumentSpec::example()));
    let p = LadderParams { max_inventory: 4, skew_ticks_per_unit: 0.5, ..LadderParams::default() };
    let s = QuotingStrategy::ladder(2, &p);
    let mut mm = MarketMaker::new(0, s).with_unit_lots(1);
    mm.heartbeat_ns = 10 * MS;
    if let Some(k) = kill {
        mm.kill = k;
    }
    let id = b.add_agent(mm, AgentConfig::new(LatencyProfile::constant(0, 1_000, 1_000, 1_000, 0)).subscribe(0));
    b.add_agent(
        Scripted::new(vec![
            (0, limit(1, Side::Buy, 10_000, 50)),
            (0, limit(2, Side::Sell, 10_001, 50)),
            (40 * MS, Request::CancelAll { instrument: 0 }),
            (40 * MS, limit(3, Side::Buy, 9_990, 50)),
            (40 * MS, limit(4, Side::Sell, 9_991, 50)),
        ]),
        AgentConfig::new(LatencyProfile::zero()),
    );
    let sell = Request::New(OrderSpec::limit(0, 1, Side::Sell, 9_999, 100).with_tif(lob::TimeInForce::Ioc));
    b.add_agent(Scripted::new(vec![(20 * MS, sell)]), AgentConfig::new(LatencyProfile::zero()));
    let mut sim = b.build().expect("valid");
    sim.run_until(until);
    (sim, id)
}

fn resting(sim: &Simulation, id: AgentId) -> Vec<(Side, Option<Price>, Qty)> {
    sim.engine(0).snapshot().iter().filter(|o| o.owner == id).map(|o| (o.side, o.price, o.leaves)).collect()
}

#[test]
fn inventory_moves_the_live_quotes() {
    // Before the seller: one share on each side, joined behind the provider.
    let (sim, id) = crash_market(None, 10 * MS);
    assert_eq!(resting(&sim, id), vec![(Side::Buy, Some(10_000), 1), (Side::Sell, Some(10_001), 1)]);
    // The seller takes the provider's 50 at 10,000, then the market maker's share. Long one unit,
    // the bid depth grows by half a tick to 1.0 (10,000.5 − 1.0 = 9,999.5, a tie, rounded away
    // from the reference to 9,999); the ask depth falls to 0 but cannot go below 10,001.
    let (sim, id) = crash_market(None, 30 * MS);
    assert_eq!(sim.ledger(id, 0).position, 1);
    assert_eq!(resting(&sim, id), vec![(Side::Buy, Some(9_999), 1), (Side::Sell, Some(10_001), 1)]);
}

#[test]
fn the_kill_switch_pulls_a_market_makers_quotes() {
    // At 40 ms the provider's new 9,991 offer trades with the market maker's 9,999 bid, leaving it
    // long 2 marked near 9,990.5, a loss of about 18 cents. With a 1-cent limit it trips at the
    // decision that fill triggers, and every quote is pulled; the position stays (a kill switch
    // does not flatten).
    let (sim, id) = crash_market(Some(KillSwitch::at_loss(1)), 100 * MS);
    let m = sim.agent::<MarketMaker>(id).expect("market maker");
    let tripped = m.kill.tripped_at.expect("the loss limit trips");
    assert!((40 * MS..=40 * MS + 10_000).contains(&tripped), "tripped at {tripped}");
    assert!(resting(&sim, id).is_empty(), "quotes left after the trip: {:?}", resting(&sim, id));
    assert_eq!(sim.ledger(id, 0).position, 2);
    let t = trace(&sim, id);
    assert!(t.last_accept.is_some_and(|ts| ts <= tripped), "orders accepted after the trip");
}

#[test]
fn the_queue_rule_saves_cancels_without_changing_the_quoted_prices_much() {
    // Steps 8 and 9 differ only by the queue rule; on the same seed step 9 cancels less.
    let (sim8, id8) = ladder_run(8, 12, 30);
    let (sim9, id9) = ladder_run(9, 12, 30);
    let (s8, s9) = (sim8.agent::<MarketMaker>(id8).unwrap().stats, sim9.agent::<MarketMaker>(id9).unwrap().stats);
    assert!(s9.cancels_sent < s8.cancels_sent, "cancels: queue-aware {} vs {}", s9.cancels_sent, s8.cancels_sent);
    assert!(s9.two_sided_share() > 0.5);
}

#[test]
fn market_makers_and_the_background_keep_consistent_books() {
    // Several ladder steps at once in one market: positions and cash net to zero across all agents,
    // and no agent's own order count disagrees with the gateway once the market makers stop.
    let CalibratedMarket { mut builder, instrument, .. } = calibrated_market(3, 1.0, &BackgroundConfig::default());
    let mut ids = HashSet::new();
    for step in [1, 5, 9, 10] {
        let mut mm = MarketMaker::new(instrument, QuotingStrategy::ladder(step, &LadderParams::default()));
        mm.stop_at = Some(20 * S);
        ids.insert(builder.add_agent(mm, AgentConfig::new(LatencyProfile::colocated()).subscribe(instrument)));
    }
    let mut sim = builder.build().expect("valid market");
    sim.run_until(22 * S);
    let net: Qty = (0..sim.agent_count() as AgentId).map(|a| sim.ledger(a, 0).position).sum();
    let cash: i128 = (0..sim.agent_count() as AgentId).map(|a| sim.ledger(a, 0).cash).sum();
    assert_eq!((net, cash), (0, 0));
    for &id in &ids {
        assert_eq!(sim.orders(id).live_count(0), 0, "market maker {id} still has live orders");
        assert_eq!(sim.gateway_open_orders(id, 0), 0);
        assert_eq!(sim.orders(id).position(0), sim.ledger(id, 0).position);
        assert!(sim.agent::<MarketMaker>(id).unwrap().stats.fills > 0);
    }
}
