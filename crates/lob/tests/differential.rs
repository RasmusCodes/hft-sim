//! Differential fuzzing: random command streams through the optimised engine and the naive
//! reference model, with every harness cross-check after every command, plus a coverage
//! floor so a fuzzer change cannot silently stop exercising a rule.
//!
//! Scale with environment variables, e.g. (PowerShell)
//! `$env:LOB_FUZZ_SEEDS=2000; $env:LOB_FUZZ_STEPS=5000; cargo test --release --test differential`

mod common;

use std::collections::BTreeMap;

use common::{Tracker, check_step};
use lob::EventKind as K;
use lob::workload::{FlowConfig, FlowGen, FuzzConfig, FuzzGen};
use lob::{
    Command, Digest, EngineError, Event, InstrumentSpec, MatchingEngine, MboBook, Phase,
    ReferenceEngine, TimeInForce, Timestamp,
};

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Counts of the rule paths a run exercised.
#[derive(Default)]
struct Coverage(BTreeMap<String, u64>);

impl Coverage {
    fn bump(&mut self, key: impl Into<String>) {
        *self.0.entry(key.into()).or_default() += 1;
    }

    fn observe(&mut self, cmd: &Command, result: &Result<(), EngineError>, events: &[Event]) {
        self.bump(match cmd {
            Command::New(o) => match (o.price, o.tif) {
                (None, _) => "cmd: new market",
                (Some(_), TimeInForce::Gtc) => "cmd: new limit gtc",
                (Some(_), TimeInForce::Ioc) => "cmd: new limit ioc",
                (Some(_), TimeInForce::Fok) => "cmd: new limit fok",
            },
            Command::Cancel { .. } => "cmd: cancel",
            Command::Modify { .. } => "cmd: modify",
            Command::CancelAll { .. } => "cmd: cancel all",
            Command::SetPhase { .. } => "cmd: set phase",
        });
        match result {
            Err(EngineError::NonMonotonicTime { .. }) => self.bump("error: NonMonotonicTime"),
            Err(EngineError::InvalidPhaseTransition { .. }) => self.bump("error: InvalidPhaseTransition"),
            Ok(()) => {}
        }
        for e in events {
            match e.kind {
                K::Rejected { reason, .. } => self.bump(format!("reject: {reason:?}")),
                K::RequestRejected { request, reason, .. } => self.bump(format!("{request:?} rejected: {reason:?}")),
                K::Cancelled { reason, .. } => self.bump(format!("cancelled: {reason:?}")),
                K::Replaced { priority_kept: true, .. } => self.bump("replaced: priority kept"),
                K::Replaced { priority_kept: false, .. } => self.bump("replaced: priority lost"),
                K::Trade { aggressor: Some(_), .. } => self.bump("trade: continuous"),
                K::Trade { aggressor: None, .. } => self.bump("trade: auction"),
                K::AuctionUncross { price: Some(_), .. } => self.bump("uncross: traded"),
                K::AuctionUncross { price: None, .. } => self.bump("uncross: no trade"),
                K::PhaseChanged { phase } => self.bump(format!("phase -> {phase:?}")),
                _ => {}
            }
        }
    }

    fn merge(&mut self, other: Coverage) {
        for (k, v) in other.0 {
            *self.0.entry(k).or_default() += v;
        }
    }

    fn require(&self, keys: &[&str]) {
        println!("{:<44} {:>10}", "path", "count");
        for (k, v) in &self.0 {
            println!("{k:<44} {v:>10}");
        }
        let missing: Vec<&&str> = keys.iter().filter(|k| !self.0.contains_key(**k)).collect();
        assert!(missing.is_empty(), "fuzzer never exercised: {missing:?}");
    }
}

/// Run one seed; panics with a reproduction context on the first disagreement.
fn run_seed(spec: InstrumentSpec, cfg: &FuzzConfig, seed: u64, steps: usize) -> Coverage {
    let mut eng = MatchingEngine::new(spec, Phase::Continuous).expect("valid spec");
    let mut refm = ReferenceEngine::new(spec, Phase::Continuous).expect("valid spec");
    let mut mbo = MboBook::new();
    let mut tracker = Tracker::default();
    let mut fuzz = FuzzGen::new(seed, spec, cfg.clone());
    let mut coverage = Coverage::default();
    let mut history: Vec<(Timestamp, Command)> = Vec::with_capacity(steps);
    let (mut ev_e, mut ev_r) = (Vec::new(), Vec::new());
    for step in 0..steps {
        let live = refm.snapshot();
        let (ts, cmd) = fuzz.next(&live);
        history.push((ts, cmd));
        ev_e.clear();
        ev_r.clear();
        let re = eng.process(ts, &cmd, &mut ev_e);
        let rr = refm.process(ts, &cmd, &mut ev_r);
        let context = || {
            let from = history.len().saturating_sub(20);
            format!("seed {seed}, step {step}, command {cmd:?}\nlast commands: {:#?}", &history[from..])
        };
        assert_eq!(re, rr, "result differs\n{}", context());
        if ev_e != ev_r {
            panic!("events differ\n{}\nengine:    {ev_e:#?}\nreference: {ev_r:#?}", context());
        }
        let compare_auction = eng.phase() == Phase::Auction && step % 4 == 0;
        if let Err(e) = check_step(&eng, &refm, &mut mbo, &mut tracker, &ev_e, compare_auction) {
            panic!("{e}\n{}", context());
        }
        coverage.observe(&cmd, &re, &ev_e);
    }
    coverage
}

const REQUIRED: &[&str] = &[
    "trade: continuous",
    "trade: auction",
    "uncross: traded",
    "uncross: no trade",
    "replaced: priority kept",
    "replaced: priority lost",
    "cancelled: Requested",
    "cancelled: MassCancel",
    "cancelled: IocRemainder",
    "cancelled: FokUnfilled",
    "cancelled: SelfTrade",
    "cancelled: ModifiedBelowFilled",
    "cancelled: AuctionMarketRemainder",
    "reject: MarketClosed",
    "reject: TradingHalted",
    "reject: InvalidQty",
    "reject: LotSize",
    "reject: MaxQty",
    "reject: TickSize",
    "reject: PriceBounds",
    "reject: TifNotAllowedInPhase",
    "reject: InvalidPostOnly",
    "reject: PostOnlyWouldCross",
    "Cancel rejected: UnknownOrder",
    "Cancel rejected: NotOwner",
    "Modify rejected: UnknownOrder",
    "Modify rejected: NotOwner",
    "Modify rejected: MarketOrderNotModifiable",
    "Modify rejected: PostOnlyWouldCross",
    "Modify rejected: TradingHalted",
    "error: NonMonotonicTime",
    "error: InvalidPhaseTransition",
    "phase -> Auction",
    "phase -> Halted",
    "phase -> Closed",
    "phase -> Continuous",
];

/// Tick 5, lot 10: exercises tick/lot validation with non-unit sizes.
fn fuzz_spec() -> InstrumentSpec {
    InstrumentSpec {
        tick_size: 5,
        lot_size: 10,
        min_price: 5,
        max_price: 2_000,
        max_order_qty: 600,
        reference_price: 1_000,
    }
}

#[test]
fn differential_fuzz_default_mix() {
    let seeds = env_or("LOB_FUZZ_SEEDS", 120);
    let steps = env_or("LOB_FUZZ_STEPS", 2_000) as usize;
    let mut cov = Coverage::default();
    for seed in 0..seeds {
        cov.merge(run_seed(fuzz_spec(), &FuzzConfig::default(), seed, steps));
    }
    cov.require(REQUIRED);
}

#[test]
fn differential_fuzz_unit_tick_many_owners_large_orders() {
    let spec = InstrumentSpec {
        tick_size: 1,
        lot_size: 1,
        min_price: 1,
        max_price: 400,
        max_order_qty: 50,
        reference_price: 200,
    };
    let cfg = FuzzConfig { owners: 8, band_ticks: 4, max_lots: 20, ..FuzzConfig::default() };
    let seeds = env_or("LOB_FUZZ_SEEDS", 60);
    let steps = env_or("LOB_FUZZ_STEPS", 2_000) as usize;
    let mut cov = Coverage::default();
    for seed in 0..seeds {
        cov.merge(run_seed(spec, &cfg, 10_000 + seed, steps));
    }
    cov.require(&["trade: continuous", "trade: auction", "cancelled: SelfTrade", "reject: MaxQty"]);
}

#[test]
fn differential_fuzz_auction_heavy() {
    let cfg = FuzzConfig { w_phase: 15, w_cancel: 10, ..FuzzConfig::default() };
    let seeds = env_or("LOB_FUZZ_SEEDS", 60);
    let steps = env_or("LOB_FUZZ_STEPS", 2_000) as usize;
    let mut cov = Coverage::default();
    for seed in 0..seeds {
        cov.merge(run_seed(fuzz_spec(), &cfg, 20_000 + seed, steps));
    }
    cov.require(&["trade: auction", "uncross: traded", "uncross: no trade", "cancelled: AuctionMarketRemainder"]);
}

#[test]
fn differential_on_benchmark_flow() {
    let spec = InstrumentSpec::example();
    let stream = FlowGen::generate(7, spec, FlowConfig { target_live: 300, ..FlowConfig::default() }, 30_000);
    let mut eng = MatchingEngine::new(spec, Phase::Continuous).expect("valid spec");
    let mut refm = ReferenceEngine::new(spec, Phase::Continuous).expect("valid spec");
    let mut mbo = MboBook::new();
    let mut tracker = Tracker::default();
    let (mut a, mut b) = (Vec::new(), Vec::new());
    let mut cov = Coverage::default();
    for (step, (ts, cmd)) in stream.iter().enumerate() {
        a.clear();
        b.clear();
        let ra = eng.process(*ts, cmd, &mut a);
        let rb = refm.process(*ts, cmd, &mut b);
        assert_eq!(ra, rb, "step {step}");
        assert_eq!(a, b, "step {step}: {cmd:?}");
        check_step(&eng, &refm, &mut mbo, &mut tracker, &a, false).unwrap_or_else(|e| panic!("step {step}: {e}"));
        cov.observe(cmd, &ra, &a);
    }
    cov.require(&["trade: continuous", "replaced: priority kept", "replaced: priority lost", "cancelled: Requested"]);
}

#[test]
fn replay_is_deterministic_and_independent_of_preallocation() {
    let spec = fuzz_spec();
    let mut eng = MatchingEngine::new(spec, Phase::Continuous).expect("valid spec");
    let mut fuzz = FuzzGen::new(99, spec, FuzzConfig::default());
    let mut log = Vec::new();
    let mut first = Digest::new();
    let mut out = Vec::new();
    for _ in 0..20_000 {
        let live = eng.snapshot();
        let (ts, cmd) = fuzz.next(&live);
        out.clear();
        let _ = eng.process(ts, &cmd, &mut out);
        out.iter().for_each(|e| first.update(e));
        log.push((ts, cmd));
    }
    for capacity in [1usize, 64, 100_000] {
        let mut replay = MatchingEngine::with_capacity(spec, Phase::Continuous, capacity).expect("valid spec");
        let mut digest = Digest::new();
        for (ts, cmd) in &log {
            out.clear();
            let _ = replay.process(*ts, cmd, &mut out);
            out.iter().for_each(|e| digest.update(e));
        }
        assert_eq!(first, digest, "capacity {capacity}");
    }
}
