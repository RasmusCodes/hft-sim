//! Run one simulated market from command-line style flags and report every phase-4 metric as JSON,
//! with a manifest that reproduces the run. The `hftsim` binary (`src/main.rs`) and the Python
//! bindings (`python/native`, module `hftsim._native`) both call [`run`] or [`run_with_tables`],
//! so the same flags give the same run, and the same digest, either way.
//!
//! ```text
//! hftsim run --seed 2026 --secs 300 --mm 7 --informed 10 --background zi
//! hftsim help
//! ```
//!
//! Every flag takes one value; `--mm` may repeat (one market maker per occurrence). The output's
//! `manifest.args` are the flags exactly as given: re-running them reproduces `manifest.digest`.
//! THE RESULTS ARE SIMULATION RESULTS (docs/MARKET_MAKING.md §7, docs/METRICS.md).

#![forbid(unsafe_code)]

pub mod export;
mod json;

use std::time::Instant;

use agents::mm::{LadderParams, MarketMaker, QuotingStrategy};
use agents::{BackgroundConfig, ExecutionAgent, InformedTrader, LatencyArbitrageur, Limits, Perception, add_background, calibrated_value};
use analysis::metrics::{
    FeeRates, OrderIndex, PnlDecomposition, QueueBucket, execution_spreads, fill_probability_by_queue, latency_races, ofi_regression,
    pnl_decomposition_window, race_messages, realized_volatility, roll_spread,
};
use analysis::{BookPath, FactsConfig, Metaorder, check_targets, stylized_facts};
use json::J;
use lob::{InstrumentSpec, RejectReason, RequestKind};
use sim::{AgentConfig, AgentId, FEE_SCALE, FeeSchedule, FeedMode, InstrumentConfig, LatencyProfile, ReportKind, Request, SimBuilder};

const S: i64 = 1_000_000_000;

/// Command-line help: every flag with its default.
pub const USAGE: &str = "hftsim run [--flag value ...]

Market:      --seed 2026  --secs 300  --warmup-secs 60  --background default|zi  --value-ms 1000
             --maker-fee 0  --taker-fee 0   (fee units; 10,000 = one cent per share)
Informed:    --informed 0  --informed-noise 0.5  --informed-threshold 1  --informed-clip 2
             --informed-latency proximity
Arbitrage:   --arbs 0  --arb-threshold 2  --arb-clip 5  --arb-latency colocated
Makers:      --mm STEP (repeat for several; steps 1-10)  --mm-latency colocated  --mm-unit-lots 5
             --mm-max-inventory 10  --mm-gamma 3.5  --mm-sigma 1  --mm-a 130  --mm-k 5
             --mm-skew 0.1  --mm-half-spread 0.5  --mm-as-horizon 0.03
Metrics:     --horizons-ms 50,1000,15000,60000,300000  --race-window-us 500
             --queue-edges 0,1,50,100,200,500  --ofi-ms 1000,10000
Output:      --out FILE (default: standard output)";

#[derive(Clone, Debug)]
struct Config {
    seed: u64,
    secs: i64,
    warmup_secs: i64,
    zero_intelligence: bool,
    value_ms: i64,
    fees: FeeSchedule,
    informed: usize,
    informed_noise: f64,
    informed_threshold: i64,
    informed_clip: i64,
    informed_location: usize,
    arbs: usize,
    arb_threshold: i64,
    arb_clip: i64,
    arb_location: usize,
    makers: Vec<usize>,
    mm_location: usize,
    mm_unit_lots: i64,
    ladder: LadderParams,
    horizons_ms: Vec<i64>,
    race_window_us: i64,
    queue_edges: Vec<i64>,
    ofi_ms: Vec<i64>,
    out: Option<String>,
}

fn location(name: &str) -> Result<usize, String> {
    match name {
        "colocated" => Ok(0),
        "proximity" => Ok(1),
        "remote" => Ok(2),
        other => Err(format!("unknown latency location `{other}` (colocated, proximity, remote)")),
    }
}

fn profile(loc: usize) -> LatencyProfile {
    agents::location_profiles()[loc].clone()
}

fn list(v: &str) -> Result<Vec<i64>, String> {
    v.split(',').map(|x| x.trim().parse::<i64>().map_err(|e| format!("bad number `{x}`: {e}"))).collect()
}

fn parse(args: &[String]) -> Result<Config, String> {
    let mut c = Config {
        seed: 2026,
        secs: 300,
        warmup_secs: 60,
        zero_intelligence: false,
        value_ms: 1000,
        fees: FeeSchedule::default(),
        informed: 0,
        informed_noise: 0.5,
        informed_threshold: 1,
        informed_clip: 2,
        informed_location: 1,
        arbs: 0,
        arb_threshold: 2,
        arb_clip: 5,
        arb_location: 0,
        makers: Vec::new(),
        mm_location: 0,
        mm_unit_lots: 5,
        ladder: LadderParams::default(),
        horizons_ms: vec![50, 1_000, 15_000, 60_000, 300_000],
        race_window_us: 500,
        queue_edges: vec![0, 1, 50, 100, 200, 500],
        ofi_ms: vec![1_000, 10_000],
        out: None,
    };
    let mut k = 0;
    while k < args.len() {
        let key = args[k].as_str();
        let val = args.get(k + 1).ok_or_else(|| format!("flag `{key}` needs a value"))?.as_str();
        let int = || val.parse::<i64>().map_err(|e| format!("{key}: {e}"));
        let float = || val.parse::<f64>().map_err(|e| format!("{key}: {e}"));
        match key {
            "--seed" => c.seed = val.parse().map_err(|e| format!("{key}: {e}"))?,
            "--secs" => c.secs = int()?,
            "--warmup-secs" => c.warmup_secs = int()?,
            "--background" => {
                c.zero_intelligence = match val {
                    "default" => false,
                    "zi" | "zero-intelligence" => true,
                    other => return Err(format!("unknown background `{other}` (default, zi)")),
                }
            }
            "--value-ms" => c.value_ms = int()?,
            "--maker-fee" => c.fees.maker = int()?,
            "--taker-fee" => c.fees.taker = int()?,
            "--informed" => c.informed = int()? as usize,
            "--informed-noise" => c.informed_noise = float()?,
            "--informed-threshold" => c.informed_threshold = int()?,
            "--informed-clip" => c.informed_clip = int()?,
            "--informed-latency" => c.informed_location = location(val)?,
            "--arbs" => c.arbs = int()? as usize,
            "--arb-threshold" => c.arb_threshold = int()?,
            "--arb-clip" => c.arb_clip = int()?,
            "--arb-latency" => c.arb_location = location(val)?,
            "--mm" => {
                let step = int()?;
                if !(1..=10).contains(&step) {
                    return Err(format!("--mm step must be 1-10, got {step}"));
                }
                c.makers.push(step as usize);
            }
            "--mm-latency" => c.mm_location = location(val)?,
            "--mm-unit-lots" => c.mm_unit_lots = int()?,
            "--mm-max-inventory" => c.ladder.max_inventory = int()?,
            "--mm-gamma" => c.ladder.gamma = float()?,
            "--mm-sigma" => c.ladder.sigma = float()?,
            "--mm-a" => c.ladder.a = float()?,
            "--mm-k" => c.ladder.k = float()?,
            "--mm-skew" => c.ladder.skew_ticks_per_unit = float()?,
            "--mm-half-spread" => c.ladder.half_spread_ticks = float()?,
            "--mm-as-horizon" => c.ladder.as_horizon_s = float()?,
            "--horizons-ms" => c.horizons_ms = list(val)?,
            "--race-window-us" => c.race_window_us = int()?,
            "--queue-edges" => c.queue_edges = list(val)?,
            "--ofi-ms" => c.ofi_ms = list(val)?,
            "--out" => c.out = Some(val.to_string()),
            other => return Err(format!("unknown flag `{other}`\n\n{USAGE}")),
        }
        k += 2;
    }
    if c.secs <= c.warmup_secs {
        return Err(format!("--secs ({}) must exceed --warmup-secs ({})", c.secs, c.warmup_secs));
    }
    Ok(c)
}

/// FNV-1a over the flags, sorted by flag name (stable for repeated flags), so equivalent
/// orderings of different flags hash alike; `--out` is left out.
fn config_hash(args: &[String]) -> String {
    let mut pairs: Vec<(&str, &str)> = args.chunks(2).filter(|p| p.len() == 2 && p[0] != "--out").map(|p| (p[0].as_str(), p[1].as_str())).collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (k, v) in pairs {
        for b in k.bytes().chain([b'=']).chain(v.bytes()).chain([0x1f]) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("fnv1a64:{h:016x}")
}

fn decomposition_json(d: &PnlDecomposition) -> J {
    let p = |x: i128| J::Num(d.price_units(x) / 100.0);
    J::obj([
        ("horizon_ms", J::Num(d.horizon_ns as f64 / 1e6)),
        ("spread_capture", p(d.spread_capture)),
        ("adverse_selection", p(d.adverse_selection)),
        ("inventory", p(d.inventory)),
        ("carried", p(d.carried)),
        ("fees", p(d.fees)),
        ("total", p(d.total)),
        ("total_exact_half_fee_units", J::Int(d.total)),
    ])
}

fn queue_json(b: &[QueueBucket]) -> J {
    J::Arr(
        b.iter()
            .map(|x| {
                J::obj([
                    ("min_ahead", J::Int(i128::from(x.min_ahead))),
                    ("max_ahead", x.max_ahead.map_or(J::Null, |m| J::Int(i128::from(m)))),
                    ("orders", J::Int(x.orders as i128)),
                    ("any_fill", J::Int(x.any_fill as i128)),
                    ("complete", J::Int(x.complete as i128)),
                ])
            })
            .collect(),
    )
}

/// One finished run: its JSON report and, if asked for, its tables.
pub struct Run {
    /// The JSON report (manifest, market metrics, agents, background).
    pub json: String,
    /// Event log, top of book and participants, for Arrow ([`run_with_tables`] only).
    pub tables: Option<export::Tables>,
    out: Option<String>,
}

/// Run one market and return its JSON report, or write the report to `--out` and return an empty
/// string.
pub fn run(args: &[String]) -> Result<String, String> {
    let r = simulate(args, false)?;
    match &r.out {
        Some(path) => {
            std::fs::write(path, &r.json).map_err(|e| format!("cannot write {path}: {e}"))?;
            Ok(String::new())
        }
        None => Ok(r.json),
    }
}

/// Run one market and return its JSON report together with its tables ([`export::Tables`]).
/// `--out` is ignored. The run is the same as [`run`]'s: building the tables reads the finished
/// event log and changes nothing.
pub fn run_with_tables(args: &[String]) -> Result<Run, String> {
    simulate(args, true)
}

/// A market built from a [`Config`], not yet run.
struct Market {
    sim: sim::Simulation,
    i: sim::InstrumentId,
    value: sim::SignalId,
    bg: agents::Background,
    extra: Vec<(AgentId, &'static str, String)>,
    spec: InstrumentSpec,
}

/// Flags for commands that only run the market and never measure over a window (LOBSTER export,
/// replay check): `--warmup-secs` defaults to 0 there, so `--secs` may be short.
fn parse_market(args: &[String]) -> Result<Config, String> {
    if args.iter().any(|a| a == "--warmup-secs") {
        parse(args)
    } else {
        parse(&[args, &["--warmup-secs".to_string(), "0".to_string()]].concat())
    }
}

/// Run one market and return its report, with tables if `with_tables`. `--out` is ignored.
pub fn simulate(args: &[String], with_tables: bool) -> Result<Run, String> {
    let c = parse(args)?;
    let Market { mut sim, i, value, bg, extra, spec } = build_market(&c, None)?;
    let wall = Instant::now();
    sim.run_until(c.secs * S);
    let wall_s = wall.elapsed().as_secs_f64();
    report(&c, args, &sim, i, value, &bg, &extra, spec, wall_s, with_tables)
}

/// Run one market and write its public event log as LOBSTER files
/// ([`data::lobster::write_lobster`]). Returns the run's manifest (as JSON) and what was written.
pub fn lobster_export<M: std::io::Write, B: std::io::Write>(
    args: &[String],
    opts: &data::lobster::ExportOptions,
    messages: &mut M,
    book: &mut B,
) -> Result<(String, data::lobster::ExportStats), String> {
    let c = parse_market(args)?;
    let Market { mut sim, i, .. } = build_market(&c, None)?;
    sim.run_until(c.secs * S);
    let stats = data::lobster::write_lobster(sim.events(i), opts, messages, book).map_err(|e| format!("LOBSTER export: {e}"))?;
    let manifest = J::obj([
        ("args", J::Arr(args.iter().map(|a| J::Str(a.clone())).collect())),
        ("config_hash", J::Str(config_hash(args))),
        ("seed", J::Int(i128::from(c.seed))),
        ("version", J::Str(env!("CARGO_PKG_VERSION").into())),
        ("digest", J::Str(format!("{:#018x}", sim.digest()))),
        ("trades", J::Int(i128::from(sim.stats().trades))),
        ("simulated_secs", J::Int(i128::from(c.secs))),
    ]);
    Ok((manifest.render(), stats))
}

/// [`lobster_export`] into `dir`, with LOBSTER file names (symbol `HFTSIM`, pseudo-date
/// 2000-01-01, simulation time 0 at 9:30) and a `provenance.json` beside them. Returns a JSON
/// summary.
pub fn lobster_export_dir(args: &[String], levels: usize, dir: &std::path::Path) -> Result<String, String> {
    use data::provenance::{DataLevel, Provenance, file_info};
    let c = parse_market(args)?;
    let opts = data::lobster::ExportOptions { levels, ..Default::default() };
    let (start_ms, end_ms) = (opts.start_ns / 1_000_000, opts.start_ns / 1_000_000 + c.secs * 1_000);
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let name = |kind: &str| dir.join(format!("HFTSIM_2000-01-01_{start_ms}_{end_ms}_{kind}_{levels}.csv"));
    let (msg_path, book_path) = (name("message"), name("orderbook"));
    let open = |p: &std::path::Path| std::fs::File::create(p).map(std::io::BufWriter::new).map_err(|e| format!("cannot write {}: {e}", p.display()));
    let (mut msg, mut book) = (open(&msg_path)?, open(&book_path)?);
    let (manifest, stats) = lobster_export(args, &opts, &mut msg, &mut book)?;
    std::io::Write::flush(&mut msg).and_then(|()| std::io::Write::flush(&mut book)).map_err(|e| e.to_string())?;
    drop((msg, book));
    let files = [&msg_path, &book_path].iter().map(|p| file_info(p)).collect::<std::io::Result<Vec<_>>>().map_err(|e| e.to_string())?;
    let provenance = Provenance {
        source: "hft-sim simulation (flags and digest in the manifest)".into(),
        symbol: "HFTSIM".into(),
        date: "2000-01-01 (pseudo-date; simulation time 0 = 09:30)".into(),
        data_level: DataLevel::Mbo,
        levels,
        files,
        notes: vec![
            "Simulated, not historical.".into(),
            "The file starts at simulation time 0 with an empty book, so no order predates it.".into(),
            "Prices in dollars x 10,000 (engine cents x 100), as LOBSTER writes them.".into(),
        ],
    };
    let summary = format!(
        "{{\"provenance\":{},\"manifest\":{},\"rows\":{},\"by_type\":[{}]}}",
        provenance.to_json(),
        manifest,
        stats.rows,
        stats.by_kind.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
    );
    std::fs::write(dir.join("provenance.json"), &summary).map_err(|e| e.to_string())?;
    Ok(summary)
}

/// Probe agents for [`replay_check`]: each rests one small order at its view of the touch at a
/// time (`agents::ProbeTrader`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProbeConfig {
    /// Probe agents.
    pub agents: usize,
    /// Mean time between placement attempts, milliseconds.
    pub interval_ms: f64,
    /// Life of a probe before it is cancelled, milliseconds.
    pub life_ms: i64,
    /// Probe size, lots.
    pub lots: i64,
    /// Latency location: 0 co-located, 1 proximity, 2 remote.
    pub location: usize,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        ProbeConfig { agents: 4, interval_ms: 500.0, life_ms: 5_000, lots: 1, location: 0 }
    }
}

/// Result of [`replay_check`]: modelled against real fills of the probe orders.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplayCheck {
    /// The run's manifest (JSON).
    pub manifest: String,
    /// Probe orders accepted by the exchange.
    pub probes: usize,
    /// Probes that took liquidity on arrival (or would cross the replayed book): not passive,
    /// not compared.
    pub marketable: usize,
    /// Passive probes the exchange filled (any part).
    pub truth_filled: usize,
    /// Passive probes the model filled.
    pub model_filled: usize,
    /// Filled by both.
    pub both: usize,
    /// Filled by the exchange only (the model was pessimistic).
    pub truth_only: usize,
    /// Filled by the model only (the model was optimistic).
    pub model_only: usize,
    /// Filled by neither.
    pub neither: usize,
    /// Filled by both, first fill at the same nanosecond.
    pub same_time: usize,
    /// Filled by both, the model's first fill earlier than the exchange's.
    pub model_earlier: usize,
    /// Filled by both, the model's first fill later.
    pub model_later: usize,
    /// Exchange fills whose aggressor also traded behind or through the probe in the same
    /// command, so that the replay stream, which lacks the probe, still shows the volume.
    pub truth_visible: usize,
    /// Of those, fills the model missed.
    pub truth_only_visible: usize,
    /// For probes the model filled later than the exchange: the delays, nanoseconds, sorted.
    pub later_ns: Vec<i64>,
}

impl ReplayCheck {
    /// As a JSON object.
    pub fn to_json(&self) -> String {
        let q = |f: f64| -> String {
            if self.later_ns.is_empty() {
                return "null".into();
            }
            let k = ((self.later_ns.len() - 1) as f64 * f).round() as usize;
            format!("{:.3}", self.later_ns[k] as f64 / 1e6)
        };
        format!(
            "{{\"manifest\":{},\"probes\":{},\"marketable\":{},\"passive\":{},\"truth_filled\":{},\"model_filled\":{},\"both\":{},\"truth_only\":{},\
             \"model_only\":{},\"neither\":{},\"same_time\":{},\"model_earlier\":{},\"model_later\":{},\"truth_visible\":{},\"truth_only_visible\":{},\
             \"later_ms\":{{\"p10\":{},\"median\":{},\"p90\":{}}}}}",
            self.manifest,
            self.probes,
            self.marketable,
            self.probes - self.marketable,
            self.truth_filled,
            self.model_filled,
            self.both,
            self.truth_only,
            self.model_only,
            self.neither,
            self.same_time,
            self.model_earlier,
            self.model_later,
            self.truth_visible,
            self.truth_only_visible,
            q(0.1),
            q(0.5),
            q(0.9)
        )
    }
}

/// Check the pure-replay queue model ([`data::replay`]) against the exchange. A market runs with
/// probe agents resting small orders at the touch. Its event log, *minus the probes' own book
/// events and their executions*, is written as LOBSTER messages: what a vendor file would show had
/// the probes been someone else's hypothetical orders. Each probe is then replayed through that
/// stream (entry at its acceptance, expiry at its cancellation) and the modelled fills compared
/// with the exchange's.
///
/// One blind spot is built in: when a probe absorbed an aggressor's entire remaining volume, the
/// stream without the probe shows no trade at all, so the model cannot see that fill.
/// `truth_visible` counts the fills the stream does show.
pub fn replay_check(args: &[String], probes: &ProbeConfig) -> Result<ReplayCheck, String> {
    use std::collections::{HashMap, HashSet};

    use data::lobster::{ExportOptions, parse_messages, write_lobster};
    use data::replay::{PassiveOrder, replay_passive};
    use lob::{EventKind, Liquidity, Side};

    struct Probe {
        ts: i64,
        side: Side,
        price: i64,
        qty: i64,
        first_maker_fill: Option<i64>,
        first_fill_visible: bool,
        taker: bool,
        cancelled: Option<i64>,
    }
    let c = parse_market(args)?;
    let Market { mut sim, i, extra, .. } = build_market(&c, Some(probes))?;
    sim.run_until(c.secs * S);
    let events = sim.events(i);
    let probe_agents: HashSet<AgentId> = extra.iter().filter(|e| e.1 == "probe").map(|e| e.0).collect();
    let mut ps: HashMap<u64, Probe> = HashMap::new();
    let mut accepted: Vec<u64> = Vec::new();
    for e in events {
        match e.kind {
            EventKind::Accepted { order_id, owner, side, price: Some(price), qty, .. } if probe_agents.contains(&owner) => {
                ps.insert(order_id, Probe { ts: e.ts, side, price, qty, first_maker_fill: None, first_fill_visible: false, taker: false, cancelled: None });
                accepted.push(order_id);
            }
            EventKind::Fill { order_id, liquidity, .. } if ps.contains_key(&order_id) => {
                let p = ps.get_mut(&order_id).expect("probe");
                if liquidity == Liquidity::Taker {
                    p.taker = true;
                } else if p.first_maker_fill.is_none() {
                    p.first_maker_fill = Some(e.ts);
                }
            }
            EventKind::Cancelled { order_id, .. } if ps.contains_key(&order_id) => {
                ps.get_mut(&order_id).expect("probe").cancelled = Some(e.ts);
            }
            _ => {}
        }
    }
    // Which probe fills would still be visible without the probe: the same command also traded
    // against a non-probe order on the probe's side at its price (behind it) or worse.
    let resting = |kind: &EventKind| match *kind {
        EventKind::Trade { price, aggressor: Some(Side::Buy), sell_order_id, .. } => Some((sell_order_id, Side::Sell, price)),
        EventKind::Trade { price, aggressor: Some(Side::Sell), buy_order_id, .. } => Some((buy_order_id, Side::Buy, price)),
        _ => None,
    };
    for (k, e) in events.iter().enumerate() {
        let Some((id, side, price)) = resting(&e.kind) else { continue };
        let Some(p) = ps.get(&id) else { continue };
        if p.first_maker_fill != Some(e.ts) {
            continue;
        }
        let visible = events[k + 1..].iter().take_while(|x| x.ts == e.ts).filter_map(|x| resting(&x.kind)).any(|(other, s, q)| {
            !ps.contains_key(&other)
                && s == side
                && match side {
                    Side::Buy => q <= price,
                    Side::Sell => q >= price,
                }
        });
        ps.get_mut(&id).expect("probe").first_fill_visible = visible;
    }
    // The stream a vendor would publish without the probes.
    let is_probe = |id: u64| ps.contains_key(&id);
    let filtered: Vec<lob::Event> = events
        .iter()
        .filter(|e| match e.kind {
            EventKind::BookAdd { order_id, .. } | EventKind::BookUpdate { order_id, .. } | EventKind::BookRemove { order_id } => !is_probe(order_id),
            _ => resting(&e.kind).is_none_or(|(id, ..)| !is_probe(id)),
        })
        .copied()
        .collect();
    let opts = ExportOptions { levels: 1, ..ExportOptions::default() };
    let (mut msg, mut book) = (Vec::new(), Vec::new());
    write_lobster(&filtered, &opts, &mut msg, &mut book).map_err(|e| format!("replay stream: {e}"))?;
    drop(book);
    let messages = parse_messages(std::str::from_utf8(&msg).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let orders: Vec<PassiveOrder> = accepted
        .iter()
        .map(|id| {
            let p = &ps[id];
            PassiveOrder { entry_ns: opts.start_ns + p.ts, side: p.side, price: p.price * opts.price_scale, size: p.qty, expire_ns: p.cancelled.map(|t| opts.start_ns + t), own_order_id: None }
        })
        .collect();
    let outcomes = replay_passive(&messages, None, 1, &orders);
    let mut r = ReplayCheck { probes: accepted.len(), ..ReplayCheck::default() };
    for (id, out) in accepted.iter().zip(&outcomes) {
        let p = &ps[id];
        if p.taker || out.marketable {
            r.marketable += 1;
            continue;
        }
        let truth = p.first_maker_fill;
        let model = out.fills.first().map(|f| f.ts_ns - opts.start_ns);
        r.truth_filled += usize::from(truth.is_some());
        r.model_filled += usize::from(model.is_some());
        r.truth_visible += usize::from(truth.is_some() && p.first_fill_visible);
        match (truth, model) {
            (Some(t), Some(m)) => {
                r.both += 1;
                match m.cmp(&t) {
                    std::cmp::Ordering::Equal => r.same_time += 1,
                    std::cmp::Ordering::Less => r.model_earlier += 1,
                    std::cmp::Ordering::Greater => {
                        r.model_later += 1;
                        r.later_ns.push(m - t);
                    }
                }
            }
            (Some(_), None) => {
                r.truth_only += 1;
                r.truth_only_visible += usize::from(p.first_fill_visible);
            }
            (None, Some(_)) => r.model_only += 1,
            (None, None) => r.neither += 1,
        }
    }
    r.later_ns.sort_unstable();
    r.manifest = J::obj([
        ("args", J::Arr(args.iter().map(|a| J::Str(a.clone())).collect())),
        ("seed", J::Int(i128::from(c.seed))),
        ("digest", J::Str(format!("{:#018x}", sim.digest()))),
        (
            "probes",
            J::obj([
                ("agents", J::Int(probes.agents as i128)),
                ("interval_ms", J::Num(probes.interval_ms)),
                ("life_ms", J::Int(i128::from(probes.life_ms))),
                ("lots", J::Int(i128::from(probes.lots))),
                ("location", J::Int(probes.location as i128)),
            ]),
        ),
    ])
    .render();
    Ok(r)
}

fn build_market(c: &Config, probes: Option<&ProbeConfig>) -> Result<Market, String> {
    let spec = InstrumentSpec::example();
    let mut b = SimBuilder::new(c.seed);
    b.record_events(true);
    let i = b.add_instrument(InstrumentConfig::new(spec).with_fees(c.fees));
    let value = b.add_signal(calibrated_value(spec.reference_price, spec.tick_size, c.value_ms as f64 / 1000.0));
    let bg_cfg = if c.zero_intelligence { BackgroundConfig::zero_intelligence() } else { BackgroundConfig::default() };
    let bg = add_background(&mut b, i, value, &bg_cfg);
    let mut extra: Vec<(AgentId, &'static str, String)> = Vec::new();
    for _ in 0..c.informed {
        let mut t = InformedTrader::new(i, value);
        (t.perception, t.threshold_ticks, t.clip_lots) = (Perception { noise_sd: c.informed_noise }, c.informed_threshold, c.informed_clip);
        t.limits = Limits { max_position: 200, max_order_qty: 10 * c.informed_clip.max(1), max_open_orders: 5, capital: i128::MAX };
        let cfg = AgentConfig::new(profile(c.informed_location)).subscribe_group(i, bg.groups[c.informed_location], FeedMode::Snapshot).with_signal(value);
        extra.push((b.add_agent(t, cfg), "informed", "informed trader".into()));
    }
    for _ in 0..c.arbs {
        let mut a = LatencyArbitrageur::new(i, value);
        (a.threshold_ticks, a.clip_lots) = (c.arb_threshold, c.arb_clip);
        let cfg = AgentConfig::new(profile(c.arb_location)).subscribe(i).with_signal(value);
        extra.push((b.add_agent(a, cfg), "latency_arbitrageur", "latency arbitrageur".into()));
    }
    for &step in &c.makers {
        let mm = MarketMaker::new(i, QuotingStrategy::ladder(step, &c.ladder)).with_unit_lots(c.mm_unit_lots);
        let cfg = AgentConfig::new(profile(c.mm_location)).subscribe(i);
        extra.push((b.add_agent(mm, cfg), "market_maker", format!("step {step}: {}", agents::mm::LADDER[step - 1])));
    }
    // Probes go last, so every other agent keeps its id and random streams.
    for _ in 0..probes.map_or(0, |p| p.agents) {
        let p = probes.expect("probes");
        let mut probe = agents::ProbeTrader::new(i);
        probe.schedule = agents::Schedule::Poisson { mean_ns: p.interval_ms * 1e6 };
        (probe.lifetime_ns, probe.lots) = (p.life_ms * 1_000_000, p.lots);
        let cfg = AgentConfig::new(profile(p.location)).subscribe(i);
        extra.push((b.add_agent(probe, cfg), "probe", "measurement probe".into()));
    }
    let sim = b.build().map_err(|e| format!("invalid market: {e:?}"))?;
    Ok(Market { sim, i, value, bg, extra, spec })
}

/// The JSON report (and tables) of a finished run.
#[allow(clippy::too_many_arguments)]
fn report(
    c: &Config,
    args: &[String],
    sim: &sim::Simulation,
    i: sim::InstrumentId,
    value: sim::SignalId,
    bg: &agents::Background,
    extra: &[(AgentId, &'static str, String)],
    spec: InstrumentSpec,
    wall_s: f64,
    with_tables: bool,
) -> Result<Run, String> {
    let end = c.secs * S;
    let from = c.warmup_secs * S;
    let events = sim.events(i);
    let path = BookPath::from_events(events);
    let ix = OrderIndex::from_events(events);
    let tick = spec.tick_size;
    let mid2_end = path.mid2_at(end).ok_or("the book was never two-sided")?;
    let horizons_ns: Vec<i64> = c.horizons_ms.iter().map(|h| h * 1_000_000).collect();
    let rates = FeeRates { maker: c.fees.maker, taker: c.fees.taker, auction: c.fees.auction, scale: FEE_SCALE };

    // Market-wide.
    let metaorders: Vec<Metaorder> = bg
        .executors
        .iter()
        .flat_map(|&a| sim.agent::<ExecutionAgent>(a).map(|e| e.reports.clone()).unwrap_or_default())
        .filter(|r| r.filled > 0)
        .map(|r| Metaorder { side: r.side, qty: r.filled, start: r.start, end: r.end })
        .collect();
    let facts = stylized_facts(events, &metaorders, &FactsConfig::standard(from, end, tick));
    let checks = check_targets(&facts);
    let spreads = execution_spreads(events, &path, &ix, from, end, &horizons_ns, None);
    let gw_fails: Vec<(i64, AgentId, u64)> = sim
        .gateway_log()
        .iter()
        .filter(|g| g.report == ReportKind::RequestRejected { request: RequestKind::Cancel, reason: RejectReason::UnknownOrder })
        .filter_map(|g| match g.request {
            Request::Cancel { client_id, .. } => Some((g.ts, g.agent, client_id)),
            _ => None,
        })
        .collect();
    let race_msgs = race_messages(events, &ix, &gw_fails, from, end);
    let races = latency_races(&race_msgs, c.race_window_us * 1_000);
    let spread_json = |s: &analysis::metrics::ExecutionSpreads| {
        J::obj([
            ("executions", J::Int(s.executions as i128)),
            ("shares", J::Int(i128::from(s.shares))),
            ("quoted_cents", J::Num(s.quoted)),
            ("effective_cents", J::Num(s.effective)),
            ("percentage_effective_bp", J::Num(1e4 * s.percentage_effective())),
            (
                "realized",
                J::Arr(
                    s.horizons_ns
                        .iter()
                        .enumerate()
                        .map(|(k, h)| {
                            J::obj([
                                ("horizon_ms", J::Num(*h as f64 / 1e6)),
                                ("realized_cents", J::Num(s.realized[k])),
                                ("price_impact_cents", J::Num(s.price_impact(k))),
                                ("percentage_realized_bp", J::Num(1e4 * s.percentage_realized(k))),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
    };
    let market = J::obj([
        ("window_s", J::Arr(vec![J::Int(i128::from(c.warmup_secs)), J::Int(i128::from(c.secs))])),
        ("final_mid", J::Num(mid2_end as f64 / 200.0)),
        ("final_value", J::Num(sim.signal_value(value) as f64 / 100.0)),
        ("spread_one_tick_share", J::Num(facts.spread.share_by_ticks[0])),
        ("mean_spread_ticks", J::Num(facts.spread.mean_ticks)),
        ("cancel_share", J::Num(facts.flow.cancel_share)),
        ("executed_share", J::Num(facts.flow.executed_share)),
        ("trades", J::Int(i128::from(sim.stats().trades))),
        ("sign_gamma", J::opt(facts.signs.gamma())),
        ("impact_delta", J::opt(facts.impact.fit.map(|f| f.slope))),
        ("return_acf1", J::Num(facts.returns.acf1)),
        ("variance_ratio_10", J::Num(facts.returns.variance_ratio_10)),
        ("stylized_targets_met", J::Int(checks.iter().filter(|x| x.pass).count() as i128)),
        ("realized_vol_ticks_per_sqrt_s", J::opt(realized_volatility(&path, from, end, S, tick))),
        ("amihud_per_dollar", J::opt(analysis::metrics::amihud(events, &path, from, end, 60 * S).map(|a| a * 100.0))),
        ("roll_spread_cents", J::opt(roll_spread(events, from, end))),
        ("execution_spreads", spread_json(&spreads)),
        (
            "ofi",
            J::Arr(
                c.ofi_ms
                    .iter()
                    .map(|&ms| match ofi_regression(&path, from, end, ms * 1_000_000, tick) {
                        Some(f) => J::obj([
                            ("interval_ms", J::Int(i128::from(ms))),
                            ("intervals", J::Int(f.intervals as i128)),
                            ("beta_ticks_per_share", J::Num(f.beta)),
                            ("r2", J::Num(f.r2)),
                            ("depth_shares", J::Num(f.depth)),
                        ]),
                        None => J::Null,
                    })
                    .collect(),
            ),
        ),
        ("queue_fill_touch", queue_json(&fill_probability_by_queue(events, &ix, from, end, None, &c.queue_edges, true))),
        ("queue_fill_all", queue_json(&fill_probability_by_queue(events, &ix, from, end, None, &c.queue_edges, false))),
        (
            "races",
            J::obj([
                ("window_us", J::Int(i128::from(c.race_window_us))),
                ("messages", J::Int(races.messages as i128)),
                ("races", J::Int(races.races as i128)),
                ("won_by_take", J::Int(races.won_by_take as i128)),
                ("won_by_cancel", J::Int(races.won_by_cancel as i128)),
                ("mean_duration_us", J::Num(races.mean_duration_ns / 1e3)),
            ]),
        ),
    ]);

    // Agents of interest, and background groups.
    let ledger_pnl = |a: AgentId| -> f64 {
        let l = sim.ledger(a, i);
        let half = (2 * l.cash + i128::from(l.position) * i128::from(mid2_end)) * i128::from(FEE_SCALE) - 2 * l.fees;
        half as f64 / (2.0 * FEE_SCALE as f64) / 100.0
    };
    let mut agent_rows = Vec::new();
    for (id, kind, label) in extra {
        // PnL over the measurement window (warm-up, end]: fills before the window are carried in
        // as an opening position marked at the window-start mid (metrics::pnl_decomposition_window).
        let windows: Vec<PnlDecomposition> = horizons_ns
            .iter()
            .map(|&h| pnl_decomposition_window(events, &path, *id, from, end, h, rates))
            .collect::<Option<_>>()
            .ok_or("no two-sided book to mark the window at")?;
        let w = windows.first().copied().unwrap_or_default();
        let money = |x: i128| J::Num(w.price_units(x) / 100.0);
        let own_cancels = race_msgs.iter().filter(|m| m.owner == *id && !m.take);
        let (cancel_attempts, cancels_too_late) = own_cancels.fold((0i128, 0i128), |(n, f), m| (n + 1, f + i128::from(!m.success)));
        let mut fields = vec![
            ("id".to_string(), J::Int(i128::from(*id))),
            ("kind".to_string(), J::Str((*kind).into())),
            ("label".to_string(), J::Str(label.clone())),
            ("pnl_dollars".to_string(), money(w.total)),
            ("pnl_whole_run_dollars".to_string(), J::Num(ledger_pnl(*id))),
            ("fees_dollars".to_string(), money(w.fees)),
            ("opening_position".to_string(), J::Int(i128::from(w.opening_position))),
            ("position".to_string(), J::Int(i128::from(w.position))),
            ("volume".to_string(), J::Int(i128::from(w.volume))),
            ("fills".to_string(), J::Int(w.fills as i128)),
            ("maker_share".to_string(), J::Num(if w.volume > 0 { w.maker_volume as f64 / w.volume as f64 } else { 0.0 })),
            ("race_wins".to_string(), J::Int(*races.wins.get(id).unwrap_or(&0) as i128)),
            ("race_losses".to_string(), J::Int(*races.losses.get(id).unwrap_or(&0) as i128)),
            ("cancel_attempts".to_string(), J::Int(cancel_attempts)),
            ("cancels_too_late".to_string(), J::Int(cancels_too_late)),
            ("decomposition".to_string(), J::Arr(windows.iter().map(decomposition_json).collect())),
        ];
        if *kind == "market_maker" {
            let m = sim.agent::<MarketMaker>(*id).ok_or("market maker missing")?;
            let s = m.stats;
            // Time-weighted root-mean-square and largest absolute position over the window, from
            // the exchange's fills.
            let (mut pos, mut last, mut acc, mut max_abs) = (0i64, from, 0f64, w.opening_position.abs());
            for e in events.iter().filter(|e| e.ts <= end) {
                if let lob::EventKind::Fill { order_id, owner, qty, .. } = e.kind {
                    if owner == *id {
                        if e.ts > from {
                            acc += (pos as f64).powi(2) * (e.ts - last.max(from)) as f64;
                            last = e.ts;
                        }
                        let side = ix.by_id.get(&order_id).map(|o| o.side);
                        pos += if side == Some(lob::Side::Buy) { qty } else { -qty };
                        if e.ts > from {
                            max_abs = max_abs.max(pos.abs());
                        }
                    }
                }
            }
            acc += (pos as f64).powi(2) * (end - last.max(from)) as f64;
            let inventory_rms = (acc / (end - from) as f64).sqrt();
            fields.push((
                "stats".to_string(),
                J::obj([
                    ("requotes", J::Int(i128::from(s.requotes))),
                    ("orders_sent", J::Int(i128::from(s.orders_sent))),
                    ("cancels_sent", J::Int(i128::from(s.cancels_sent))),
                    ("post_only_rejects", J::Int(i128::from(s.post_only_rejects))),
                    ("max_abs_position", J::Int(i128::from(max_abs))),
                    ("inventory_rms", J::Num(inventory_rms)),
                    ("two_sided_share", J::Num(s.two_sided_share())),
                    ("sigma_estimate", J::opt(m.vol.sigma())),
                    ("markout_1s_ticks", J::opt(m.markouts.recent())),
                ]),
            ));
            fields.push(("queue_fill_touch".to_string(), queue_json(&fill_probability_by_queue(events, &ix, from, end, Some(*id), &c.queue_edges, true))));
        } else {
            fields.push(("taker_spreads".to_string(), spread_json(&execution_spreads(events, &path, &ix, from, end, &horizons_ns, Some(&[*id])))));
        }
        agent_rows.push(J::Obj(fields));
    }
    let group = |ids: &[AgentId]| J::obj([("agents", J::Int(ids.len() as i128)), ("pnl_whole_run_dollars", J::Num(ids.iter().map(|&a| ledger_pnl(a)).sum()))]);
    let manifest = J::obj([
        ("args", J::Arr(args.iter().map(|a| J::Str(a.clone())).collect())),
        ("config_hash", J::Str(config_hash(args))),
        ("seed", J::Int(i128::from(c.seed))),
        ("version", J::Str(env!("CARGO_PKG_VERSION").into())),
        ("git_commit", J::Str(option_env!("HFTSIM_GIT_COMMIT").unwrap_or("unknown").into())),
        ("digest", J::Str(format!("{:#018x}", sim.digest()))),
        ("trades", J::Int(i128::from(sim.stats().trades))),
        ("engine_commands", J::Int(i128::from(sim.stats().engine_commands))),
        ("simulated_secs", J::Int(i128::from(c.secs))),
        ("wall_secs", J::Num(wall_s)),
    ]);
    let out = J::obj([
        ("manifest", manifest),
        ("market", market),
        ("agents", J::Arr(agent_rows)),
        ("background", J::obj([("noise", group(&bg.noise)), ("fundamental", group(&bg.fundamental)), ("executors", group(&bg.executors))])),
    ]);
    let tables = with_tables.then(|| {
        let roles = [("noise", &bg.noise, "noise trader"), ("fundamental", &bg.fundamental, "fundamental trader"), ("executor", &bg.executors, "metaorder executor")];
        let mut owners: Vec<(u32, &'static str, String)> = roles.iter().flat_map(|(role, ids, label)| ids.iter().map(move |&id| (id, *role, (*label).to_string()))).collect();
        owners.extend(extra.iter().map(|(id, kind, label)| (*id, *kind, label.clone())));
        owners.sort_by_key(|o| o.0);
        export::Tables { events: export::event_columns(events, &ix), book: export::book_columns(&path), owners }
    });
    Ok(Run { json: out.render(), tables, out: c.out.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_parse_and_reject_mistakes() {
        let c = parse(&a(&["--seed", "7", "--mm", "3", "--mm", "10", "--background", "zi", "--horizons-ms", "50,1000"])).expect("valid");
        assert_eq!((c.seed, c.makers.clone(), c.zero_intelligence, c.horizons_ms.clone()), (7, vec![3, 10], true, vec![50, 1000]));
        assert!(parse(&a(&["--mm", "11"])).is_err());
        assert!(parse(&a(&["--bogus", "1"])).is_err());
        assert!(parse(&a(&["--seed"])).is_err());
        assert!(parse(&a(&["--secs", "30", "--warmup-secs", "60"])).is_err());
    }

    #[test]
    fn the_same_flags_reproduce_the_same_run() {
        let args = a(&["--seed", "3", "--secs", "4", "--warmup-secs", "1", "--mm", "5", "--informed", "2", "--maker-fee", "-20", "--taker-fee", "30"]);
        let strip = |s: String| -> String {
            // Everything except the wall-clock time must be identical.
            let k = s.find("\"wall_secs\":").expect("wall time");
            let end = s[k..].find('}').expect("closing brace") + k;
            format!("{}{}", &s[..k], &s[end..])
        };
        let digest = |s: &str| -> String {
            let k = s.find("\"digest\":\"").expect("digest") + 10;
            s[k..k + 18].to_string()
        };
        let (x, y) = (run(&args).expect("run"), run(&args).expect("run"));
        assert!(digest(&x).starts_with("0x"));
        let other = run(&a(&["--seed", "4", "--secs", "4", "--warmup-secs", "1", "--mm", "5", "--informed", "2"])).expect("run");
        assert_ne!(digest(&x), digest(&other), "a different seed is a different run");
        assert_eq!(strip(x), strip(y));
    }

    #[test]
    fn tables_come_from_the_same_run_and_agree_with_its_report() {
        let args = a(&["--seed", "3", "--secs", "4", "--warmup-secs", "1", "--mm", "5", "--informed", "2"]);
        let r = run_with_tables(&args).expect("run");
        let field = |s: &str, key: &str| -> String {
            let k = s.find(key).expect(key) + key.len();
            s[k..].chars().take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '.' || *ch == '-').collect()
        };
        assert_eq!(field(&r.json, "\"digest\":\""), field(&run(&args).expect("run"), "\"digest\":\""), "building tables changes nothing");
        let t = r.tables.expect("tables");
        let rows = t.events[0].len;
        assert!(rows > 0 && t.events.iter().all(|c| c.len == rows));
        let kind = t.events.iter().find(|c| c.name == "kind").expect("kind column");
        let export::Values::Codes(codes, names) = &kind.values else { panic!("kind is coded") };
        let code = |n: &str| names.iter().position(|x| *x == n).expect("kind name") as i8;
        let count = |n: &str| codes.iter().filter(|&&c| c == code(n)).count();
        let trades: usize = field(&r.json, "\"trades\":").parse().expect("trades");
        assert_eq!(count("trade"), trades, "one trade row per trade in the manifest");
        assert_eq!(count("fill"), 2 * trades, "a maker and a taker fill per continuous trade");
        // Participants: the background, then 2 informed traders and the market maker, by id.
        assert!(t.owners.windows(2).all(|w| w[0].0 < w[1].0));
        let kinds: Vec<&str> = t.owners.iter().map(|o| o.1).filter(|k| !["noise", "fundamental", "executor"].contains(k)).collect();
        assert_eq!(kinds, ["informed", "informed", "market_maker"]);
        let book_rows = t.book[0].len;
        assert!(book_rows > 0 && t.book.iter().all(|c| c.len == book_rows));
    }

    #[test]
    fn config_hash_ignores_flag_order_and_output_path() {
        let h = config_hash(&a(&["--seed", "7", "--mm", "3"]));
        assert_eq!(h, config_hash(&a(&["--mm", "3", "--seed", "7", "--out", "x.json"])));
        assert_ne!(h, config_hash(&a(&["--seed", "8", "--mm", "3"])));
    }
}
