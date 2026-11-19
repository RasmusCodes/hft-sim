//! Simulation-kernel tests. Timings are worked out by hand from the configured delays; see
//! docs/SIMULATION.md for the path of a message.

use lob::{
    CancelReason, EventKind, InstrumentSpec, L3Entry, Liquidity, Phase, Price, Qty, RejectReason,
    RequestKind, Side, TimeInForce, Timestamp,
};
use sim::agents::{Scripted, ZeroIntelligence};
use sim::{
    Agent, AgentConfig, AgentId, BuildError, ClientOrderId, Ctx, ExchangeTiming, FeeSchedule,
    FeedGroupId, FeedMode, GatewayReject, InstrumentConfig, InstrumentId, LatencyModel,
    LatencyProfile, OrderSpec, ReportKind, Request, RequestType, RiskLimits, SessionEvent, SignalId,
    SignalProcess, SimBuilder, Simulation, Throttle,
};

fn spec() -> InstrumentSpec {
    InstrumentSpec {
        tick_size: 1,
        lot_size: 1,
        min_price: 1,
        max_price: 1_000_000,
        max_order_qty: 1_000_000,
        reference_price: 10_000,
    }
}

fn limit(i: InstrumentId, cid: ClientOrderId, side: Side, price: Price, qty: Qty) -> Request {
    Request::New(OrderSpec::limit(i, cid, side, price, qty))
}

fn ioc(i: InstrumentId, cid: ClientOrderId, side: Side, price: Price, qty: Qty) -> Request {
    Request::New(OrderSpec::limit(i, cid, side, price, qty).with_tif(TimeInForce::Ioc))
}

fn cancel(i: InstrumentId, cid: ClientOrderId) -> Request {
    Request::Cancel { instrument: i, client_id: cid }
}

fn kinds(sim: &Simulation, agent: AgentId) -> Vec<ReportKind> {
    sim.agent::<Scripted>(agent).expect("scripted agent").reports.iter().map(|(_, r)| r.kind).collect()
}

/// Order path: compute 500 → out 1,000 → gateway 7,000 → hop 2,000 → engine 3,000 + 100/event
/// → in 1,500 (reports) or feed 4,000 (market data).
#[test]
fn round_trip_is_the_sum_of_every_configured_delay() {
    let mut b = SimBuilder::new(1);
    b.timing(ExchangeTiming { gateway_ns: 7_000, hop_ns: 2_000, engine_base_ns: 3_000, engine_per_event_ns: 100 });
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    let profile = LatencyProfile::constant(500, 1_000, 1_500, 4_000, 0);
    let maker = b.add_agent(
        Scripted::new(vec![(10_000, limit(i, 1, Side::Sell, 10_001, 5))]),
        AgentConfig::new(profile.clone()),
    );
    let watcher = b.add_agent(Scripted::silent(), AgentConfig::new(profile).subscribe(i));
    let mut sim = b.build().expect("valid");
    sim.run_until(1_000_000);

    // Departs 10,500; gateway 11,500 → 18,500; engine at 20,500; two events (Accepted,
    // BookAdd) → service 3,200 → results leave at 23,700.
    let m = sim.agent::<Scripted>(maker).expect("maker");
    assert_eq!(m.reports.len(), 1);
    let (arrival, report) = m.reports[0];
    assert_eq!(report.exchange_ts, 20_500, "engine processing time");
    assert_eq!(arrival, 23_700 + 1_500, "report arrival");
    assert!(matches!(report.kind, ReportKind::Accepted { qty: 5, .. }));
    assert_eq!(report.order_id, Some(1), "gateway-assigned id matches the engine's");

    let w = sim.agent::<Scripted>(watcher).expect("watcher");
    assert_eq!(w.market_data.len(), 1);
    let (arrival, instrument, events) = &w.market_data[0];
    assert_eq!((*arrival, *instrument), (23_700 + 4_000, i));
    assert!(matches!(events.as_slice(), [e] if matches!(e.kind, EventKind::BookAdd { qty: 5, .. }) && e.ts == 20_500));
    assert!(sim.warnings().is_empty(), "{:?}", sim.warnings());
}

#[test]
fn engine_service_time_grows_with_the_events_produced() {
    let mut b = SimBuilder::new(2);
    b.timing(ExchangeTiming { gateway_ns: 0, hop_ns: 0, engine_base_ns: 0, engine_per_event_ns: 1_000 });
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    let z = LatencyProfile::zero();
    let script = (1..=3).map(|k| (1_000, limit(i, k, Side::Sell, 10_000 + k as i64, 1))).collect();
    b.add_agent(Scripted::new(script), AgentConfig::new(z.clone()));
    let taker = b.add_agent(Scripted::new(vec![(100_000, ioc(i, 1, Side::Buy, 10_003, 3))]), AgentConfig::new(z));
    let mut sim = b.build().expect("valid");
    sim.run_until(1_000_000);
    // The sweep produces Accepted + 3 x (Trade, maker Fill, taker Fill) = 10 events.
    let t = sim.agent::<Scripted>(taker).expect("taker");
    assert!(t.reports.iter().all(|(arrival, r)| *arrival == 100_000 + 10_000 && r.exchange_ts == 100_000));
    assert_eq!(t.reports.len(), 4, "Accepted + 3 fills");
}

#[test]
fn simultaneous_orders_queue_at_the_engine() {
    let mut b = SimBuilder::new(3);
    b.timing(ExchangeTiming { gateway_ns: 0, hop_ns: 0, engine_base_ns: 1_000, engine_per_event_ns: 0 });
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    let agents: Vec<AgentId> = (0..5)
        .map(|k| {
            b.add_agent(
                Scripted::new(vec![(10_000, limit(i, 1, Side::Buy, 9_990 + k, 1))]),
                AgentConfig::new(LatencyProfile::zero()),
            )
        })
        .collect();
    let mut sim = b.build().expect("valid");
    sim.run_until(1_000_000);
    for (k, &a) in agents.iter().enumerate() {
        let (arrival, r) = sim.agent::<Scripted>(a).expect("agent").reports[0];
        assert_eq!(r.exchange_ts, 10_000 + 1_000 * k as i64, "agent {k} waited for {k} services");
        assert_eq!(arrival, 11_000 + 1_000 * k as i64);
    }
    assert_eq!(sim.stats().max_engine_queue, 4);
}

#[test]
fn each_gateway_session_handles_one_message_at_a_time() {
    let mut b = SimBuilder::new(14);
    b.timing(ExchangeTiming { gateway_ns: 7_000, hop_ns: 0, engine_base_ns: 0, engine_per_event_ns: 0 });
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    let burst = (1..=3).map(|k| (1_000, limit(i, k, Side::Buy, 9_990 + k as i64, 1))).collect();
    let busy = b.add_agent(Scripted::new(burst), AgentConfig::new(LatencyProfile::zero()));
    let other = b.add_agent(
        Scripted::new(vec![(1_000, limit(i, 1, Side::Sell, 10_010, 1))]),
        AgentConfig::new(LatencyProfile::zero()),
    );
    let mut sim = b.build().expect("valid");
    sim.run_until(1_000_000);
    let processed = |a: AgentId| -> Vec<Timestamp> {
        sim.agent::<Scripted>(a).expect("agent").reports.iter().map(|(_, r)| r.exchange_ts).collect()
    };
    // Three messages queue behind each other in their own session ...
    assert_eq!(processed(busy), vec![8_000, 15_000, 22_000]);
    // ... while another participant's session is unaffected.
    assert_eq!(processed(other), vec![8_000]);
}

#[test]
fn a_jittery_link_never_reorders_one_agents_messages() {
    let mut b = SimBuilder::new(4);
    b.record_events(true);
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    let profile = LatencyProfile {
        compute: LatencyModel::log_normal(100, 5_000.0, 1.5),
        order_out: LatencyModel::Pareto { base_ns: 1_000, scale_ns: 20_000.0, alpha: 0.9 },
        ..LatencyProfile::zero()
    };
    let script = (1..=300).map(|k| (1_000, limit(i, k, Side::Buy, 9_000 + k as i64, 1))).collect();
    b.add_agent(Scripted::new(script), AgentConfig::new(profile));
    let mut sim = b.build().expect("valid");
    sim.run_until(10_000_000_000);
    let tags: Vec<u64> = sim
        .events(i)
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Accepted { client_tag, .. } => Some(client_tag),
            _ => None,
        })
        .collect();
    assert_eq!(tags, (1..=300).collect::<Vec<u64>>());
}

fn race(maker_out: i64, taker_out: i64, engine_ns: i64) -> (Simulation, AgentId, AgentId) {
    let mut b = SimBuilder::new(5);
    b.timing(ExchangeTiming { gateway_ns: 0, hop_ns: 0, engine_base_ns: engine_ns, engine_per_event_ns: 0 });
    b.record_events(true);
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    let maker = b.add_agent(
        Scripted::new(vec![(1_000, limit(i, 1, Side::Sell, 10_000, 10)), (1_000_000, cancel(i, 1))]),
        AgentConfig::new(LatencyProfile::constant(0, maker_out, 0, 0, 0)),
    );
    let taker = b.add_agent(
        Scripted::new(vec![(1_000_000, ioc(i, 1, Side::Buy, 10_000, 10))]),
        AgentConfig::new(LatencyProfile::constant(0, taker_out, 0, 0, 0)),
    );
    let mut sim = b.build().expect("valid");
    sim.run_until(10_000_000);
    (sim, maker, taker)
}

#[test]
fn cancel_fill_race_is_decided_by_latency() {
    // Maker's cancel arrives first: the taker finds nothing.
    let (sim, maker, taker) = race(10_000, 50_000, 0);
    assert!(matches!(
        kinds(&sim, maker)[..],
        [ReportKind::Accepted { .. }, ReportKind::Cancelled { reason: CancelReason::Requested, qty: 10 }]
    ));
    assert!(matches!(
        kinds(&sim, taker)[..],
        [ReportKind::Accepted { .. }, ReportKind::Cancelled { reason: CancelReason::IocRemainder, qty: 10 }]
    ));
    assert_eq!(sim.ledger(maker, 0).position, 0);

    // Taker arrives first: the maker is filled, and its cancel is too late (the gateway
    // already knows the order is gone).
    let (sim, maker, taker) = race(50_000, 10_000, 0);
    let mk = kinds(&sim, maker);
    assert!(matches!(mk[1], ReportKind::Fill { qty: 10, leaves: 0, liquidity: Liquidity::Maker, .. }));
    assert_eq!(mk[2], ReportKind::RequestRejected { request: RequestKind::Cancel, reason: RejectReason::UnknownOrder });
    let late = sim.agent::<Scripted>(maker).expect("maker").reports[2].1;
    assert_eq!(late.exchange_ts, 1_050_000, "answered by the gateway");
    assert!(matches!(kinds(&sim, taker)[1], ReportKind::Fill { liquidity: Liquidity::Taker, .. }));
    assert_eq!((sim.ledger(maker, 0).position, sim.ledger(taker, 0).position), (-10, 10));
    // The engine never saw that cancel; the gateway log has it.
    assert!(!sim.events(0).iter().any(|e| matches!(e.kind, EventKind::RequestRejected { .. })));
    let log = sim.gateway_log();
    assert_eq!(log.len(), 1);
    assert_eq!((log[0].ts, log[0].agent, log[0].request), (1_050_000, maker, cancel(0, 1)));
    assert_eq!(log[0].report, ReportKind::RequestRejected { request: RequestKind::Cancel, reason: RejectReason::UnknownOrder });

    // Same race with a slow engine: the cancel passes the gateway before the fill is
    // published, waits in the engine queue, and the engine reports it too late.
    let (sim, maker, _) = race(50_000, 10_000, 100_000);
    let mk = kinds(&sim, maker);
    assert!(matches!(mk[1], ReportKind::Fill { qty: 10, .. }));
    assert_eq!(mk[2], ReportKind::RequestRejected { request: RequestKind::Cancel, reason: RejectReason::UnknownOrder });
    let late = sim.agent::<Scripted>(maker).expect("maker").reports[2].1;
    assert_eq!(late.exchange_ts, 1_110_000, "answered by the engine after its queue cleared");
    assert!(sim.warnings().is_empty(), "{:?}", sim.warnings());
    // This time the refusal is the engine's, in its event log, and the gateway log is empty.
    assert!(sim.gateway_log().is_empty());
    assert!(sim.events(0).iter().any(|e| matches!(e.kind, EventKind::RequestRejected { request: RequestKind::Cancel, .. })));
}

/// Records its view of the best ask at fixed times.
struct Peeker {
    instrument: InstrumentId,
    at: Vec<Timestamp>,
    seen: Vec<(Timestamp, Option<(Price, Qty)>)>,
}

impl Agent for Peeker {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        for (k, &t) in self.at.iter().enumerate() {
            ctx.set_timer(t, k as u64);
        }
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _token: u64) {
        let ask = ctx.book(self.instrument).and_then(|b| b.best_ask());
        self.seen.push((ctx.now(), ask));
    }
}

#[test]
fn agents_see_the_book_only_after_their_feed_latency() {
    let mut b = SimBuilder::new(6);
    b.timing(ExchangeTiming::zero());
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    b.add_agent(Scripted::new(vec![(1_000, limit(i, 1, Side::Sell, 10_010, 5))]), AgentConfig::new(LatencyProfile::zero()));
    let peeker = b.add_agent(
        Peeker { instrument: i, at: vec![50_000, 100_999, 101_000, 101_001, 200_000], seen: Vec::new() },
        AgentConfig::new(LatencyProfile::constant(0, 0, 0, 100_000, 0)).subscribe(i),
    );
    let mut sim = b.build().expect("valid");
    sim.run_until(50_000);
    assert_eq!(sim.engine(i).best_ask(), Some((10_010, 5)), "the exchange has it since t = 1,000");
    sim.run_until(1_000_000);
    let p = sim.agent::<Peeker>(peeker).expect("peeker");
    // The packet arrives at 101,000 exactly. The timer due at the same nanosecond was scheduled
    // earlier (at t = 0), and same-time events run in scheduling order, so it still sees nothing.
    assert_eq!(
        p.seen,
        vec![
            (50_000, None),
            (100_999, None),
            (101_000, None),
            (101_001, Some((10_010, 5))),
            (200_000, Some((10_010, 5)))
        ]
    );
}

#[test]
fn gateway_enforces_risk_limits_and_throttle() {
    let mut b = SimBuilder::new(7);
    b.timing(ExchangeTiming::zero());
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    let limits = RiskLimits {
        max_order_qty: 100,
        max_open_orders: 2,
        max_position: 150,
        throttle: Some(Throttle { burst: 5, per_second: 1 }),
    };
    let script = vec![
        (1_000, limit(i, 1, Side::Buy, 9_990, 200)), // above max order qty
        (1_000, limit(i, 2, Side::Buy, 9_990, 100)), // accepted
        (1_000, limit(i, 3, Side::Buy, 9_980, 60)),  // could reach +160 > 150
        (1_000, limit(i, 4, Side::Sell, 10_020, 50)), // accepted
        (1_000, limit(i, 5, Side::Sell, 10_030, 10)), // third open order
        (1_000, cancel(i, 2)),                        // sixth message in a burst of five
    ];
    let a = b.add_agent(Scripted::new(script), AgentConfig::new(LatencyProfile::zero()).with_limits(limits));
    let mut sim = b.build().expect("valid");
    sim.run_until(1_000_000);
    let outcome = |cid: ClientOrderId| -> Vec<ReportKind> {
        sim.agent::<Scripted>(a).expect("agent").reports.iter().filter(|(_, r)| r.client_id == cid).map(|(_, r)| r.kind).collect()
    };
    let gw = |request, reason| ReportKind::GatewayRejected { request, reason };
    assert_eq!(outcome(1), vec![gw(RequestType::New, GatewayReject::MaxOrderQty)]);
    // Order 2 was accepted; the cancel of it (sixth message) was throttled. The gateway answers
    // at once while the engine is still busy, so the throttle reply even arrives first.
    assert!(matches!(
        outcome(2)[..],
        [ReportKind::GatewayRejected { request: RequestType::Cancel, reason: GatewayReject::Throttled }, ReportKind::Accepted { .. }]
    ));
    assert_eq!(outcome(3), vec![gw(RequestType::New, GatewayReject::MaxPosition)]);
    assert!(matches!(outcome(4)[..], [ReportKind::Accepted { .. }]));
    assert_eq!(outcome(5), vec![gw(RequestType::New, GatewayReject::MaxOpenOrders)]);
    assert_eq!(sim.stats().gateway_rejects, 4);
    assert_eq!(sim.engine(i).next_order_id(), 3, "the engine only saw the two accepted orders");
    assert_eq!(sim.gateway_open_orders(a, i), 2);
}

#[test]
fn an_order_can_be_cancelled_before_its_acknowledgement_arrives() {
    let mut b = SimBuilder::new(8);
    b.timing(ExchangeTiming::zero());
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    let a = b.add_agent(
        Scripted::new(vec![(1_000, limit(i, 7, Side::Buy, 9_990, 10)), (1_000, cancel(i, 7))]),
        AgentConfig::new(LatencyProfile::constant(0, 0, 1_000_000, 0, 0)),
    );
    let mut sim = b.build().expect("valid");
    sim.run_until(10_000_000);
    let r = &sim.agent::<Scripted>(a).expect("agent").reports;
    assert_eq!(r.len(), 2);
    assert!(r.iter().all(|(arrival, rep)| *arrival == 1_001_000 && rep.client_id == 7 && rep.order_id == Some(1)));
    assert!(matches!(r[1].1.kind, ReportKind::Cancelled { reason: CancelReason::Requested, qty: 10 }));
    assert_eq!(sim.engine(i).live_orders(), 0);
    assert_eq!(sim.orders(a).live().count(), 0);
}

fn auction_run(seed: u64) -> (Simulation, Timestamp) {
    let mut b = SimBuilder::new(seed);
    b.timing(ExchangeTiming::zero()).record_events(true);
    let i = b.add_instrument(InstrumentConfig::new(spec()).with_phase(Phase::Auction));
    b.add_agent(Scripted::new(vec![(1_000, limit(i, 1, Side::Buy, 10_005, 10))]), AgentConfig::new(LatencyProfile::zero()));
    b.add_agent(Scripted::new(vec![(1_000, limit(i, 1, Side::Sell, 9_995, 10))]), AgentConfig::new(LatencyProfile::zero()));
    b.add_session_event(SessionEvent::between(i, Phase::Continuous, 1_000_000, 2_000_000));
    let mut sim = b.build().expect("valid");
    let t = sim.session_schedule()[0].0;
    sim.run_until(3_000_000);
    (sim, t)
}

#[test]
fn random_auction_end_is_reproducible_and_uncrosses_on_time() {
    let (sim, t) = auction_run(9);
    assert!((1_000_000..=2_000_000).contains(&t));
    let trade = sim.events(0).iter().find(|e| matches!(e.kind, EventKind::Trade { .. })).expect("uncross trade");
    assert_eq!(trade.ts, t, "uncross happens at the drawn time");
    // B = S = 10 on 9,995..=10,005 with zero surplus: closest to the reference price 10,000.
    assert!(matches!(trade.kind, EventKind::Trade { price: 10_000, qty: 10, aggressor: None, .. }));
    assert_eq!(auction_run(9).1, t, "same seed, same time");
    let times: std::collections::BTreeSet<Timestamp> = (1..=10).map(|s| auction_run(s).1).collect();
    assert!(times.len() >= 9, "different seeds draw different times: {times:?}");
}

#[test]
fn invalid_scheduled_phase_change_is_a_warning_not_a_crash() {
    let mut b = SimBuilder::new(10);
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    b.add_session_event(SessionEvent::at(i, Phase::Continuous, 5_000));
    let mut sim = b.build().expect("valid");
    sim.run_until(1_000_000);
    assert_eq!(sim.warnings().len(), 1);
    assert!(sim.warnings()[0].contains("not allowed"));
}

const ZI_FEES: FeeSchedule = FeeSchedule { maker: -20, taker: 30, auction: 0 };

fn zi_market(seed: u64, agents: usize, instruments: u32, stop_at: Timestamp, extra_silent: bool) -> Simulation {
    let mut b = SimBuilder::new(seed);
    b.record_events(true);
    for _ in 0..instruments {
        b.add_instrument(InstrumentConfig::new(spec()).with_fees(ZI_FEES));
    }
    for a in 0..agents {
        let i = (a as u32) % instruments;
        let latency = match a % 3 {
            0 => LatencyProfile::colocated(),
            1 => LatencyProfile::proximity(),
            _ => LatencyProfile::remote(),
        };
        let mut zi = ZeroIntelligence::new(i);
        zi.stop_at = Some(stop_at);
        let cfg = if a % 4 == 3 { AgentConfig::new(latency) } else { AgentConfig::new(latency).subscribe(i) };
        b.add_agent(zi, cfg);
    }
    if extra_silent {
        b.add_agent(Scripted::silent(), AgentConfig::new(LatencyProfile::zero()));
    }
    b.build().expect("valid")
}

#[test]
fn same_seed_same_run_and_streams_are_independent() {
    let mut a = zi_market(5, 12, 1, 400_000_000, false);
    let mut b = zi_market(5, 12, 1, 400_000_000, false);
    a.run_until(500_000_000);
    b.run_until(500_000_000);
    assert!(a.stats().trades > 100, "the market should trade: {:?}", a.stats());
    assert_eq!(a.digest(), b.digest());
    assert_eq!(a.events(0), b.events(0));

    let mut c = zi_market(6, 12, 1, 400_000_000, false);
    c.run_until(500_000_000);
    assert_ne!(a.digest(), c.digest());

    // An extra agent that never acts changes nothing on the exchange: every other agent and
    // link keeps its own random stream.
    let mut d = zi_market(5, 12, 1, 400_000_000, true);
    d.run_until(500_000_000);
    assert_eq!(a.events(0), d.events(0));
}

/// The `examples/demo.rs` market. Its digest was recorded before the scheduler and feed refactor;
/// any change to it means simulated behaviour changed. (Rust's derived `Hash` feeds the digest,
/// so a compiler upgrade could, in principle, require re-recording it.)
#[test]
fn golden_digest_of_the_demo_market() {
    let mut b = SimBuilder::new(2026);
    let fees = FeeSchedule { maker: -20, taker: 30, auction: 0 };
    let i = b.add_instrument(InstrumentConfig::new(InstrumentSpec::example()).with_fees(fees));
    for a in 0..30 {
        let latency = match a % 3 {
            0 => LatencyProfile::colocated(),
            1 => LatencyProfile::proximity(),
            _ => LatencyProfile::remote(),
        };
        b.add_agent(ZeroIntelligence::new(i), AgentConfig::new(latency).subscribe(i));
    }
    let mut sim = b.build().expect("valid");
    sim.run_until(1_000_000_000);
    assert_eq!(sim.stats().trades, 4_642);
    assert_eq!(sim.digest(), 0x964f_31ce_1846_c416);
}

/// Twelve timer-driven traders in three locations with constant feed latencies, either on
/// private feeds (one delivery and one book copy each) or on three shared snapshot groups.
fn zi_constant_feeds(shared: bool) -> Simulation {
    let mut b = SimBuilder::new(21);
    b.record_events(true);
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    let feed_ns = [2_000i64, 60_000, 4_000_000];
    let groups: Vec<FeedGroupId> = feed_ns.iter().map(|&ns| b.add_feed_group(LatencyModel::constant(ns))).collect();
    for a in 0..12 {
        let class = a % 3;
        let mut latency = match class {
            0 => LatencyProfile::colocated(),
            1 => LatencyProfile::proximity(),
            _ => LatencyProfile::remote(),
        };
        latency.feed = LatencyModel::constant(feed_ns[class]);
        let mut zi = ZeroIntelligence::new(i);
        zi.stop_at = Some(300_000_000);
        let cfg = AgentConfig::new(latency);
        let cfg = if shared { cfg.subscribe_group(i, groups[class], FeedMode::Snapshot) } else { cfg.subscribe(i) };
        b.add_agent(zi, cfg);
    }
    let mut sim = b.build().expect("valid");
    sim.run_until(1_000_000_000);
    sim
}

#[test]
fn shared_snapshot_groups_reproduce_private_feeds_exactly() {
    let private = zi_constant_feeds(false);
    let shared = zi_constant_feeds(true);
    assert!(private.stats().trades > 100);
    // The exchange saw exactly the same messages at exactly the same times.
    assert_eq!(private.events(0), shared.events(0));
    // ... while the shared market delivered each packet 3 times instead of 12, with no callbacks.
    let (p, s) = (private.stats(), shared.stats());
    assert_eq!(p.feed_packets_delivered, 4 * s.feed_packets_delivered);
    assert_eq!(p.market_data_callbacks, p.feed_packets_delivered);
    assert_eq!(s.market_data_callbacks, 0);
    check_quiescent_consistency(&shared, FeeSchedule::default());
}

#[test]
fn shared_group_members_get_each_packet_at_the_same_moment() {
    let mut b = SimBuilder::new(22);
    b.timing(ExchangeTiming::zero());
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    let g = b.add_feed_group(LatencyModel::log_normal(10_000, 5_000.0, 1.0));
    let script = (1..=20).map(|k| (k * 1_000, limit(i, k as u64, Side::Buy, 9_000 + k, 1))).collect();
    b.add_agent(Scripted::new(script), AgentConfig::new(LatencyProfile::zero()));
    let member = |mode| AgentConfig::new(LatencyProfile::zero()).subscribe_group(i, g, mode);
    let m1 = b.add_agent(Scripted::silent(), member(FeedMode::Callback));
    let m2 = b.add_agent(Scripted::silent(), member(FeedMode::Callback));
    let quiet = b.add_agent(Scripted::silent(), member(FeedMode::Snapshot));
    let mut sim = b.build().expect("valid");
    sim.run_until(10_000_000);
    let arrivals = |a: AgentId| -> Vec<Timestamp> {
        sim.agent::<Scripted>(a).expect("agent").market_data.iter().map(|(t, _, _)| *t).collect()
    };
    let t1 = arrivals(m1);
    assert_eq!(t1.len(), 20);
    assert_eq!(t1, arrivals(m2), "one draw per packet, common to the group");
    assert!(t1.windows(2).all(|w| w[0] <= w[1]), "group link never reorders");
    let delays: std::collections::BTreeSet<Timestamp> = t1.iter().enumerate().map(|(k, t)| t - (k as i64 + 1) * 1_000).collect();
    assert!(delays.len() > 10, "delays are random: {delays:?}");
    assert!(sim.agent::<Scripted>(quiet).expect("agent").market_data.is_empty(), "snapshot members get no callbacks");
    assert_eq!(sim.book_view(quiet, i).map(|v| v.order_count()), Some(20), "but their view is current");
    assert_eq!(sim.stats().feed_packets_delivered, 20);
    assert_eq!(sim.stats().market_data_callbacks, 40);
}

#[test]
fn a_snapshot_member_sees_the_book_only_after_its_groups_latency() {
    let mut b = SimBuilder::new(23);
    b.timing(ExchangeTiming::zero());
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    let g = b.add_feed_group(LatencyModel::constant(100_000));
    b.add_agent(Scripted::new(vec![(1_000, limit(i, 1, Side::Sell, 10_010, 5))]), AgentConfig::new(LatencyProfile::zero()));
    let peeker = b.add_agent(
        Peeker { instrument: i, at: vec![100_999, 101_000, 101_001], seen: Vec::new() },
        AgentConfig::new(LatencyProfile::zero()).subscribe_group(i, g, FeedMode::Snapshot),
    );
    let mut sim = b.build().expect("valid");
    sim.run_until(1_000_000);
    let p = sim.agent::<Peeker>(peeker).expect("peeker");
    assert_eq!(p.seen, vec![(100_999, None), (101_000, None), (101_001, Some((10_010, 5)))]);
}

#[test]
fn an_unknown_feed_group_is_a_build_error() {
    let mut b = SimBuilder::new(24);
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    b.add_agent(Scripted::silent(), AgentConfig::new(LatencyProfile::zero()).subscribe_group(i, 7, FeedMode::Snapshot));
    assert!(matches!(b.build(), Err(BuildError::UnknownFeedGroup(7))));
}

/// After all activity has died down, every view must agree with the exchange.
fn check_quiescent_consistency(sim: &Simulation, fees: FeeSchedule) {
    assert_eq!(sim.pending_events(), 0, "run did not reach quiescence");
    assert!(sim.warnings().is_empty(), "{:?}", sim.warnings());
    for i in 0..sim.instrument_count() as InstrumentId {
        let (mut pos, mut cash) = (0i64, 0i128);
        let snapshot = sim.engine(i).snapshot();
        for a in 0..sim.agent_count() as AgentId {
            let l = sim.ledger(a, i);
            pos += l.position;
            cash += l.cash;
            let mine = sim.orders(a);
            assert_eq!(mine.position(i), l.position, "agent {a}: own view of position");
            assert_eq!(mine.cash(i), l.cash, "agent {a}: own view of cash");
            assert_eq!(
                l.fees,
                i128::from(l.maker_qty) * i128::from(fees.maker) + i128::from(l.taker_qty) * i128::from(fees.taker)
            );
            let resting = snapshot.iter().filter(|o| o.owner == a).count();
            assert_eq!(mine.live_count(i), resting, "agent {a}: own view of live orders");
            assert_eq!(sim.gateway_open_orders(a, i) as usize, resting, "agent {a}: gateway view");
            if let Some(view) = sim.book_view(a, i) {
                let truth: Vec<L3Entry> = snapshot
                    .iter()
                    .map(|o| L3Entry { order_id: o.order_id, side: o.side, price: o.price, qty: o.leaves })
                    .collect();
                assert_eq!(view.l3(), truth, "agent {a}: book view");
            }
        }
        assert_eq!(pos, 0, "positions net to zero on instrument {i}");
        assert_eq!(cash, 0, "cash nets to zero on instrument {i}");
    }
}

#[test]
fn exchange_and_agent_views_agree_once_messages_settle() {
    let mut sim = zi_market(11, 12, 1, 400_000_000, false);
    sim.run_until(2_000_000_000);
    assert!(sim.stats().trades > 100);
    check_quiescent_consistency(&sim, ZI_FEES);
}

#[test]
fn two_instruments_stay_separate_and_consistent() {
    let mut sim = zi_market(12, 12, 2, 300_000_000, false);
    sim.run_until(2_000_000_000);
    assert!(sim.stats().trades > 100);
    for i in 0..2u32 {
        assert!(sim.engine(i).snapshot().iter().all(|o| o.owner % 2 == i), "orders on the right instrument");
    }
    check_quiescent_consistency(&sim, ZI_FEES);
}

/// Quotes one ask at start and pulls it when the signal says the value moved above it.
struct Quoter {
    instrument: InstrumentId,
    ask: Price,
    pulled: bool,
}

impl Agent for Quoter {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        ctx.send(limit(self.instrument, 1, Side::Sell, self.ask, 10));
    }

    fn on_signal(&mut self, ctx: &mut Ctx<'_>, _signal: SignalId, value: i64) {
        if value > self.ask && !self.pulled {
            self.pulled = true;
            ctx.send(cancel(self.instrument, 1));
        }
    }
}

/// Lifts stale asks below the new value as soon as it hears the signal.
struct Sniper {
    instrument: InstrumentId,
}

impl Agent for Sniper {
    fn on_signal(&mut self, ctx: &mut Ctx<'_>, _signal: SignalId, value: i64) {
        let cid = ctx.next_client_id();
        ctx.send(ioc(self.instrument, cid, Side::Buy, value - 10, 10));
    }
}

fn latency_race(quoter_ns: i64, sniper_ns: i64) -> (Simulation, AgentId, AgentId) {
    let mut b = SimBuilder::new(13);
    b.timing(ExchangeTiming::zero());
    let i = b.add_instrument(InstrumentConfig::new(spec()));
    let s = b.add_signal(SignalProcess::Path { start: 10_000, points: vec![(1_000_000, 10_050)] });
    let q = b.add_agent(
        Quoter { instrument: i, ask: 10_010, pulled: false },
        AgentConfig::new(LatencyProfile::constant(0, quoter_ns, 0, 0, quoter_ns)).with_signal(s),
    );
    let k = b.add_agent(
        Sniper { instrument: i },
        AgentConfig::new(LatencyProfile::constant(0, sniper_ns, 0, 0, sniper_ns)).with_signal(s),
    );
    let mut sim = b.build().expect("valid");
    sim.run_until(10_000_000);
    (sim, q, k)
}

#[test]
fn latency_decides_who_wins_a_stale_quote_race() {
    // Fast market maker: hears the news after 1 µs and cancels 1 µs later, before the sniper
    // (5 + 5 µs) arrives.
    let (sim, quoter, sniper) = latency_race(1_000, 5_000);
    assert_eq!(sim.ledger(quoter, 0).position, 0);
    assert_eq!(sim.ledger(sniper, 0).position, 0);
    assert_eq!(sim.signal_value(0), 10_050);

    // Slow market maker: the sniper lifts the stale 10,010 ask; marked at the new value
    // 10,050 the maker lost 40 x 10 and the sniper made it.
    let (sim, quoter, sniper) = latency_race(5_000, 1_000);
    let (q, s) = (sim.ledger(quoter, 0), sim.ledger(sniper, 0));
    assert_eq!((q.position, s.position), (-10, 10));
    assert_eq!(q.gross_value(10_050), -400);
    assert_eq!(s.gross_value(10_050), 400);
    assert!(sim.warnings().is_empty(), "{:?}", sim.warnings());
}
