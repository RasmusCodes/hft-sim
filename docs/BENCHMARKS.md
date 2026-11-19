# Benchmarks

Two parts: the matching engine (phase 0) and the simulation kernel (phase 1, at the end).

# Matching engine (phase 0)

Measured 2026-11-13. The harness is `crates/lob/benches/engine_bench.rs`.

## Setup

| Item | Value |
|---|---|
| Machine | AMD Ryzen AI 7 PRO 350 laptop (8 cores / 16 threads), 31 GB RAM |
| OS / power | Windows 11 Pro, Balanced power plan, on AC |
| Toolchain | Rust 1.99.1 (`x86_64-pc-windows-gnu`), release profile: opt-level 3, thin LTO, 1 codegen unit |
| Process | Raised to HIGH_PRIORITY_CLASS (its own priority only), thread not pinned |
| Timer | lfence-serialised `rdtsc` at 1.996 GHz (invariant TSC), calibrated against `Instant`; median timer overhead (~50 ns) subtracted per sample |

## Workload

`FlowGen` (crates/lob/src/workload.rs) produces a zero-intelligence load:

* passive limit orders clustered near the touch (geometric distance, mean ≈ 3 ticks);
* about 5% liquidity-taking orders (20% market, 80% IOC limit up to 2 ticks through);
* 5% modifies (70% size-down, which keeps priority; 30% one-tick reprice, which loses it);
* cancels whose probability scales with book size, so the book mean-reverts to a target depth.

The stream is generated once (untimed) and replayed into a fresh engine per repetition.

| Scenario | Target live orders | Live at end | Levels (bid/ask) | Trades | Events/msg | User-cancelled share of accepted orders |
|---|---:|---:|---|---:|---:|---:|
| shallow | 1,000 | 983 | 19 / 18 | 433,169 | 2.23 | 80.0% |
| medium | 20,000 | 18,784 | 30 / 32 | 433,453 | 2.24 | 79.4% |
| deep | 200,000 | 187,541 | 38 / 37 | 433,266 | 2.23 | 73.9% |

Each scenario has 5,000,000 messages: about 53% new orders, 41–42% cancels and 5% modifies. Real US
equity flow cancels about 96–97% of orders (SEC MIDAS 2012–13, snippet-level). This load fills more
orders than a real market, which makes it slightly *harder* for the engine, since executions cost
more than cancels. It is a load generator, not a market model.

## Results (5 repetitions, 5M messages each)

| Scenario | Throughput, median (best / worst) | Mean per message | p50 | p90 | p99 | p99.9 |
|---|---|---:|---:|---:|---:|---:|
| shallow (~1k orders) | **8.55 M msg/s** (8.66 / 8.33) | 117 ns | 120 ns | 160 ns | 271 ns | 491 ns |
| medium (~20k orders) | **3.26 M msg/s** (3.30 / 3.15) | 306 ns | 250 ns | 501 ns | 741 ns | 1.36 µs |
| deep (~190k orders) | **2.44 M msg/s** (2.47 / 2.15) | 409 ns | 421 ns | 751 ns | 1.07 µs | 2.96 µs |

Per message type (p50 / p99, ns):

| Scenario | New, rests | New, trades | Cancel | Modify |
|---|---|---|---|---|
| shallow | 120 / 230 | 140 / 431 | 110 / 220 | 170 / 381 |
| medium | 140 / 371 | 311 / 1,383 | 431 / 711 | 471 / 972 |
| deep | 281 / 551 | 451 / 2,094 | 621 / 1,022 | 651 / 1,423 |

Latency here is the in-process cost of `MatchingEngine::process()`, including writing events to the
output vector. No network, gateway, serialisation or I/O is included.

## Noise floor: what is the engine and what is the machine

Each scenario re-runs the same harness around a busy-wait as long as the engine's median (same sample
count), so anything slower is timer quantisation or interference from the OS and other processes:

| Scenario | Samples > 10 µs: engine | Samples > 10 µs: noise floor | Share of engine time in samples > 10 µs |
|---|---:|---:|---:|
| shallow | 533 | 492 | 2% |
| medium | 1,029 | 1,038 | 1% |
| deep | 1,877 | 1,813 | 3% |

The engine's slow samples match the noise floor in number, so tails beyond about 10 µs, including the
maxima of 75 µs–1.1 ms in the raw output, are environmental. The engine's own tail is the p99–p99.9
range in the tables.

**Process priority matters a lot on this laptop.** The first full run at normal priority measured
1.75 / 0.64 / 0.45 M msg/s (shallow / medium / deep): 3–5× worse. Its worst samples were 27–105 ms,
i.e. whole scheduler quanta. Results from normal-priority runs on a shared desktop are not engine
measurements.

## Interpretation

* **Memory, not algorithmic work, sets the cost at depth.** The median goes from 120 ns to 421 ns as
  the book grows from 1k to 190k orders, while the work per message is the same: a hash lookup, one
  node, its neighbours and a level. The order arena alone is about 12 MB at 190k orders (64 bytes per
  node), beyond the per-core L2, so a cancel touches several cache lines that miss. Options if it ever
  matters are in RISKS.md (K17).
* **Executions cost more than cancels at small depth,** and at large depth cancels cost more than
  passive adds. A random cancel lands on a cold node, while new orders join hot levels near the touch.
* **For simulation capacity:** at 2.4–8.6 M messages per second, a simulated day of 10M exchange
  messages costs 1–4 s of engine time. The simulation kernel and agents, not matching, will dominate
  research runs.
* **For realism:** real exchange round trips are tens to low hundreds of microseconds (survey A1),
  two to three orders of magnitude above this in-process cost. In the simulator, exchange latency is
  a modelled parameter.
* **Not comparable to published exchange figures.** LMAX's quoted 6M orders/s (2011, its matching
  processor) used different hardware, workload and accounting. The numbers above say only that the
  engine is fast enough for its role here.

## Correctness checks inside the benchmark

* Every scenario's stream is replayed twice and the event digests must match (determinism).
* On the first 100,000 shallow-scenario messages, the naive reference model produced **identical
  events** (digest `98962bf8bdf08657`), 38× slower (1,395 ms vs 36.5 ms).

## Reproduce

```bash
cargo bench --offline -p lob --bench engine_bench
```

```bash
cargo bench --offline -p lob --bench engine_bench -- --quick
```

Options: `--pin` pins to logical CPU 2; `--normal-priority` skips the priority raise. Quick mode
(1M messages) flatters the deep scenario, because much of the run happens while the book is still
filling.

## Raw output (final run)

```text
arch x86_64 / os windows / 16 logical CPUs / thread pinned: false / high priority: true / TSC 1.996 GHz / timer overhead 100 ticks (50.1 ns)
messages per scenario: 5000000, repetitions: 5, size_of::<Event>() = 64 bytes

## shallow: book mean-reverts to ~1000 live orders
stream: generated in 1.3s; mix: 2387732 new without trade, 250583 new with trade, 2111683 cancel, 250002 modify; 433169 trades; 2.23 events/message; user cancels / accepted new orders = 80.0%; live orders peak 1050 end 983; levels at end bid 19 ask 18; event digest 02733e30a9fe5be6
throughput (wall clock, whole stream): median 8.55 M msg/s (117 ns/msg), best 8.66 M msg/s (115 ns/msg), worst 8.33 M msg/s over 5 runs
| new, no trade | 2387732 | 120 | 150 | 230 | 461 | 74622 |
| new, trades | 250583 | 140 | 260 | 431 | 671 | 26160 |
| cancel | 2111683 | 110 | 140 | 220 | 421 | 28875 |
| modify | 250002 | 170 | 230 | 381 | 611 | 24096 |
| **all** | 5000000 | 120 | 160 | 271 | 491 | 74622 |
| *noise floor: 120 ns busy-wait, same harness* | 5000000 | 130 | 130 | 150 | 210 | 203697 |
slow samples: engine 533 > 10 us and 0 > 1 ms (together 2% of measured engine time); noise floor 492 > 10 us and 0 > 1 ms
mean per message: engine 125 ns including samples > 10 us, 123 ns excluding them
reference model on the first 100000 messages: identical events (digest 98962bf8bdf08657); engine 36.5 ms, reference 1394.7 ms (38x)

## medium: book mean-reverts to ~20000 live orders
stream: generated in 3.1s; mix: 2396755 new without trade, 250836 new with trade, 2102387 cancel, 250022 modify; 433453 trades; 2.24 events/message; user cancels / accepted new orders = 79.4%; live orders peak 19237 end 18784; levels at end bid 30 ask 32; event digest 88b9741e77afe7f0
throughput (wall clock, whole stream): median 3.26 M msg/s (306 ns/msg), best 3.30 M msg/s (303 ns/msg), worst 3.15 M msg/s over 5 runs
| new, no trade | 2396755 | 140 | 230 | 371 | 571 | 116111 |
| new, trades | 250836 | 311 | 741 | 1383 | 2114 | 103406 |
| cancel | 2102387 | 431 | 541 | 711 | 1002 | 182627 |
| modify | 250022 | 471 | 621 | 972 | 1693 | 30501 |
| **all** | 5000000 | 250 | 501 | 741 | 1363 | 182627 |
| *noise floor: 250 ns busy-wait, same harness* | 5000000 | 291 | 301 | 321 | 361 | 761006 |
slow samples: engine 1029 > 10 us and 0 > 1 ms (together 1% of measured engine time); noise floor 1038 > 10 us and 0 > 1 ms
mean per message: engine 295 ns including samples > 10 us, 291 ns excluding them

## deep: book mean-reverts to ~200000 live orders
stream: generated in 4.5s; mix: 2481377 new without trade, 250659 new with trade, 2018053 cancel, 249911 modify; 433266 trades; 2.23 events/message; user cancels / accepted new orders = 73.9%; live orders peak 188823 end 187541; levels at end bid 38 ask 37; event digest f6fbfbe99770d809
throughput (wall clock, whole stream): median 2.44 M msg/s (409 ns/msg), best 2.47 M msg/s (405 ns/msg), worst 2.15 M msg/s over 5 runs
| new, no trade | 2481377 | 281 | 391 | 551 | 2805 | 1104959 |
| new, trades | 250659 | 451 | 1052 | 2094 | 3717 | 429486 |
| cancel | 2018053 | 621 | 812 | 1022 | 3206 | 661777 |
| modify | 249911 | 651 | 852 | 1423 | 3437 | 755716 |
| **all** | 5000000 | 421 | 751 | 1072 | 2956 | 1104959 |
| *noise floor: 421 ns busy-wait, same harness* | 5000000 | 471 | 481 | 501 | 721 | 1154383 |
slow samples: engine 1877 > 10 us and 1 > 1 ms (together 3% of measured engine time); noise floor 1813 > 10 us and 2 > 1 ms
mean per message: engine 481 ns including samples > 10 us, 465 ns excluding them
```
(Columns of the per-type rows: count, p50, p90, p99, p99.9, max, in ns.)

---

# Simulation kernel (phase 1)

Release profile, process at high priority, same machine as above. The harness is
`crates/sim/benches/kernel_bench.rs`.

**Workload.** A market of zero-intelligence traders (10% co-located, 60% proximity, 30% remote
latency profiles). The total order rate is fixed at about 20,000 agent actions per simulated second,
and only the number of agents changes (20, 200, 1,000). Each scenario simulates 10 s and runs twice;
the digests must match.

## Current results (2026-11-14)

| Market data | Agents | Wall time for 10 simulated s | Simulated s per wall s | Scheduler events / wall s | Feed deliveries | Market-data callbacks | Trades | Digest equal on rerun |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| private feeds, callback | 20 | 2.90 s | **3.4** | 1.47 M | 3,343,924 | 3,343,924 | 30,886 | yes |
| private feeds, callback | 200 | 38.0 s | **0.26** | 0.96 M | 35,630,142 | 35,630,142 | 33,785 | yes |
| 3 shared groups, callback | 200 | 3.67 s | **2.7** | 0.40 M | 533,367 | 35,558,180 | 33,354 | yes |
| 3 shared groups, snapshot | 20 | 0.96 s | **10.5** | 1.47 M | 500,386 | 0 | 30,640 | yes |
| 3 shared groups, snapshot | 200 | 1.34 s | **7.5** | 1.11 M | 533,367 | 0 | 33,354 | yes |
| 3 shared groups, snapshot | 1,000 | 1.89 s | **5.3** | 0.81 M | 550,074 | 0 | 38,489 | yes |

"Shared groups" means one feed group per location (co-located, proximity, remote). Each group gets
one delivery and keeps one book view per packet, instead of one per agent
([SIMULATION.md](SIMULATION.md) §3a).

## What changed

On 2026-11-13, with every agent on a private feed and before the scheduler change:

| Agents | Wall time for 10 simulated s | Simulated s per wall s |
|---:|---:|---:|
| 20 | 3.6 s | 2.8 |
| 200 | 46 s | 0.22 |
| 1,000 | 395 s | 0.025 |

The bottleneck was **market-data fan-out**. Every engine command produced one packet per subscriber,
each packet was its own scheduler event, and each subscriber applied it to its own book copy. Each
delivery cost 1.1–2.2 µs, rising with agent count as the private book copies outgrew the CPU caches.
The matching engine itself would need only about 2 ms per simulated second.

Two changes, both verified bit-identical for existing configurations by a golden-digest test:

1. **Smaller scheduler entries.** The heap holds 24-byte `(time, seq, slot)` keys; payloads live in a
   slot table. Private-feed markets got 17–18% faster (3.6 → 2.9 s, 46 → 38 s).
2. **Shared feed groups.** At 1,000 agents: 395 s → 1.9 s, about **200× faster**, now 5.3× faster
   than real time. At 200 agents: 46 s → 1.3 s, 34× faster. Deliveries fall 67-fold.

**Reading the table.**
* *Callback versus snapshot inside groups.* At 200 agents the two produced the same market (33,354
  trades, identical deliveries), as they should: these traders act on timers, not on packets. The
  callback version is slower only because it invokes 35.6 M callbacks.
* *Private versus shared feeds.* These trade slightly differently (e.g. 30,886 vs 30,640 at 20
  agents). Shared groups have common jitter within a location, private feeds independent jitter.
  Both are legitimate latency models; with constant latencies they give identical markets (tested).

**What remains expensive:** many agents that genuinely need a callback on every packet (event-driven
strategies). That costs one callback per member per packet, about 0.08 µs each in the shared-callback
row. Conflated feeds for slow subscribers are still not built.

The demo (`cargo run --release -p sim --example demo`: 30 agents on private feeds, 1 simulated second,
758,302 deliveries) took 0.57 s of wall time before the scheduler change.

Reproduce:

```bash
cargo bench --offline -p sim --bench kernel_bench
```

```bash
cargo bench --offline -p sim --bench kernel_bench -- --quick
```
