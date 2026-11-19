//! Run the market-making ladder in the calibrated background market: each step alone, on the same
//! seeds (common random numbers), on one or both backgrounds.
//!
//! ```text
//! cargo run --release -p agents --example mm_ladder                                  # all steps, both backgrounds, 4 seeds × 5 min
//! cargo run --release -p agents --example mm_ladder -- --steps 1,5,10 --seeds 8 --minutes 10
//! ```
//!
//! Options: `--steps` (comma list of 1–10), `--seeds`, `--seed` (first), `--minutes`, `--threads`,
//! `--background` (`both`, `default` or `zi`), `--unit-lots` (lots per quote unit, default 5),
//! `--informed` (number of informed traders added to the market, default 0).
//!
//! The background has no informed aggressive flow of its own: its executors trade in random
//! directions. Without `--informed`, a passive market maker therefore meets almost no adverse
//! selection, so its profits say little.
//!
//! THESE ARE SIMULATION RESULTS ON A SYNTHETIC MARKET. They say how the strategies behave in this
//! simulator; they are not evidence that any strategy is profitable in a real market. The default
//! background's 1-second mean reversion is a known artefact (docs/STYLIZED_FACTS.md §5), which is why
//! every result is shown on both backgrounds.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use agents::mm::{LADDER, LadderParams, MarketMaker, MmStats, QuotingStrategy};
use agents::{BackgroundConfig, CalibratedMarket, InformedTrader, Limits, Perception, calibrated_market};
use sim::{AgentConfig, FeedMode, LatencyProfile};

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|k| args.get(k + 1)).cloned()
}

fn num(name: &str, default: i64) -> i64 {
    arg(name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[derive(Clone, Debug)]
struct Outcome {
    step: usize,
    background: &'static str,
    seed: u64,
    pnl: f64,
    position: i64,
    stats: MmStats,
    markout: f64,
    markout_count: u64,
    sigma: Option<f64>,
    intensity: Option<(f64, f64)>,
}

fn run(step: usize, background: &'static str, seed: u64, minutes: i64, unit_lots: i64, informed: usize) -> Outcome {
    let end = minutes * 60 * 1_000_000_000;
    let cfg = if background == "default" { BackgroundConfig::default() } else { BackgroundConfig::zero_intelligence() };
    let CalibratedMarket { mut builder, instrument: i, value, background: bg } = calibrated_market(seed, 1.0, &cfg);
    for _ in 0..informed {
        // Value with half a tick of noise, at proximity latency; takes quotes at least a tick
        // through its estimate, two lots at a time.
        let mut t = InformedTrader::new(i, value);
        (t.perception, t.threshold_ticks, t.clip_lots) = (Perception { noise_sd: 0.5 }, 1, 2);
        t.limits = Limits { max_position: 200, max_order_qty: 10, max_open_orders: 5, capital: i128::MAX };
        let c = AgentConfig::new(LatencyProfile::proximity()).subscribe_group(i, bg.groups[1], FeedMode::Snapshot).with_signal(value);
        builder.add_agent(t, c);
    }
    let strategy = QuotingStrategy::ladder(step, &LadderParams::default());
    let mut mm = MarketMaker::new(i, strategy).with_unit_lots(unit_lots);
    mm.stop_at = Some(end - 1_000_000_000);
    let id = builder.add_agent(mm, AgentConfig::new(LatencyProfile::colocated()).subscribe(i));
    let mut sim = builder.build().expect("valid market");
    sim.run_until(end);
    let mark = sim.engine(i).mid().unwrap_or(0.0);
    let ledger = sim.ledger(id, i);
    let pnl = (ledger.cash as f64 + ledger.position as f64 * mark - ledger.fees as f64 / 10_000.0) / 100.0;
    let m = sim.agent::<MarketMaker>(id).expect("market maker");
    Outcome {
        step,
        background,
        seed,
        pnl,
        position: ledger.position,
        stats: m.stats,
        markout: m.markouts.total,
        markout_count: m.markouts.count,
        sigma: m.vol.sigma(),
        intensity: m.intensity.fit(),
    }
}

fn mean_se(x: &[f64]) -> (f64, f64) {
    let n = x.len() as f64;
    let m = x.iter().sum::<f64>() / n;
    let v = if x.len() > 1 { x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (n - 1.0) } else { 0.0 };
    (m, (v / n).sqrt())
}

fn main() {
    let steps: Vec<usize> = arg("--steps")
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).filter(|x| (1..=10).contains(x)).collect())
        .unwrap_or_else(|| (1..=10).collect());
    let seeds = num("--seeds", 4).max(1) as u64;
    let first = num("--seed", 2026) as u64;
    let minutes = num("--minutes", 5).max(1);
    let threads = num("--threads", 4).max(1) as usize;
    let unit_lots = num("--unit-lots", 5).max(1);
    let informed = num("--informed", 0).max(0) as usize;
    let backgrounds: Vec<&'static str> = match arg("--background").as_deref() {
        Some("default") => vec!["default"],
        Some("zi") => vec!["zero-intelligence"],
        _ => vec!["default", "zero-intelligence"],
    };
    let jobs: Vec<(usize, &'static str, u64)> =
        steps.iter().flat_map(|&s| backgrounds.iter().flat_map(move |&b| (0..seeds).map(move |k| (s, b, first + k)))).collect();
    let next = AtomicUsize::new(0);
    let done = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads.min(jobs.len()) {
            scope.spawn(|| {
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(s, b, seed)) = jobs.get(k) else { break };
                    let o = run(s, b, seed, minutes, unit_lots, informed);
                    done.lock().expect("no worker panicked").push(o);
                }
            });
        }
    });
    let mut out = done.into_inner().expect("no worker panicked");
    out.sort_by_key(|o| (o.background, o.step, o.seed));

    println!(
        "# market-making ladder: {} seeds × {minutes} simulated minutes from seed {first}, {unit_lots} lots per unit, {informed} informed traders",
        seeds
    );
    println!("SIMULATION RESULTS ON A SYNTHETIC MARKET: not evidence of real-market profitability.");
    for &b in &backgrounds {
        println!();
        println!("## background: {b}");
        println!();
        println!("| step | strategy | PnL $ (mean ± se) | vs previous step $ | fills | volume | spread capture $ | mark-out 1 s (ticks/unit) | max |pos| | two-sided | orders | cancels | rejects | σ̂ | Â, k̂ |");
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
        let mut prev: Option<Vec<f64>> = None;
        for &s in &steps {
            let rows: Vec<&Outcome> = out.iter().filter(|o| o.background == b && o.step == s).collect();
            let pnl: Vec<f64> = rows.iter().map(|o| o.pnl).collect();
            let (m, se) = mean_se(&pnl);
            let diff = prev.as_ref().map(|p| {
                let d: Vec<f64> = pnl.iter().zip(p).map(|(a, b)| a - b).collect();
                let (dm, dse) = mean_se(&d);
                format!("{dm:+.2} ± {dse:.2}")
            });
            let avg = |f: &dyn Fn(&Outcome) -> f64| rows.iter().map(|o| f(o)).sum::<f64>() / rows.len() as f64;
            let markout = {
                let (tot, cnt): (f64, u64) = rows.iter().fold((0.0, 0), |a, o| (a.0 + o.markout, a.1 + o.markout_count));
                if cnt > 0 { format!("{:+.3}", tot / cnt as f64) } else { "n/a".into() }
            };
            let sig = rows.iter().filter_map(|o| o.sigma).collect::<Vec<f64>>();
            let inten = rows.iter().filter_map(|o| o.intensity).collect::<Vec<(f64, f64)>>();
            println!(
                "| {s} | {} | {m:+.2} ± {se:.2} | {} | {:.0} | {:.0} | {:+.2} | {markout} | {:.0} | {:.0}% | {:.0} | {:.0} | {:.0} | {} | {} |",
                LADDER[s - 1],
                diff.unwrap_or_else(|| "–".into()),
                avg(&|o| o.stats.fills as f64),
                avg(&|o| (o.stats.bought + o.stats.sold) as f64),
                avg(&|o| o.stats.spread_capture / 100.0),
                avg(&|o| o.stats.max_abs_position as f64),
                100.0 * avg(&|o| o.stats.two_sided_share()),
                avg(&|o| o.stats.orders_sent as f64),
                avg(&|o| o.stats.cancels_sent as f64),
                avg(&|o| o.stats.post_only_rejects as f64),
                if sig.is_empty() { "n/a".into() } else { format!("{:.2}", sig.iter().sum::<f64>() / sig.len() as f64) },
                if inten.is_empty() {
                    "n/a".into()
                } else {
                    format!("{:.1}, {:.2}", inten.iter().map(|x| x.0).sum::<f64>() / inten.len() as f64, inten.iter().map(|x| x.1).sum::<f64>() / inten.len() as f64)
                },
            );
            prev = Some(pnl);
        }
        let ends: Vec<i64> = out.iter().filter(|o| o.background == b).map(|o| o.position).collect();
        println!();
        println!("final positions (should be within ±bound × unit): min {}, max {}", ends.iter().min().unwrap_or(&0), ends.iter().max().unwrap_or(&0));
    }
}
