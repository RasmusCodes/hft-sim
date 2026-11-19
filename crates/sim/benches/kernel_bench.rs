//! Simulation-kernel capacity: how much simulated market activity one thread can process.
//!
//! ```text
//! cargo bench -p sim --bench kernel_bench              # 10 s of simulated time per scenario
//! cargo bench -p sim --bench kernel_bench -- --quick   # 2 s
//! ```
//!
//! Each scenario is a market of zero-intelligence agents (10% co-located, 60% proximity, 30%
//! remote latency profiles) with the same total order rate, ~20,000 actions per simulated
//! second. What changes is the number of agents and how they receive market data:
//! * `private, callback`: each agent has its own feed link, book copy and per-packet callback;
//! * `3 groups, callback`: one shared feed group per location (one delivery, one book view per
//!   group), every member still called on every packet;
//! * `3 groups, snapshot`: the same groups, members read the view only when they act.
//! Every scenario runs twice and must produce the same digest.

use std::time::Instant;

use lob::InstrumentSpec;
use sim::agents::ZeroIntelligence;
use sim::{AgentConfig, FeedMode, InstrumentConfig, LatencyProfile, SimBuilder, SimStats};

#[cfg(windows)]
fn raise_priority() -> bool {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn SetPriorityClass(process: *mut c_void, class: u32) -> i32;
    }
    // SAFETY: pseudo-handle of the current process; the call only reads its arguments.
    unsafe { SetPriorityClass(GetCurrentProcess(), 0x0000_0080) != 0 }
}

#[cfg(not(windows))]
fn raise_priority() -> bool {
    false
}

#[derive(Clone, Copy)]
enum Feeds {
    Private,
    Groups(FeedMode),
}

fn run(feeds: Feeds, agents: usize, sim_seconds: i64, seed: u64) -> (SimStats, u64, f64) {
    let profiles = [LatencyProfile::colocated(), LatencyProfile::proximity(), LatencyProfile::remote()];
    let mut b = SimBuilder::new(seed);
    let i = b.add_instrument(InstrumentConfig::new(InstrumentSpec::example()));
    let groups: Vec<_> = profiles.iter().map(|p| b.add_feed_group(p.feed.clone())).collect();
    let mean_interval_ns = agents as f64 * 50_000.0; // ~20,000 actions per simulated second in total
    for a in 0..agents {
        let location = match a % 10 {
            0 => 0,
            1..=6 => 1,
            _ => 2,
        };
        let mut zi = ZeroIntelligence::new(i);
        zi.mean_interval_ns = mean_interval_ns;
        let cfg = AgentConfig::new(profiles[location].clone());
        let cfg = match feeds {
            Feeds::Private => cfg.subscribe(i),
            Feeds::Groups(mode) => cfg.subscribe_group(i, groups[location], mode),
        };
        b.add_agent(zi, cfg);
    }
    let mut sim = b.build().expect("valid market");
    let t0 = Instant::now();
    let stats = sim.run_until(sim_seconds * 1_000_000_000);
    (stats, sim.digest(), t0.elapsed().as_secs_f64())
}

fn main() {
    let quick = std::env::args().any(|a| a == "--quick");
    let sim_seconds: i64 = if quick { 2 } else { 10 };
    let high = raise_priority();
    println!("# sim kernel benchmark");
    println!();
    println!("{sim_seconds} s simulated per scenario; ~20,000 agent actions per simulated second in total; high priority: {high}");
    println!();
    println!("| market data | agents | wall s | sim s / wall s | scheduler events/s | feed deliveries | market-data callbacks | trades | digest equal on rerun |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---|");
    let scenarios = [
        ("private, callback", Feeds::Private, 20usize),
        ("private, callback", Feeds::Private, 200),
        ("3 groups, callback", Feeds::Groups(FeedMode::Callback), 200),
        ("3 groups, snapshot", Feeds::Groups(FeedMode::Snapshot), 20),
        ("3 groups, snapshot", Feeds::Groups(FeedMode::Snapshot), 200),
        ("3 groups, snapshot", Feeds::Groups(FeedMode::Snapshot), 1_000),
    ];
    for (label, feeds, agents) in scenarios {
        let (s, d1, wall) = run(feeds, agents, sim_seconds, 7);
        let (_, d2, _) = run(feeds, agents, sim_seconds, 7);
        println!(
            "| {label} | {agents} | {wall:.2} | {:.1} | {:.2} M | {} | {} | {} | {} |",
            sim_seconds as f64 / wall,
            s.scheduler_events as f64 / wall / 1e6,
            s.feed_packets_delivered,
            s.market_data_callbacks,
            s.trades,
            if d1 == d2 { "yes" } else { "NO" }
        );
        assert_eq!(d1, d2, "kernel is not deterministic");
    }
}
