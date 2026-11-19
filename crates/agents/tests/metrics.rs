//! Phase-4 metrics on real simulation runs, checked against what the exchange itself recorded.

use agents::mm::{LadderParams, MarketMaker, QuotingStrategy};
use agents::{BackgroundConfig, InformedTrader, LatencyArbitrageur, Perception, Schedule, add_background, calibrated_value};
use analysis::BookPath;
use analysis::metrics::{FeeRates, OrderIndex, latency_races, pnl_decomposition, pnl_decomposition_window, race_messages};
use lob::{InstrumentSpec, Price, RejectReason, RequestKind, Side};
use sim::{
    Agent, AgentConfig, AgentId, Ctx, ExchangeTiming, FEE_SCALE, FeeSchedule, InstrumentConfig, InstrumentId, LatencyProfile, OrderSpec,
    ReportKind, Request, SignalId, SignalProcess, SimBuilder,
};

const S: i64 = 1_000_000_000;

#[test]
fn pnl_decomposition_matches_every_ledger_exactly() {
    // The calibrated background plus a market maker and informed traders, with maker rebates and
    // taker fees. For every agent and three horizons, the decomposition (from the event log and
    // the book path) must equal the exchange ledger's own PnL to the last fee unit.
    let fees = FeeSchedule { maker: -20, taker: 30, auction: 0 };
    let spec = InstrumentSpec::example();
    let mut b = SimBuilder::new(77);
    b.record_events(true);
    let i = b.add_instrument(InstrumentConfig::new(spec).with_fees(fees));
    let value = b.add_signal(calibrated_value(spec.reference_price, spec.tick_size, 1.0));
    add_background(&mut b, i, value, &BackgroundConfig::default());
    b.add_agent(MarketMaker::new(i, QuotingStrategy::ladder(7, &LadderParams::default())), AgentConfig::new(LatencyProfile::colocated()).subscribe(i));
    for _ in 0..3 {
        let mut t = InformedTrader::new(i, value);
        t.perception = Perception { noise_sd: 0.5 };
        b.add_agent(t, AgentConfig::new(LatencyProfile::proximity()).subscribe(i).with_signal(value));
    }
    let mut sim = b.build().expect("valid market");
    let end = 20 * S;
    sim.run_until(end);
    let path = BookPath::from_events(sim.events(i));
    let m2 = path.mid2_at(end).expect("two-sided book");
    let rates = FeeRates { maker: fees.maker, taker: fees.taker, auction: fees.auction, scale: FEE_SCALE };
    let mut traded = 0;
    let mut carried_in = 0;
    let from = 5 * S;
    for a in 0..sim.agent_count() as AgentId {
        let l = sim.ledger(a, i);
        // Twice the ledger's net PnL in fee units, marked at the final mid.
        let ledger_total = (2 * l.cash + i128::from(l.position) * i128::from(m2)) * i128::from(FEE_SCALE) - 2 * l.fees;
        for h in [50_000_000, S, 15 * S] {
            let d = pnl_decomposition(sim.events(i), &path, a, end, h, rates).expect("decomposition");
            assert_eq!(d.total, ledger_total, "agent {a}, horizon {h}");
            assert_eq!(d.spread_capture + d.adverse_selection + d.inventory - d.fees, d.total);
            assert_eq!((d.position, d.volume), (l.position, l.bought + l.sold), "agent {a}");
            // Over the window (5 s, 20 s]: the ledger's PnL less the PnL marked at 5 s.
            let before = pnl_decomposition(sim.events(i), &path, a, from, h, rates).expect("marked at the window start");
            let w = pnl_decomposition_window(sim.events(i), &path, a, from, end, h, rates).expect("window");
            assert_eq!(w.total, ledger_total - before.total, "agent {a}, horizon {h}: window");
            assert_eq!(w.spread_capture + w.adverse_selection + w.inventory + w.carried - w.fees, w.total);
            assert_eq!((w.opening_position, w.position, w.volume + before.volume), (before.position, l.position, l.bought + l.sold), "agent {a}");
            carried_in += usize::from(w.carried != 0);
        }
        traded += usize::from(l.fills > 0);
    }
    assert!(traded > 100, "most agents traded ({traded})");
    assert!(carried_in > 100, "positions carried into the window are exercised ({carried_in})");
}

/// Rests one 10-share ask and pulls it as soon as it hears that value moved above it.
struct PullOnNews {
    instrument: InstrumentId,
    ask: Price,
    pulled: bool,
}

impl Agent for PullOnNews {
    fn on_start(&mut self, ctx: &mut Ctx<'_>) {
        ctx.send(Request::New(OrderSpec::limit(self.instrument, 1, Side::Sell, self.ask, 10)));
    }

    fn on_signal(&mut self, ctx: &mut Ctx<'_>, _signal: SignalId, value: i64) {
        if value > self.ask && !self.pulled {
            self.pulled = true;
            ctx.send(Request::Cancel { instrument: self.instrument, client_id: 1 });
        }
    }
}

#[test]
fn a_sniping_race_is_measured_including_the_cancel_the_gateway_refused() {
    // Value jumps from 10,000 to 10,050 at 1 ms. The fast sniper (1 µs each way) lifts the whole
    // stale 10,010 ask at 1.002 ms. The maker (5 µs) sends its cancel too late; at 1.010 ms the
    // gateway, which already saw the fill, refuses it without the engine. A slower sniper (8 µs)
    // still sees the ask when the news reaches it and fails at the engine at 1.016 ms.
    let mut b = SimBuilder::new(13);
    b.timing(ExchangeTiming::zero());
    b.record_events(true);
    let i = b.add_instrument(InstrumentConfig::new(InstrumentSpec::example()));
    let s = b.add_signal(SignalProcess::Path { start: 10_000, points: vec![(1_000_000, 10_050)] });
    let maker = b.add_agent(PullOnNews { instrument: i, ask: 10_010, pulled: false }, AgentConfig::new(LatencyProfile::constant(0, 5_000, 0, 5_000, 5_000)).with_signal(s));
    let mut snipers = Vec::new();
    for ns in [1_000, 8_000] {
        let mut arb = LatencyArbitrageur::new(i, s);
        (arb.unwind, arb.clip_lots) = (Schedule::Periodic { period_ns: 50_000_000 }, 10);
        snipers.push(b.add_agent(arb, AgentConfig::new(LatencyProfile::constant(0, ns, 0, ns, ns)).subscribe(i).with_signal(s)));
    }
    let mut sim = b.build().expect("valid");
    sim.run_until(20_000_000);
    let refused: Vec<(i64, AgentId, u64)> = sim
        .gateway_log()
        .iter()
        .filter(|g| g.report == ReportKind::RequestRejected { request: RequestKind::Cancel, reason: RejectReason::UnknownOrder })
        .filter_map(|g| match g.request {
            Request::Cancel { client_id, .. } => Some((g.ts, g.agent, client_id)),
            _ => None,
        })
        .collect();
    assert_eq!(refused.len(), 1, "the maker's late cancel, refused at the gateway: {:?}", sim.gateway_log());
    let ix = OrderIndex::from_events(sim.events(i));
    let msgs = race_messages(sim.events(i), &ix, &refused, 0, 20_000_000);
    let r = latency_races(&msgs, 500_000);
    assert_eq!((r.races, r.won_by_take), (1, 1));
    assert_eq!(r.wins.get(&snipers[0]), Some(&1));
    assert_eq!(r.losses.get(&maker), Some(&1), "the refused cancel counts as the maker's loss");
    assert_eq!(r.losses.get(&snipers[1]), Some(&1));
    // First success at 1.002 ms (the take), first failure at 1.010 ms (the refused cancel).
    assert_eq!(r.mean_duration_ns, 8_000.0);
}
