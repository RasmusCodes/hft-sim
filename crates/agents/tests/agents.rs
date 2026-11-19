//! Integration tests of the phase-2 agents, run inside the simulation kernel. Everything checked
//! here is read from the exchange (event log, ledgers, book snapshot), not from the agents' own
//! records, except where an agent's report itself is under test.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use agents::{
    Algo, Background, BackgroundConfig, CalibratedMarket, ExecutionAgent, FundamentalTrader, InformedTrader, KillSwitch,
    LatencyArbitrageur, Limits, MeanReversionTrader, MomentumTrader, PairsTrader, ParentOrder, Perception, Schedule,
    add_background, calibrated_market, calibrated_value,
};
use analysis::{FactsConfig, FactsReport, ImpactStats, Metaorder, check_targets, impact_check, sign_acf_fit, stylized_facts};
use lob::rng::Rng;
use lob::{Event, EventKind, InstrumentSpec, OrderId, Price, Qty, Side, Timestamp};
use sim::agents::Scripted;
use sim::{
    Agent, AgentConfig, AgentId, Ctx, ExchangeTiming, InstrumentConfig, InstrumentId, LatencyProfile, OrderSpec, Request,
    SignalId, SignalProcess, SimBuilder, Simulation,
};

const MS: i64 = 1_000_000;
const S: i64 = 1_000_000_000;

fn limit(i: InstrumentId, cid: u64, side: Side, price: Price, qty: Qty) -> Request {
    Request::New(OrderSpec::limit(i, cid, side, price, qty))
}

/// A smaller background for quick tests: 30 noise traders, 6 fundamental traders, one executor
/// with parents of at most 100 child orders (1 s).
fn small_background() -> BackgroundConfig {
    let mut cfg = BackgroundConfig::default();
    cfg.noise = 30;
    cfg.fundamental = 6;
    cfg.executors = 1;
    cfg.metaorders.max_children = 100;
    cfg
}

/// A random-walk value path with a move every 100 ms until `until` (finite, so a run can go quiet).
fn value_path(seed: u64, start: i64, until: Timestamp) -> SignalProcess {
    let mut rng = Rng::new(seed);
    let (mut t, mut v, mut points) = (0, start, Vec::new());
    while t + 100 * MS <= until {
        t += 100 * MS;
        v += if rng.chance(0.5) { 1 } else { -1 };
        points.push((t, v));
    }
    SignalProcess::Path { start, points }
}

/// Small background that stops at 3 s, run to 6 s so every message settles.
fn quiet_background_run(seed: u64) -> (Simulation, Background) {
    let spec = InstrumentSpec::example();
    let mut b = SimBuilder::new(seed);
    b.record_events(true);
    let i = b.add_instrument(InstrumentConfig::new(spec));
    let value = b.add_signal(value_path(seed, spec.reference_price, 3 * S));
    let mut cfg = small_background();
    cfg.stop_at = Some(3 * S);
    let bg = add_background(&mut b, i, value, &cfg);
    let mut sim = b.build().expect("valid market");
    sim.run_until(6 * S);
    (sim, bg)
}

/// Once the run is quiet: exchange, gateway and every agent's own records agree, and positions
/// and cash net to zero (no fees are configured).
fn check_consistency(sim: &Simulation) {
    assert_eq!(sim.pending_events(), 0, "run did not reach quiescence");
    assert!(sim.warnings().is_empty(), "{:?}", sim.warnings());
    for i in 0..sim.instrument_count() as InstrumentId {
        let snapshot = sim.engine(i).snapshot();
        let (mut pos, mut cash) = (0i64, 0i128);
        for a in 0..sim.agent_count() as AgentId {
            let l = sim.ledger(a, i);
            pos += l.position;
            cash += l.cash;
            let mine = sim.orders(a);
            assert_eq!(mine.position(i), l.position, "agent {a}: own view of position");
            assert_eq!(mine.cash(i), l.cash, "agent {a}: own view of cash");
            assert_eq!(l.fees, 0, "agent {a}: no fees configured");
            let resting = snapshot.iter().filter(|o| o.owner == a).count();
            assert_eq!(mine.live_count(i), resting, "agent {a}: own view of live orders");
            assert_eq!(sim.gateway_open_orders(a, i) as usize, resting, "agent {a}: gateway view");
        }
        assert_eq!((pos, cash), (0, 0), "positions and cash net to zero on instrument {i}");
    }
}

#[test]
fn background_is_reproducible_and_the_books_balance() {
    let (a, bg) = quiet_background_run(7);
    let (b, _) = quiet_background_run(7);
    let (c, _) = quiet_background_run(8);
    assert!(a.stats().trades > 100, "the market should trade: {:?}", a.stats());
    assert_eq!(a.digest(), b.digest());
    assert_eq!(a.events(0), b.events(0));
    assert_ne!(a.digest(), c.digest());
    check_consistency(&a);
    // Every noise and fundamental trader sent orders, and the executor finished parents.
    for &n in bg.noise.iter().chain(&bg.fundamental) {
        assert!(!trace(a.events(0), n).accepted.is_empty(), "agent {n} never had an order accepted");
    }
    let exec = a.agent::<ExecutionAgent>(bg.executors[0]).expect("executor");
    assert!(exec.reports.iter().filter(|r| r.filled > 0).count() >= 2, "{:?}", exec.reports);
}

/// Behaviour regression: any change to this digest means the background agents now behave
/// differently. Re-record it only for an intended change, and say why in the change log.
#[test]
fn golden_digest_of_the_small_background() {
    let (sim, _) = quiet_background_run(2026);
    assert_eq!((sim.stats().trades, sim.digest()), (GOLDEN_TRADES, GOLDEN_DIGEST), "digest {:#x}", sim.digest());
}

// Re-recorded 2026-11-15: a noise trader that draws a cancel with nothing to cancel now does
// nothing (it used to send a marketable order instead).
const GOLDEN_TRADES: u64 = 110;
const GOLDEN_DIGEST: u64 = 0x1a4c_6a9a_eacf_59ec;

/// What one agent did, rebuilt from the exchange's event log.
#[derive(Debug, Default)]
struct Trace {
    fills: usize,
    final_position: Qty,
    max_abs_position: Qty,
    max_live_orders: usize,
    max_order_qty: Qty,
    accepted: Vec<(Timestamp, Side, Option<Price>)>,
}

fn trace(events: &[Event], agent: AgentId) -> Trace {
    let mut t = Trace::default();
    let mut sides: HashMap<OrderId, Side> = HashMap::new();
    let mut live: HashSet<OrderId> = HashSet::new();
    for e in events {
        match e.kind {
            EventKind::Accepted { order_id, owner, side, price, qty, .. } if owner == agent => {
                sides.insert(order_id, side);
                live.insert(order_id);
                t.max_live_orders = t.max_live_orders.max(live.len());
                t.max_order_qty = t.max_order_qty.max(qty);
                t.accepted.push((e.ts, side, price));
            }
            EventKind::Fill { order_id, owner, qty, leaves, .. } if owner == agent => {
                t.final_position += if sides[&order_id] == Side::Buy { qty } else { -qty };
                t.max_abs_position = t.max_abs_position.max(t.final_position.abs());
                t.fills += 1;
                if leaves == 0 {
                    live.remove(&order_id);
                }
            }
            EventKind::Cancelled { order_id, owner, .. } | EventKind::Rejected { order_id, owner, .. } if owner == agent => {
                live.remove(&order_id);
            }
            _ => {}
        }
    }
    t
}

#[test]
fn strategies_respect_their_limits_at_the_exchange() {
    let tight = Limits { max_position: 20, max_order_qty: 5, max_open_orders: 3, capital: i128::MAX };
    let CalibratedMarket { mut builder, instrument: i, value: v, .. } = calibrated_market(11, 1.0, &BackgroundConfig::default());
    builder.record_events(true);
    let colo = || AgentConfig::new(LatencyProfile::colocated()).subscribe(i);
    // Each strategy wants more than its limits allow.
    let mut momentum = MomentumTrader::new(i);
    (momentum.limits, momentum.position_lots, momentum.clip_lots, momentum.entry_ticks, momentum.exit_ticks) = (tight, 40, 10, 0.25, 0.05);
    let mut reversion = MeanReversionTrader::new(i);
    (reversion.limits, reversion.position_lots, reversion.z_entry) = (tight, 40, 0.5);
    let mut informed = InformedTrader::new(i, v);
    (informed.limits, informed.threshold_ticks, informed.clip_lots) = (tight, 1, 10);
    let mut arb = LatencyArbitrageur::new(i, v);
    (arb.limits, arb.threshold_ticks, arb.clip_lots) = (tight, 1, 10);
    let mut fundamental = FundamentalTrader::new(i, v);
    (fundamental.limits, fundamental.max_lots, fundamental.lots_per_tick) = (tight, 100, 50.0);
    let ids = [
        builder.add_agent(momentum, colo()),
        builder.add_agent(reversion, colo()),
        builder.add_agent(informed, colo().with_signal(v)),
        builder.add_agent(arb, colo().with_signal(v)),
        builder.add_agent(fundamental, colo().with_signal(v)),
    ];
    let mut sim = builder.build().expect("valid market");
    sim.run_until(60 * S);
    let mut binding = 0;
    for &a in &ids {
        let t = trace(sim.events(i), a);
        assert!(t.fills > 0, "agent {a} never traded: {t:?}");
        assert!(t.max_abs_position <= tight.max_position, "agent {a}: position {}", t.max_abs_position);
        assert!(t.max_order_qty <= tight.max_order_qty, "agent {a}: order of {}", t.max_order_qty);
        assert!(t.max_live_orders <= tight.max_open_orders, "agent {a}: {} live orders", t.max_live_orders);
        assert_eq!(t.final_position, sim.ledger(a, i).position, "agent {a}: trace matches the ledger");
        binding += usize::from(t.max_abs_position == tight.max_position);
    }
    assert!(binding >= 1, "the position limit should bind for at least one strategy");
}

/// Computes `Limits::room` in its first callback (no orders, no position yet).
struct RoomProbe {
    limits: Vec<Limits>,
    rooms: Vec<(Qty, Qty)>,
}

impl Agent for RoomProbe {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        self.rooms = self.limits.iter().map(|l| (l.room(ctx, 0, Side::Buy, 10_000), l.room(ctx, 0, Side::Sell, 10_000))).collect();
    }
}

#[test]
fn room_applies_order_size_capital_and_lot_rounding() {
    let mut b = SimBuilder::new(1);
    b.add_instrument(InstrumentConfig::new(InstrumentSpec { lot_size: 10, ..InstrumentSpec::example() }));
    let base = Limits { max_position: 1_000, max_order_qty: 100, max_open_orders: 10, capital: i128::MAX };
    let probe = RoomProbe {
        limits: vec![
            base,
            Limits { capital: 500_000, ..base },                    // 50 units at $100.00
            Limits { capital: 455_000, ..base },                    // 45 units, rounded down to 40
            Limits { max_order_qty: 95, ..base },                   // rounded down to 90
            Limits { max_open_orders: 0, ..base },                  // no order may be sent
            Limits { max_position: 0, ..base },                     // flat only
        ],
        rooms: Vec::new(),
    };
    let a = b.add_agent(probe, AgentConfig::new(LatencyProfile::zero()));
    let mut sim = b.build().expect("valid");
    sim.run_until(1);
    let rooms = &sim.agent::<RoomProbe>(a).expect("probe").rooms;
    assert_eq!(rooms, &vec![(100, 100), (50, 50), (40, 40), (90, 90), (0, 0), (0, 0)]);
}

/// Sends five 10-lot bids through `send_limit` in its first callback.
struct Stacker {
    limits: Limits,
    sent: usize,
}

impl Agent for Stacker {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        for k in 0..5 {
            if agents::common::send_limit(ctx, 0, Side::Buy, 9_990 - k, 10, lob::TimeInForce::Gtc, &self.limits).is_some() {
                self.sent += 1;
            }
        }
    }
}

#[test]
fn open_orders_count_against_the_position_limit() {
    // Resting bids could all fill, so with a 20-share limit only two 10-share bids may rest. A
    // seller then sweeps the book: the position stops at the limit.
    let mut b = SimBuilder::new(2);
    b.timing(ExchangeTiming::zero());
    let i = b.add_instrument(InstrumentConfig::new(InstrumentSpec::example()));
    let limits = Limits { max_position: 20, max_order_qty: 10, max_open_orders: 10, capital: i128::MAX };
    let a = b.add_agent(Stacker { limits, sent: 0 }, AgentConfig::new(LatencyProfile::zero()));
    let sell = Request::New(OrderSpec::limit(i, 1, Side::Sell, 9_000, 100).with_tif(lob::TimeInForce::Ioc));
    b.add_agent(Scripted::new(vec![(MS, sell)]), AgentConfig::new(LatencyProfile::zero()));
    let mut sim = b.build().expect("valid");
    sim.run_until(2 * MS);
    assert_eq!(sim.agent::<Stacker>(a).expect("stacker").sent, 2);
    assert_eq!(sim.ledger(a, i).position, 20);
}

/// A market maker rests 10 @ 10,010 and 10 @ 9,990; value then jumps to `value_after` at 1 ms.
fn informed_on_stale_quotes(value_after: i64) -> (Simulation, AgentId) {
    let mut b = SimBuilder::new(3);
    b.timing(ExchangeTiming::zero());
    b.record_events(true);
    let i = b.add_instrument(InstrumentConfig::new(InstrumentSpec::example()));
    let s = b.add_signal(SignalProcess::Path { start: 10_000, points: vec![(MS, value_after)] });
    b.add_agent(
        Scripted::new(vec![(0, limit(i, 1, Side::Sell, 10_010, 10)), (0, limit(i, 2, Side::Buy, 9_990, 10))]),
        AgentConfig::new(LatencyProfile::zero()),
    );
    let mut informed = InformedTrader::new(i, s);
    (informed.clip_lots, informed.recheck) = (5, None);
    let a = b.add_agent(informed, AgentConfig::new(LatencyProfile::constant(0, 1_000, 1_000, 1_000, 1_000)).subscribe(i).with_signal(s));
    let mut sim = b.build().expect("valid");
    sim.run_until(10 * MS);
    (sim, a)
}

#[test]
fn informed_trader_takes_only_quotes_beyond_its_threshold() {
    // Value 10,050: the 10,010 ask is 40 ticks cheap. It buys 5 at the resting price, never
    // bidding more than value − 2 ticks.
    let (sim, a) = informed_on_stale_quotes(10_050);
    assert_eq!((sim.ledger(a, 0).position, sim.ledger(a, 0).cash), (5, -5 * 10_010));
    let t = trace(sim.events(0), a);
    assert_eq!(t.accepted.iter().map(|x| (x.1, x.2)).collect::<Vec<_>>(), vec![(Side::Buy, Some(10_048))]);
    // Value 9,950: it sells 5 into the 9,990 bid.
    let (sim, a) = informed_on_stale_quotes(9_950);
    assert_eq!((sim.ledger(a, 0).position, sim.ledger(a, 0).cash), (-5, 5 * 9_990));
    // Value 10,011: the ask is only 1 tick below value, inside the 2-tick threshold.
    let (sim, a) = informed_on_stale_quotes(10_011);
    assert_eq!(sim.ledger(a, 0).fills, 0);
    assert_eq!(sim.agent::<InformedTrader>(a).expect("informed").orders_sent, 0);
}

/// Rests one ask and pulls it as soon as it hears that value moved above it.
struct PullOnNews {
    instrument: InstrumentId,
    ask: Price,
    pulled: bool,
}

impl Agent for PullOnNews {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        ctx.send(limit(self.instrument, 1, Side::Sell, self.ask, 10));
    }

    fn on_signal(&mut self, ctx: &mut Ctx<'_>, _signal: SignalId, value: i64) {
        if value > self.ask && !self.pulled {
            self.pulled = true;
            ctx.send(Request::Cancel { instrument: self.instrument, client_id: 1 });
        }
    }
}

fn snipe_race(maker_ns: i64, arb_ns: i64) -> (Simulation, AgentId, AgentId) {
    let mut b = SimBuilder::new(13);
    b.timing(ExchangeTiming::zero());
    let i = b.add_instrument(InstrumentConfig::new(InstrumentSpec::example()));
    let s = b.add_signal(SignalProcess::Path { start: 10_000, points: vec![(MS, 10_050)] });
    let maker = b.add_agent(
        PullOnNews { instrument: i, ask: 10_010, pulled: false },
        AgentConfig::new(LatencyProfile::constant(0, maker_ns, 0, 0, maker_ns)).with_signal(s),
    );
    let mut arb = LatencyArbitrageur::new(i, s);
    arb.unwind = Schedule::Periodic { period_ns: 20 * MS };
    // The sniper's market data is as slow as its other links, so a slow sniper still sees the
    // stale quote when the news reaches it.
    let a = b.add_agent(arb, AgentConfig::new(LatencyProfile::constant(0, arb_ns, 0, arb_ns, arb_ns)).subscribe(i).with_signal(s));
    let mut sim = b.build().expect("valid");
    sim.run_until(30 * MS);
    (sim, maker, a)
}

#[test]
fn latency_arbitrageur_wins_only_when_faster_and_then_unwinds_at_value() {
    // Fast sniper (1 µs each way) against a slow maker (5 µs): it lifts 5 (its clip) of the stale
    // 10,010 ask, then on its 20 ms timer offers the 5 back at the new value, 10,050.
    let (sim, maker, arb) = snipe_race(5_000, 1_000);
    assert_eq!((sim.ledger(arb, 0).position, sim.ledger(maker, 0).position), (5, -5));
    let resting: Vec<(AgentId, Side, Option<Price>, Qty)> =
        sim.engine(0).snapshot().iter().map(|o| (o.owner, o.side, o.price, o.leaves)).collect();
    assert!(resting.contains(&(arb, Side::Sell, Some(10_050), 5)), "{resting:?}");
    // Slow sniper against a fast maker: the quote is gone before the sniper arrives.
    let (sim, maker, arb) = snipe_race(1_000, 5_000);
    assert_eq!((sim.ledger(arb, 0).position, sim.ledger(maker, 0).position), (0, 0));
    assert_eq!(sim.agent::<LatencyArbitrageur>(arb).expect("arb").snipes, 1, "it still tried");
}

#[test]
fn fundamental_trader_never_pays_more_than_value() {
    let spec = InstrumentSpec::example();
    let mut b = SimBuilder::new(21);
    b.record_events(true);
    let i = b.add_instrument(InstrumentConfig::new(spec));
    // Agents learn a signal only when it publishes, so the constant value is published once.
    let value = b.add_signal(SignalProcess::Path { start: 10_000, points: vec![(MS, 10_000)] });
    add_background(&mut b, i, value, &small_background());
    let mut f = FundamentalTrader::new(i, value);
    (f.lots_per_tick, f.max_lots, f.perception) = (5.0, 20, Perception { noise_sd: 0.0 });
    let a = b.add_agent(f, AgentConfig::new(LatencyProfile::colocated()).subscribe(i).with_signal(value));
    let mut sim = b.build().expect("valid");
    sim.run_until(30 * S);
    let t = trace(sim.events(i), a);
    assert!(t.fills > 0 && t.accepted.len() > 10, "{t:?}");
    for (ts, side, price) in &t.accepted {
        let p = price.expect("limit orders only");
        match side {
            Side::Buy => assert!(p <= 10_000, "bid {p} above value at {ts}"),
            Side::Sell => assert!(p >= 10_000, "offer {p} below value at {ts}"),
        }
    }
}

#[test]
fn fundamental_trader_never_bids_above_value_even_when_covering_a_short() {
    // Value 10,000; the market quotes 10,009 / 10,011. The trader sells 10 at the ask, a buyer
    // lifts it, and the ask side empties. With the price still above value but closer, its target
    // shrinks to −9, so it buys 1 back. Joining the 10,009 bid would pay above value, so it must
    // bid at its value estimate, 10,000.
    let mut b = SimBuilder::new(4);
    b.timing(ExchangeTiming::zero());
    b.record_events(true);
    let i = b.add_instrument(InstrumentConfig::new(InstrumentSpec::example()));
    let value = b.add_signal(SignalProcess::Path { start: 10_000, points: vec![(MS, 10_000)] });
    b.add_agent(
        Scripted::new(vec![(0, limit(i, 1, Side::Buy, 10_009, 10)), (0, limit(i, 2, Side::Sell, 10_011, 10))]),
        AgentConfig::new(LatencyProfile::zero()),
    );
    let mut f = FundamentalTrader::new(i, value);
    (f.schedule, f.lots_per_tick, f.max_lots) = (Schedule::Periodic { period_ns: 10 * MS }, 1.0, 10);
    let a = b.add_agent(f, AgentConfig::new(LatencyProfile::zero()).subscribe(i).with_signal(value));
    let lift = Request::New(OrderSpec::limit(i, 1, Side::Buy, 10_011, 20).with_tif(lob::TimeInForce::Ioc));
    b.add_agent(Scripted::new(vec![(15 * MS, lift)]), AgentConfig::new(LatencyProfile::zero()));
    let mut sim = b.build().expect("valid");
    sim.run_until(25 * MS);
    let t = trace(sim.events(i), a);
    assert_eq!(t.accepted.iter().map(|x| (x.1, x.2)).collect::<Vec<_>>(), vec![(Side::Sell, Some(10_011)), (Side::Buy, Some(10_000))]);
    assert_eq!(sim.ledger(a, i).position, -10);
}

#[test]
fn kill_switch_pulls_resting_quotes_when_it_trips() {
    // The sniper buys 5 stale shares at 10,010 and offers them back at the new value, 10,050. At
    // 25 ms the market collapses to 9,000 / 9,001; its next check (the 40 ms timer) marks the long
    // at 9,001, a loss of 5,045 > 100: it trips, and its resting offer must be cancelled.
    let mut b = SimBuilder::new(13);
    b.timing(ExchangeTiming::zero());
    let i = b.add_instrument(InstrumentConfig::new(InstrumentSpec::example()));
    let s = b.add_signal(SignalProcess::Path { start: 10_000, points: vec![(MS, 10_050)] });
    b.add_agent(
        PullOnNews { instrument: i, ask: 10_010, pulled: false },
        AgentConfig::new(LatencyProfile::constant(0, 5_000, 0, 0, 5_000)).with_signal(s),
    );
    let mut arb = LatencyArbitrageur::new(i, s);
    (arb.unwind, arb.kill) = (Schedule::Periodic { period_ns: 20 * MS }, KillSwitch::at_loss(100));
    let a = b.add_agent(arb, AgentConfig::new(LatencyProfile::constant(0, 1_000, 0, 1_000, 1_000)).subscribe(i).with_signal(s));
    b.add_agent(
        Scripted::new(vec![(25 * MS, limit(i, 1, Side::Buy, 9_000, 10)), (25 * MS, limit(i, 2, Side::Sell, 9_001, 10))]),
        AgentConfig::new(LatencyProfile::zero()),
    );
    let mut sim = b.build().expect("valid");
    sim.run_until(60 * MS);
    assert_eq!(sim.agent::<LatencyArbitrageur>(a).expect("arb").kill.tripped_at, Some(40 * MS));
    assert_eq!(sim.ledger(a, i).position, 5, "a kill switch stops trading; it does not flatten");
    assert!(sim.engine(i).snapshot().iter().all(|o| o.owner != a), "{:?}", sim.engine(i).snapshot());
}

#[test]
fn execution_algorithms_follow_their_schedules() {
    let CalibratedMarket { mut builder, instrument: i, .. } = calibrated_market(5, 1.0, &BackgroundConfig::default());
    builder.record_events(true);
    let parent = |side, qty, start: i64, secs: i64, slices, algo| ParentOrder { side, qty, start: start * S, end: (start + secs) * S, slices, algo };
    let parents = vec![
        parent(Side::Buy, 300, 10, 3, 30, Algo::Twap),
        parent(Side::Sell, 300, 15, 3, 30, Algo::Vwap { profile: vec![3.0, 1.0, 1.0, 3.0] }),
        parent(Side::Buy, 300, 20, 3, 30, Algo::AlmgrenChriss { urgency: 3.0 }),
        parent(Side::Sell, 1_000, 25, 10, 100, Algo::Pov { rate: 0.1 }),
    ];
    let a = builder.add_agent(ExecutionAgent::new(i, parents.clone()), AgentConfig::new(LatencyProfile::colocated()).subscribe(i));
    let mut sim = builder.build().expect("valid market");
    sim.run_until(40 * S);
    let reports = &sim.agent::<ExecutionAgent>(a).expect("executor").reports;
    assert_eq!(reports.len(), 4);
    for (r, p) in reports.iter().zip(&parents).take(3) {
        assert_eq!(r.filled, p.qty, "{:?} should complete: {r:?}", p.algo);
        assert!(r.shortfall().is_some() && r.children >= 10, "{r:?}");
    }
    // Share of a parent filled by a point in its window, from the exchange's fills.
    let sides: HashMap<OrderId, ()> = sim
        .events(i)
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Accepted { order_id, owner, .. } if owner == a => Some((order_id, ())),
            _ => None,
        })
        .collect();
    let filled_between = |from: Timestamp, to: Timestamp| -> Qty {
        sim.events(i)
            .iter()
            .filter(|e| e.ts >= from && e.ts < to)
            .filter_map(|e| match e.kind {
                EventKind::Fill { order_id, owner, qty, .. } if owner == a && sides.contains_key(&order_id) => Some(qty),
                _ => None,
            })
            .sum()
    };
    let share = |k: usize, at_ms: i64| filled_between(parents[k].start, parents[k].start + at_ms * MS) as f64 / parents[k].qty as f64;
    let twap_half = share(0, 1_500);
    assert!((0.4..=0.6).contains(&twap_half), "TWAP done at half time: {twap_half}");
    // VWAP on a U-shaped profile: 22/60 of the slices' weight falls in the first 8 of 30 slices.
    let vwap_quarter = share(1, 750);
    assert!((0.30..=0.45).contains(&vwap_quarter), "VWAP done at a quarter: {vwap_quarter}");
    // Almgren–Chriss with κ = 3: 1 − sinh(1.5)/sinh(3) = 0.79 of the parent after 15 of 30 slices.
    let ac_half = share(2, 1_500);
    assert!(ac_half > 0.65, "Almgren–Chriss done at half time: {ac_half}");
    // POV at 10%: its fills are about a tenth of all volume traded in its window.
    let volume: Qty = sim
        .events(i)
        .iter()
        .filter(|e| e.ts >= parents[3].start && e.ts < parents[3].end)
        .filter_map(|e| match e.kind {
            EventKind::Trade { qty, .. } => Some(qty),
            _ => None,
        })
        .sum();
    let participation = reports[3].filled as f64 / volume as f64;
    assert!((0.06..=0.11).contains(&participation), "POV participation {participation} of {volume}");
}

#[test]
fn execution_never_overfills_when_slices_outpace_the_round_trip() {
    // A remote executor slicing every 100 µs: dozens of children are in flight before the first
    // fill comes back, so the schedule must count them or the parent overfills many times over.
    let CalibratedMarket { mut builder, instrument: i, .. } = calibrated_market(6, 1.0, &BackgroundConfig::default());
    let parent = ParentOrder { side: Side::Buy, qty: 100, start: 5 * S, end: 5 * S + 10 * MS, slices: 100, algo: Algo::Twap };
    let a = builder.add_agent(ExecutionAgent::new(i, vec![parent]), AgentConfig::new(LatencyProfile::remote()).subscribe(i));
    let mut sim = builder.build().expect("valid market");
    sim.run_until(6 * S);
    let r = &sim.agent::<ExecutionAgent>(a).expect("executor").reports[0];
    assert_eq!((r.filled, sim.ledger(a, i).position), (100, 100), "{r:?}");
}

#[test]
fn pairs_trader_trades_both_legs_of_a_tracking_pair() {
    let spec = InstrumentSpec::example();
    let mut b = SimBuilder::new(17);
    b.record_events(true);
    let (ia, ib) = (b.add_instrument(InstrumentConfig::new(spec)), b.add_instrument(InstrumentConfig::new(spec)));
    let va = b.add_signal(calibrated_value(spec.reference_price, spec.tick_size, 1.0));
    let vb = b.add_signal(SignalProcess::Tracking { leader: va, offset: 0, step: 1, reversion: 0.05, mean_interval_ns: 2e8 });
    add_background(&mut b, ia, va, &small_background());
    add_background(&mut b, ib, vb, &small_background());
    let pairs = PairsTrader::new(ia, ib);
    let (size, clip) = (pairs.size_lots, pairs.clip_lots);
    let p = b.add_agent(pairs, AgentConfig::new(LatencyProfile::colocated()).subscribe(ia).subscribe(ib));
    let mut sim = b.build().expect("valid market");
    sim.run_until(120 * S);
    let (la, lb) = (sim.ledger(p, ia), sim.ledger(p, ib));
    assert!(la.taker_qty > 0 && lb.taker_qty > 0, "both legs traded: {la:?} {lb:?}");
    assert!(la.position.abs() <= size && lb.position.abs() <= size);
    assert!((la.position + lb.position).abs() <= clip, "legs out of balance: {} vs {}", la.position, lb.position);
    // Along the whole path, from the exchange's fills on both books: the legs are opposite
    // (β = 1), so the net position stays within one leg's size even when a leg misses, while the
    // spread position itself reaches full size at some point.
    let mut fills: Vec<(Timestamp, usize, Qty)> = Vec::new();
    for (k, i) in [ia, ib].into_iter().enumerate() {
        let mut sides: HashMap<OrderId, Side> = HashMap::new();
        for e in sim.events(i) {
            match e.kind {
                EventKind::Accepted { order_id, owner, side, .. } if owner == p => {
                    sides.insert(order_id, side);
                }
                EventKind::Fill { order_id, owner, qty, .. } if owner == p => {
                    fills.push((e.ts, k, if sides[&order_id] == Side::Buy { qty } else { -qty }));
                }
                _ => {}
            }
        }
    }
    fills.sort_by_key(|f| f.0);
    let (mut pos, mut max_net, mut max_b) = ([0 as Qty; 2], 0 as Qty, 0 as Qty);
    for (k, f) in fills.iter().enumerate() {
        pos[f.1] += f.2;
        if fills.get(k + 1).is_none_or(|next| next.0 != f.0) {
            max_net = max_net.max((pos[0] + pos[1]).abs());
            max_b = max_b.max(pos[1].abs());
        }
    }
    assert_eq!(pos, [la.position, lb.position], "fill path matches the ledgers");
    assert_eq!(max_b, size, "the spread position reached full size");
    assert!(max_net <= size, "legs moved together: net position reached {max_net}");
    for i in [ia, ib] {
        let net: Qty = (0..sim.agent_count() as AgentId).map(|a| sim.ledger(a, i).position).sum();
        assert_eq!(net, 0, "positions net to zero on instrument {i}");
    }
}

#[test]
fn kill_switch_stops_trading_and_pulls_quotes() {
    let CalibratedMarket { mut builder, instrument: i, .. } = calibrated_market(9, 1.0, &BackgroundConfig::default());
    builder.record_events(true);
    let mut reversion = MeanReversionTrader::new(i);
    (reversion.kill, reversion.z_entry) = (KillSwitch::at_loss(1), 0.5);
    let a = builder.add_agent(reversion, AgentConfig::new(LatencyProfile::colocated()).subscribe(i));
    let mut sim = builder.build().expect("valid market");
    sim.run_until(60 * S);
    let tripped = sim.agent::<MeanReversionTrader>(a).expect("trader").kill.tripped_at.expect("a 1-cent loss limit trips");
    let t = trace(sim.events(i), a);
    assert!(t.fills > 0);
    assert!(t.accepted.iter().all(|x| x.0 <= tripped + 5 * MS), "orders after the trip at {tripped}");
    assert!(sim.engine(i).snapshot().iter().all(|o| o.owner != a), "quotes left resting after the trip");
}

fn facts_of(seed: u64, minutes: i64) -> FactsReport {
    let end = minutes * 60 * S;
    let CalibratedMarket { mut builder, instrument: i, background, .. } = calibrated_market(seed, 1.0, &BackgroundConfig::default());
    builder.record_events(true);
    let mut sim = builder.build().expect("valid market");
    sim.run_until(end);
    let metaorders: Vec<Metaorder> = background
        .executors
        .iter()
        .flat_map(|&a| sim.agent::<ExecutionAgent>(a).expect("executor").reports.clone())
        .filter(|r| r.filled > 0)
        .map(|r| Metaorder { side: r.side, qty: r.filled, start: r.start, end: r.end })
        .collect();
    stylized_facts(sim.events(i), &metaorders, &FactsConfig::standard(60 * S, end, 1))
}

/// Three simulated minutes: the structural facts hold even in a short window. The exponents need
/// longer runs (see `phase_2_acceptance`).
#[test]
fn stylized_facts_quick() {
    let f = facts_of(2026, 3);
    let checks = check_targets(&f);
    for c in checks.iter().filter(|c| ["cancellations", "executed share", "spread"].contains(&c.name)) {
        assert!(c.pass, "{} failed: {}", c.name, c.measured);
    }
    assert!(f.signs.positive_share_1_100 >= 0.95, "{}", f.signs.positive_share_1_100);
    assert!(f.signs.gamma().is_some_and(|g| g > 0.0), "{:?}", f.signs.fit);
    assert!(f.impact.fitted_bins().all(|b| b.mean_impact_ticks > 0.0), "{}", f.impact);
}

/// The phase-2 exit criterion (docs/STYLIZED_FACTS.md): 8 seeds × 10 simulated minutes. Every
/// seed meets the structural targets; the impact and sign-memory exponents, pooled over the
/// seeds, fall inside their bands; and at least 6 of 8 seeds meet all five targets on their own.
/// Slow (about a minute in release on 4 threads):
/// `cargo test --release -p agents --test agents -- --ignored`.
#[test]
#[ignore]
fn phase_2_acceptance() {
    let seeds: Vec<u64> = (2026..2034).collect();
    let next = AtomicU64::new(0);
    let done = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed) as usize;
                    if k >= seeds.len() {
                        break;
                    }
                    let f = facts_of(seeds[k], 10);
                    done.lock().expect("no worker panicked").push(f);
                }
            });
        }
    });
    let reports = done.into_inner().expect("no worker panicked");
    let mut all_five = 0;
    for f in &reports {
        let checks = check_targets(f);
        for c in checks.iter().take(3) {
            assert!(c.pass, "{} failed: {}", c.name, c.measured);
        }
        all_five += usize::from(checks.iter().all(|c| c.pass));
    }
    assert!(all_five >= 6, "only {all_five}/8 seeds met all five targets");
    let pooled = ImpactStats::from_samples(reports.iter().flat_map(|f| f.impact.samples.iter().copied()).collect(), 2.0, 30);
    let c = impact_check(&pooled);
    assert!(c.pass, "pooled impact: {}\n{pooled}", c.measured);
    let lags = reports[0].signs.acf.len();
    let mean_acf: Vec<f64> = (0..lags).map(|l| reports.iter().map(|f| f.signs.acf[l]).sum::<f64>() / reports.len() as f64).collect();
    let gamma = -sign_acf_fit(&mean_acf, (5, 500)).expect("fit").slope;
    assert!((0.3..=0.8).contains(&gamma), "pooled γ {gamma}");
}
