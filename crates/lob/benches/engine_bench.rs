//! Matching-engine benchmark: throughput and per-message latency on a synthetic flow.
//!
//! ```text
//! cargo bench -p lob --bench engine_bench              # 5M messages x 3 book depths, 5 reps
//! cargo bench -p lob --bench engine_bench -- --quick   # 1M messages, 3 reps
//!   options: --pin (pin to logical CPU 2), --normal-priority (do not raise process priority)
//! ```
//!
//! The message stream is generated up front with `FlowGen` (excluded from timing) and
//! replayed into a fresh engine for every repetition. Throughput is wall-clock time for the
//! whole replay. Latency is measured per `process()` call with the CPU time-stamp counter
//! (lfence-serialised `rdtsc`, calibrated against `Instant`), minus the median timer
//! overhead. Numbers include writing events into the output vector; there is no I/O,
//! no network and no gateway in the measured path.
//!
//! By default the process raises itself to HIGH_PRIORITY_CLASS on Windows: at normal
//! priority, background work preempted the benchmark thread for whole scheduler quanta and
//! the mean was dominated by those pauses. Each scenario also reports a noise floor (the
//! same harness around a busy-wait as long as the engine's median), which shows how much of
//! the tail is the environment rather than the engine.

use std::hint::black_box;
use std::time::Instant;

use lob::workload::{FlowConfig, FlowGen};
use lob::{Command, Digest, Event, EventKind, InstrumentSpec, MatchingEngine, Phase, ReferenceEngine, Side};

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn cycles() -> u64 {
    // SAFETY: lfence and rdtsc have no memory-safety preconditions on x86_64.
    #[allow(unused_unsafe)]
    unsafe {
        core::arch::x86_64::_mm_lfence();
        let t = core::arch::x86_64::_rdtsc();
        core::arch::x86_64::_mm_lfence();
        t
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn cycles() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

#[cfg(windows)]
mod os {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> *mut c_void;
        fn GetCurrentProcess() -> *mut c_void;
        fn SetThreadAffinityMask(thread: *mut c_void, mask: usize) -> usize;
        fn SetPriorityClass(process: *mut c_void, class: u32) -> i32;
    }

    /// Restrict the calling thread to one logical CPU.
    pub fn pin_to_core(core: usize) -> bool {
        // SAFETY: pseudo-handle of the calling thread; the call only reads its arguments.
        unsafe { SetThreadAffinityMask(GetCurrentThread(), 1usize << core) != 0 }
    }

    /// Raise this process (only this process) to HIGH_PRIORITY_CLASS so that ordinary
    /// background work preempts it less often. No system setting is changed.
    pub fn raise_priority() -> bool {
        const HIGH_PRIORITY_CLASS: u32 = 0x0000_0080;
        // SAFETY: pseudo-handle of the current process; the call only reads its arguments.
        unsafe { SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS) != 0 }
    }
}

#[cfg(not(windows))]
mod os {
    pub fn pin_to_core(_core: usize) -> bool {
        false
    }
    pub fn raise_priority() -> bool {
        false
    }
}

/// Control measurement: the same timing harness around a fixed busy-wait of about `ns`
/// nanoseconds. Anything beyond the payload is timer cost plus interference from the OS
/// and other processes, so it bounds how much of the engine's tail is environmental.
fn noise_floor(ticks_per_ns: f64, overhead: u64, ns: f64, n: usize) -> Vec<u64> {
    let spin = (ns * ticks_per_ns) as u64;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let a = cycles();
        let target = a + spin;
        while cycles() < target {
            std::hint::spin_loop();
        }
        let b = cycles();
        v.push((b - a).saturating_sub(overhead));
    }
    v.sort_unstable();
    v
}

/// Time-stamp-counter ticks per nanosecond.
fn calibrate() -> f64 {
    let t0 = Instant::now();
    let c0 = cycles();
    while t0.elapsed().as_millis() < 300 {
        std::hint::spin_loop();
    }
    let c1 = cycles();
    (c1 - c0) as f64 / t0.elapsed().as_nanos() as f64
}

/// Median cost of an empty timed region, in ticks.
fn timer_overhead() -> u64 {
    let mut v: Vec<u64> = (0..200_000)
        .map(|_| {
            let a = cycles();
            let b = cycles();
            b - a
        })
        .collect();
    v.sort_unstable();
    v[v.len() / 2]
}

const CLASSES: [&str; 5] = ["new, no trade", "new, trades", "cancel", "modify", "other"];

fn classify(cmd: &Command, events: &[Event]) -> usize {
    match cmd {
        Command::New(_) => {
            if events.iter().any(|e| matches!(e.kind, EventKind::Trade { .. })) {
                1
            } else {
                0
            }
        }
        Command::Cancel { .. } => 2,
        Command::Modify { .. } => 3,
        _ => 4,
    }
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

struct StreamStats {
    messages: usize,
    by_class: [usize; 5],
    trades: usize,
    events: usize,
    accepted_new: usize,
    cancelled_by_user: usize,
    end_live: usize,
    end_levels: (usize, usize),
    peak_live: usize,
    digest: u64,
}

fn verify_and_describe(spec: InstrumentSpec, cap: usize, stream: &[(i64, Command)]) -> StreamStats {
    let mut eng = MatchingEngine::with_capacity(spec, Phase::Continuous, cap).expect("spec");
    let mut out = Vec::with_capacity(64);
    let mut digest = Digest::new();
    let mut s = StreamStats {
        messages: stream.len(),
        by_class: [0; 5],
        trades: 0,
        events: 0,
        accepted_new: 0,
        cancelled_by_user: 0,
        end_live: 0,
        end_levels: (0, 0),
        peak_live: 0,
        digest: 0,
    };
    for (ts, cmd) in stream {
        out.clear();
        eng.process(*ts, cmd, &mut out).expect("valid stream");
        s.by_class[classify(cmd, &out)] += 1;
        s.events += out.len();
        for e in &out {
            digest.update(e);
            match e.kind {
                EventKind::Trade { .. } => s.trades += 1,
                EventKind::Accepted { .. } => s.accepted_new += 1,
                EventKind::Cancelled { reason: lob::CancelReason::Requested, .. } => s.cancelled_by_user += 1,
                _ => {}
            }
        }
        s.peak_live = s.peak_live.max(eng.live_orders());
    }
    s.end_live = eng.live_orders();
    s.end_levels = (eng.level_count(Side::Buy), eng.level_count(Side::Sell));
    s.digest = digest.value();
    s
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let quick = flag("--quick");
    let n: usize = if quick { 1_000_000 } else { 5_000_000 };
    let reps = if quick { 3 } else { 5 };
    let pinned = flag("--pin") && os::pin_to_core(2);
    let high_priority = !flag("--normal-priority") && os::raise_priority();
    let ticks_per_ns = calibrate();
    let overhead = timer_overhead();

    println!("# lob engine benchmark");
    println!();
    println!(
        "arch {} / os {} / {} logical CPUs / thread pinned: {pinned} / high priority: {high_priority} / TSC {:.3} GHz / timer overhead {overhead} ticks ({:.1} ns)",
        std::env::consts::ARCH,
        std::env::consts::OS,
        std::thread::available_parallelism().map(|p| p.get()).unwrap_or(0),
        ticks_per_ns,
        overhead as f64 / ticks_per_ns
    );
    println!("messages per scenario: {n}, repetitions: {reps}, size_of::<Event>() = {} bytes", std::mem::size_of::<Event>());

    let spec = InstrumentSpec::example();
    let scenarios: [(&str, usize); 3] = [("shallow", 1_000), ("medium", 20_000), ("deep", 200_000)];
    for (name, target_live) in scenarios {
        let cfg = FlowConfig { target_live, ..FlowConfig::default() };
        let t_gen = Instant::now();
        let stream = FlowGen::generate(42, spec, cfg, n);
        let gen_s = t_gen.elapsed().as_secs_f64();
        let cap = target_live * 2;
        let stats = verify_and_describe(spec, cap, &stream);
        let again = verify_and_describe(spec, cap, &stream);
        assert_eq!(stats.digest, again.digest, "engine is not deterministic");

        println!();
        println!("## {name}: book mean-reverts to ~{target_live} live orders");
        println!();
        println!(
            "stream: generated in {gen_s:.1}s; mix: {} new without trade, {} new with trade, {} cancel, {} modify; \
             {} trades; {:.2} events/message; user cancels / accepted new orders = {:.1}%; \
             live orders peak {} end {}; levels at end bid {} ask {}; event digest {:016x}",
            stats.by_class[0],
            stats.by_class[1],
            stats.by_class[2],
            stats.by_class[3],
            stats.trades,
            stats.events as f64 / stats.messages as f64,
            100.0 * stats.cancelled_by_user as f64 / stats.accepted_new.max(1) as f64,
            stats.peak_live,
            stats.end_live,
            stats.end_levels.0,
            stats.end_levels.1,
            stats.digest
        );

        // Throughput.
        let mut rates = Vec::with_capacity(reps);
        for _ in 0..reps {
            let mut eng = MatchingEngine::with_capacity(spec, Phase::Continuous, cap).expect("spec");
            let mut out: Vec<Event> = Vec::with_capacity(64);
            let t0 = Instant::now();
            for (ts, cmd) in &stream {
                out.clear();
                eng.process(*ts, cmd, &mut out).expect("valid stream");
                black_box(&out);
            }
            let secs = t0.elapsed().as_secs_f64();
            black_box(eng.live_orders());
            rates.push(n as f64 / secs);
        }
        rates.sort_by(|a, b| a.total_cmp(b));
        let median = rates[rates.len() / 2];
        let best = rates[rates.len() - 1];
        println!();
        println!(
            "throughput (wall clock, whole stream): median {:.2} M msg/s ({:.0} ns/msg), best {:.2} M msg/s ({:.0} ns/msg), worst {:.2} M msg/s over {reps} runs",
            median / 1e6,
            1e9 / median,
            best / 1e6,
            1e9 / best,
            rates[0] / 1e6
        );

        // Per-message latency.
        let mut samples: Vec<Vec<u64>> = vec![Vec::new(); CLASSES.len()];
        {
            let mut eng = MatchingEngine::with_capacity(spec, Phase::Continuous, cap).expect("spec");
            let mut out: Vec<Event> = Vec::with_capacity(64);
            for (ts, cmd) in &stream {
                out.clear();
                let a = cycles();
                eng.process(*ts, cmd, &mut out).expect("valid stream");
                let b = cycles();
                samples[classify(cmd, &out)].push((b - a).saturating_sub(overhead));
            }
        }
        println!();
        println!("| message type | count | p50 ns | p90 ns | p99 ns | p99.9 ns | max ns |");
        println!("|---|---:|---:|---:|---:|---:|---:|");
        let mut all: Vec<u64> = Vec::with_capacity(n);
        for (i, v) in samples.iter_mut().enumerate() {
            if v.is_empty() {
                continue;
            }
            v.sort_unstable();
            all.extend_from_slice(v);
            let ns = |p: f64| percentile(v, p) as f64 / ticks_per_ns;
            println!(
                "| {} | {} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} |",
                CLASSES[i],
                v.len(),
                ns(0.5),
                ns(0.9),
                ns(0.99),
                ns(0.999),
                *v.last().expect("non-empty") as f64 / ticks_per_ns
            );
        }
        all.sort_unstable();
        let ns = |p: f64| percentile(&all, p) as f64 / ticks_per_ns;
        println!(
            "| **all** | {} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} |",
            all.len(),
            ns(0.5),
            ns(0.9),
            ns(0.99),
            ns(0.999),
            *all.last().expect("non-empty") as f64 / ticks_per_ns
        );
        let total_engine: f64 = all.iter().map(|&t| t as f64).sum::<f64>() / ticks_per_ns;
        let floor = noise_floor(ticks_per_ns, overhead, ns(0.5), all.len());
        let fns = |p: f64| percentile(&floor, p) as f64 / ticks_per_ns;
        println!(
            "| *noise floor: {:.0} ns busy-wait, same harness* | {} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} |",
            ns(0.5),
            floor.len(),
            fns(0.5),
            fns(0.9),
            fns(0.99),
            fns(0.999),
            *floor.last().expect("non-empty") as f64 / ticks_per_ns
        );
        let slow = |v: &[u64], us: f64| v.iter().filter(|&&t| t as f64 / ticks_per_ns > us * 1e3).count();
        let sum_above = |v: &[u64], us: f64| -> f64 {
            v.iter().map(|&t| t as f64 / ticks_per_ns).filter(|&t| t > us * 1e3).sum::<f64>()
        };
        println!();
        println!(
            "slow samples: engine {} > 10 us and {} > 1 ms (together {:.0}% of measured engine time); noise floor {} > 10 us and {} > 1 ms",
            slow(&all, 10.0),
            slow(&all, 1000.0),
            100.0 * sum_above(&all, 10.0) / total_engine,
            slow(&floor, 10.0),
            slow(&floor, 1000.0)
        );
        println!(
            "mean per message: engine {:.0} ns including samples > 10 us, {:.0} ns excluding them",
            total_engine / all.len() as f64,
            (total_engine - sum_above(&all, 10.0)) / (all.len() - slow(&all, 10.0)) as f64
        );

        // Naive reference model on a prefix: speed ratio and identical output.
        if target_live <= 1_000 {
            let prefix = &stream[..stream.len().min(100_000)];
            let mut eng = MatchingEngine::with_capacity(spec, Phase::Continuous, cap).expect("spec");
            let mut refm = ReferenceEngine::new(spec, Phase::Continuous).expect("spec");
            let (mut de, mut dr) = (Digest::new(), Digest::new());
            let mut out = Vec::new();
            let t0 = Instant::now();
            for (ts, cmd) in prefix {
                out.clear();
                eng.process(*ts, cmd, &mut out).expect("valid stream");
                out.iter().for_each(|e| de.update(e));
            }
            let te = t0.elapsed().as_secs_f64();
            let t0 = Instant::now();
            for (ts, cmd) in prefix {
                out.clear();
                refm.process(*ts, cmd, &mut out).expect("valid stream");
                out.iter().for_each(|e| dr.update(e));
            }
            let tr = t0.elapsed().as_secs_f64();
            assert_eq!(de.value(), dr.value(), "engine and reference disagree on the benchmark stream");
            println!();
            println!(
                "reference model on the first {} messages: identical events (digest {:016x}); engine {:.1} ms, reference {:.1} ms ({:.0}x)",
                prefix.len(),
                de.value(),
                te * 1e3,
                tr * 1e3,
                tr / te
            );
        }
    }
}
