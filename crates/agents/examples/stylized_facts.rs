//! Run the background market and measure its stylised facts against the phase-2 targets.
//!
//! ```text
//! cargo run --release -p agents --example stylized_facts                    # seed 2026, 10 simulated minutes
//! cargo run --release -p agents --example stylized_facts -- --seeds 8       # seeds 2026..=2033, 4 at a time
//! cargo run --release -p agents --example stylized_facts -- --minutes 30 --seed 7 --threads 2
//! ```
//!
//! Options: `--minutes`, `--seed` (first seed), `--seeds` (how many consecutive seeds), `--threads`
//! (seeds run at once; each 10-minute run holds about 0.5 GB of event log), `--value-ms` (mean time
//! between moves of fundamental value, default 1000), and for sensitivity runs `--memory-lo-ms` /
//! `--memory-hi-ms` (range of the noise traders' anchor memory; `--memory-lo-ms 0` removes it).
//!
//! The first 60 simulated seconds (the book filling up) are excluded from every measurement. With
//! several seeds, each seed's checks are reported, plus an impact fit on all seeds' metaorders
//! pooled: the seeds are independent replications of the same market.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use agents::{BackgroundConfig, CalibratedMarket, ExecutionAgent, calibrated_market};
use analysis::{FactsConfig, FactsReport, ImpactStats, Metaorder, check_targets, impact_check, sign_acf_fit, stylized_facts};
use lob::InstrumentSpec;

const WARMUP_NS: i64 = 60 * 1_000_000_000;

fn arg(name: &str, default: i64) -> i64 {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|k| args.get(k + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Run {
    seed: u64,
    wall_s: f64,
    sim_s: f64,
    commands: u64,
    trades: u64,
    final_mid: Option<f64>,
    final_value: i64,
    agents: (usize, usize, usize),
    /// Fundamental traders at their position cap at the end (saturated value traders cannot
    /// anchor the price).
    fundamental_at_cap: usize,
    facts: FactsReport,
}

/// The calibrated background, or a sensitivity variant: `--memory-lo-ms`/`--memory-hi-ms` set the
/// noise traders' anchor-memory range, and `--memory-lo-ms 0` removes the anchor (pure zero
/// intelligence).
fn config() -> BackgroundConfig {
    let mut cfg = BackgroundConfig::default();
    let (lo, hi) = (arg("--memory-lo-ms", -1), arg("--memory-hi-ms", -1));
    if lo == 0 {
        cfg.noise_anchor_memory_s = None;
        cfg.noise_trader.anchor_memory_ns = None;
    } else if lo > 0 && hi >= lo {
        cfg.noise_anchor_memory_s = Some((lo as f64 / 1e3, hi as f64 / 1e3));
    }
    cfg
}

fn run(seed: u64, minutes: i64, value_ms: i64) -> Run {
    let end = minutes * 60 * 1_000_000_000;
    let spec = InstrumentSpec::example();
    let cfg = config();
    let CalibratedMarket { mut builder, instrument: i, value, background: bg } =
        calibrated_market(seed, value_ms as f64 / 1000.0, &cfg);
    builder.record_events(true);
    let mut sim = builder.build().expect("valid market");

    let wall = Instant::now();
    sim.run_until(end);
    let wall_s = wall.elapsed().as_secs_f64();
    let stats = *sim.stats();
    let metaorders: Vec<Metaorder> = bg
        .executors
        .iter()
        .flat_map(|&a| sim.agent::<ExecutionAgent>(a).expect("executor").reports.clone())
        .filter(|r| r.filled > 0)
        .map(|r| Metaorder { side: r.side, qty: r.filled, start: r.start, end: r.end })
        .collect();
    let facts = stylized_facts(sim.events(i), &metaorders, &FactsConfig::standard(WARMUP_NS, end, spec.tick_size));
    Run {
        seed,
        wall_s,
        sim_s: end as f64 / 1e9,
        commands: stats.engine_commands,
        trades: stats.trades,
        final_mid: sim.engine(i).mid(),
        final_value: sim.signal_value(value),
        agents: (bg.noise.len(), bg.fundamental.len(), bg.executors.len()),
        fundamental_at_cap: bg
            .fundamental
            .iter()
            .filter(|&&a| sim.ledger(a, i).position.abs() >= cfg.fundamental_trader.limits.max_position)
            .count(),
        facts,
    }
}

fn print_checks(facts: &FactsReport) {
    println!("| fact | target | measured | pass |");
    println!("|---|---|---|---|");
    let checks = check_targets(facts);
    for c in &checks {
        println!("| {} | {} | {} | {} |", c.name, c.target, c.measured, if c.pass { "yes" } else { "NO" });
    }
    println!();
    println!("{}/{} targets met", checks.iter().filter(|c| c.pass).count(), checks.len());
}

fn mean_sd(x: &[f64]) -> (f64, f64) {
    let n = x.len() as f64;
    let m = x.iter().sum::<f64>() / n;
    let v = if x.len() > 1 { x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (n - 1.0) } else { 0.0 };
    (m, v.sqrt())
}

fn main() {
    let minutes = arg("--minutes", 10);
    let first = arg("--seed", 2026) as u64;
    let seeds = arg("--seeds", 1).max(1) as u64;
    let threads = arg("--threads", 4).clamp(1, seeds as i64) as u64;
    let value_ms = arg("--value-ms", 1000).max(1);

    let next = AtomicU64::new(0);
    let done = Mutex::new(Vec::new());
    let wall = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    if k >= seeds {
                        break;
                    }
                    let r = run(first + k, minutes, value_ms);
                    done.lock().expect("no worker panicked").push(r);
                }
            });
        }
    });
    let mut runs = done.into_inner().expect("no worker panicked");
    runs.sort_by_key(|r| r.seed);

    if let [r] = runs.as_slice() {
        println!("# background market: {minutes} simulated minutes, seed {}, value moves every {value_ms} ms", r.seed);
        println!();
        println!(
            "{} noise, {} fundamental, {} execution agents; {:.1} s wall ({:.1}x real time); {} engine commands, {} trades; final mid {:?}, value {}; {} fundamental traders at their position cap",
            r.agents.0,
            r.agents.1,
            r.agents.2,
            r.wall_s,
            r.sim_s / r.wall_s,
            r.commands,
            r.trades,
            r.final_mid,
            r.final_value,
            r.fundamental_at_cap
        );
        println!();
        println!("{}", r.facts);
        println!();
        print_checks(&r.facts);
        return;
    }

    println!(
        "# background market: {} seeds × {minutes} simulated minutes (seeds {}..={}), value moves every {value_ms} ms; {:.0} s wall on {threads} threads",
        runs.len(),
        first,
        first + seeds - 1,
        wall.elapsed().as_secs_f64()
    );
    println!();
    println!(
        "| seed | trades | 1-tick spread | cancel share | executed | γ | δ (fitted bins) | ret ACF(1) | VR(10) | kurtosis | mid − value | value traders at cap | targets |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for r in &runs {
        let f = &r.facts;
        let checks = check_targets(f);
        println!(
            "| {} | {} | {:.1}% | {:.1}% | {:.2}% | {} | {} | {:.3} | {:.2} | {:.2} | {} | {}/{} | {}/{} |",
            r.seed,
            r.trades,
            100.0 * f.spread.share_by_ticks[0],
            100.0 * f.flow.cancel_share,
            100.0 * f.flow.executed_share,
            f.signs.gamma().map_or("n/a".to_string(), |g| format!("{g:.2}")),
            f.impact.fit.map_or("n/a".to_string(), |p| format!("{:.2} ({})", p.slope, p.points)),
            f.returns.acf1,
            f.returns.variance_ratio_10,
            f.returns.excess_kurtosis,
            r.final_mid.map_or("n/a".to_string(), |m| format!("{:+.1}", m - r.final_value as f64)),
            r.fundamental_at_cap,
            r.agents.1,
            checks.iter().filter(|c| c.pass).count(),
            checks.len()
        );
    }
    println!();
    let col = |g: &dyn Fn(&FactsReport) -> Option<f64>| -> Vec<f64> { runs.iter().filter_map(|r| g(&r.facts)).collect() };
    for (name, values) in [
        ("γ", col(&|f| f.signs.gamma())),
        ("δ", col(&|f| f.impact.fit.map(|p| p.slope))),
        ("return ACF(1)", col(&|f| Some(f.returns.acf1))),
        ("variance ratio(10)", col(&|f| Some(f.returns.variance_ratio_10))),
        ("excess kurtosis", col(&|f| Some(f.returns.excess_kurtosis))),
    ] {
        let (m, sd) = mean_sd(&values);
        println!("{name}: mean {m:.3}, sd across seeds {sd:.3} ({} seeds)", values.len());
    }
    println!();
    println!("| fact | seeds passing |");
    println!("|---|---|");
    let all: Vec<Vec<bool>> = runs.iter().map(|r| check_targets(&r.facts).iter().map(|c| c.pass).collect()).collect();
    for (k, c) in check_targets(&runs[0].facts).iter().enumerate() {
        println!("| {} | {}/{} |", c.name, all.iter().filter(|p| p[k]).count(), runs.len());
    }
    println!();
    let pooled = ImpactStats::from_samples(
        runs.iter().flat_map(|r| r.facts.impact.samples.iter().copied()).collect(),
        runs[0].facts.impact.bin_ratio,
        runs[0].facts.impact.min_bin_count,
    );
    println!("pooled over all seeds, {pooled}");
    let c = impact_check(&pooled);
    println!();
    println!("pooled concave-impact check: {} -> {}", c.measured, if c.pass { "pass" } else { "FAIL" });
    let lags = runs[0].facts.signs.acf.len();
    let mean_acf: Vec<f64> =
        (0..lags).map(|l| runs.iter().map(|r| r.facts.signs.acf[l]).sum::<f64>() / runs.len() as f64).collect();
    let fit_range = runs[0].facts.signs.fit_range;
    let positive = mean_acf.iter().take(100).filter(|&&c| c > 0.0).count();
    println!(
        "pooled sign ACF (mean over seeds): C(1) {:.3}, C(10) {:.3}, C(100) {:.4}; {positive}/100 of lags 1-100 positive; γ {} over lags {}-{}",
        mean_acf[0],
        mean_acf[9.min(lags - 1)],
        mean_acf[99.min(lags - 1)],
        sign_acf_fit(&mean_acf, fit_range).map_or("n/a".to_string(), |p| format!("{:.2} (r² {:.2})", -p.slope, p.r2)),
        fit_range.0,
        fit_range.1
    );
}
