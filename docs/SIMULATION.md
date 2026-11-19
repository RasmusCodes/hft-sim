# Simulation kernel (phase 1)

Specification of `crates/sim`: how simulated time passes, how messages travel, what agents can see,
and what is guaranteed to be reproducible. The matching rules themselves are in
[MATCHING_RULES.md](MATCHING_RULES.md).

## 1. Time and ordering

* Time is an `i64` count of nanoseconds from the start of the run (t = 0).
* One scheduler holds every future event, ordered by **(time, insertion sequence)**. Events at the
  same nanosecond run in the order they were scheduled. Example (tested): an agent's timer set at
  t = 0 for t = 101,000 runs *before* a market-data packet that was scheduled at t = 1,000 to arrive
  at t = 101,000.
* At t = 0, agents start in id order, then signals take their first step, then session events
  fire.
* One simulation runs on one thread. Parallelism comes from running independent simulations side
  by side, one `hftsim` process each (phase 4, `python/hftsim`), which keeps each run
  deterministic.
* The scheduler's heap holds only small `(time, sequence, slot)` keys; event payloads sit in a
  reusable slot table. Order depends on `(time, sequence)` alone. The golden-digest test confirms that
  this change, and the feed refactor in §3a, left every earlier run bit-identical.

## 2. Path of a message

```
agent callback at t
 └─ compute delay (agent profile; sampled once per callback)
     └─ order_out link ──────────────► gateway session: one message at a time, gateway_ns each
                                          throttle → pre-trade risk → client id ↦ exchange id
          ◄── order_in link ─── reply ─┤  (throttle/risk rejections and "unknown order" answers)
                                          └─ hop_ns (constant) ─► matching-engine queue
                                                                  (one per instrument, FIFO)
            service starts: the command takes effect on the book at this instant
            service time = engine_base_ns + engine_per_event_ns × events produced
            service ends: results leave the engine
 private events ─► owner's gateway session ─► execution reports ─► order_in link ─► agent
 public events  ─► one packet per command ─► each subscriber's feed link ─► agent's book view
```

| Hop | Model | Notes |
|---|---|---|
| Compute | `LatencyProfile::compute` | The agent "thinks" once per callback; all requests from that callback then leave in order |
| Agent → gateway | `order_out` link | One link per agent |
| Gateway | `ExchangeTiming::gateway_ns` per message | Each agent's session is its own one-at-a-time server, so bursts from one participant queue there |
| Gateway → engine | `hop_ns`, constant | Constant, so the engine receives messages in forwarding order. This is what makes gateway-assigned exchange ids exact |
| Engine | `engine_base_ns + engine_per_event_ns × events` | Per-instrument queue: simultaneous orders from many participants wait their turn; big sweeps and uncrosses take longer |
| Exchange → agent | `order_in` link | Execution reports for one command are delivered together |
| Feed | One link per feed: an agent's private feed uses the agent's `feed` model; a shared feed group uses the group's model | One packet per engine command, handed to every feed of the instrument (see §3a) |
| Signal | `signal` link per (agent, signal) | Each update reaches each subscriber with its own delay |

**Links never reorder.** A message cannot overtake the one sent before it on the same link: arrival
= max(send time + sampled delay, previous arrival). Different links are independent, so a
participant's market data and its execution reports can arrive in either order, as in reality.

**Latency models** (`LatencyModel`): constant; uniform; floor + log-normal jitter (median and log
standard deviation); floor + Pareto/Lomax jitter (power-law tail, capped at 1,000 s); and any model
plus random spikes. Delays are integer nanoseconds.

**Profiles** (`LatencyProfile::colocated`, `proximity`, `remote`) are illustrative orders of magnitude,
not calibrations:

* co-located: about 1 µs per path;
* proximity: about 50–60 µs;
* remote: about 4 ms, the order of the Chicago–New Jersey microwave route.

The default `ExchangeTiming` (gateway 8 µs, hop 2 µs, engine 3 µs + 0.15 µs per event) puts a
co-located round trip in the tens of microseconds, the order of magnitude published for European
venues ([RESEARCH_SURVEY.md](RESEARCH_SURVEY.md) A1). It is not calibrated to any venue.

## 3. What an agent sees and does

An agent implements `Agent` with optional callbacks: `on_start`, `on_market_data`, `on_report`,
`on_timer` and `on_signal`. Through `Ctx` it can:

* read **its own book view** (`ctx.book(i)`), an `MboBook` the kernel rebuilds from the agent's
  delayed feed. The packet that triggers `on_market_data` is already applied. The agent never sees
  the exchange's book;
* read **its own order and position view** (`ctx.orders()`), rebuilt from its own requests and
  execution reports, so it is also delayed. After all messages settle, it equals the exchange's
  ledger (tested);
* send requests using **its own client order ids**. Cancels and modifies may follow a new order
  immediately, before the acknowledgement returns; the gateway maps ids;
* set timers and use its own random stream.

The kernel calls agents one at a time. A callback runs instantly in simulated time; its cost is
modelled by the compute delay applied to whatever it sends.

## 3a. Market-data feeds: private or shared

Every subscription goes through a **feed**: one instrument's packets, one link, one book view.

* **Private feed** (`AgentConfig::subscribe`). One member, the agent's own `feed` latency, and the
  agent's own book copy. Each subscriber's jitter is independent.
* **Shared feed group** (`SimBuilder::add_feed_group(latency)`, then `AgentConfig::subscribe_group`).
  This stands for subscribers at one location receiving the same multicast packets. Each packet takes
  **one** delay draw for the whole group, reaches every member at the same instant, and updates
  **one** book view that all members read. Jitter is common to the group, as it is for co-located
  receivers of one multicast stream. The group's latency replaces the members' own `feed` latency.

Each member chooses a mode:

* `FeedMode::Callback`: `on_market_data` on every packet, after the view is updated. Members are
  called in subscription order.
* `FeedMode::Snapshot`: no per-packet callbacks. The member reads `ctx.book(i)` when it acts (on a
  timer or a report) and sees exactly the view a callback member would see at that moment, so it pays
  nothing per packet.

**Equivalence (tested).** Twelve agents on three shared snapshot groups produce exactly the same
exchange event stream as the same agents on private callback feeds, provided each private feed has
its group's constant latency.

**Cost.** Per-packet work scales with the number of feeds, not the number of subscribers. With 1,000
agents on three location groups, the kernel runs 5.3× faster than real time instead of 0.025×
([BENCHMARKS.md](BENCHMARKS.md), kernel section).

**When to use which.**
* Use private feeds when independent per-subscriber jitter matters, or for a handful of agents.
* Use shared snapshot groups for large populations of timer-driven traders.
* Use shared callback groups for event-driven strategies at a common location.

## 4. Gateway

One session per agent:

* **Throttle**: a token bucket (burst, sustained rate per second) counting every message, including
  ones rejected later for other reasons.
* **Pre-trade risk**, per instrument:
  * maximum order quantity (new orders and modifies);
  * maximum open orders (resting or in flight);
  * maximum absolute position the session could reach if all its open orders filled
    (position + open buys + new order ≤ limit, and the same for sells).

  A rejection is reported as `GatewayRejected` and never reaches the engine.
* **Client id mapping**: the gateway assigns the exchange order id when it forwards a new order
  (the engine's next id for that instrument, which the constant hop makes exact). Unknown or finished
  client ids get `RequestRejected { UnknownOrder }`, the same "too late to cancel" answer the engine
  gives when the cancel reaches it after the fill. Which of the two answers depends on whether the
  fill had already been published when the cancel passed the gateway. Both cases are tested.
* **Ledger**: the gateway books every fill into the exchange's ground-truth `Ledger`:
  position, cash (`i128`, price units × quantity), fees in fee units (`FEE_SCALE` = 10,000 per price
  unit, maker/taker/auction rates, negative = rebate), and maker/taker/auction volumes.

## 5. Sessions and signals

* `SessionEvent::at(instrument, phase, t)` or `::between(instrument, phase, from, to)`. A random time
  is drawn uniformly per event when the simulation is built (Xetra volatility auctions end at a random
  time for this reason), and `Simulation::session_schedule()` shows the resolved times. Session commands
  join the engine queue like any other command. An invalid transition becomes a warning, not a crash.
* `SignalProcess` models information from outside the book: news, a leading market, a latent
  fundamental value. The processes:
  * `RandomWalk`: ±step at exponential intervals;
  * `JumpDiffusion`: a random walk with occasional large jumps;
  * `Tracking`: a leader plus a mean-reverting gap, republished whenever the leader moves; for
    related instruments;
  * `Path`: scripted values.

  Subscribers learn a value only when the signal publishes. The starting value is never delivered
  on its own, so a constant signal needs one `Path` point. Who learns
  first is decided by each subscriber's signal latency. The stale-quote race test is a miniature of
  research question 4. A market maker that hears the news and reaches the exchange in 2 µs cancels
  its stale quote before a 10 µs sniper arrives. With the latencies swapped, the sniper lifts the
  quote and the maker loses 40 cents a share against the new value.

## 6. Reproducibility guarantees

* **One seed, independent streams.** Every random source has its own stream
  `Rng::stream(seed, (kind, entity))`: each agent's strategy, each agent's compute delay, each link,
  each signal, each session event. Adding an agent does not change other agents' or links' draws.
  Tested: a silent extra agent leaves the exchange's event stream bit-identical. Streams are keyed by
  agent *id* (insertion order), so inserting an agent in the middle renumbers the ones after it.
* **No platform math.** Distributions are sampled through `detmath` (log, exp and the normal
  sampler built from IEEE basic operations only), not the operating system's math library. That
  library differs across platforms in the last bit, and one bit can reorder two messages.
* **No hidden nondeterminism.** No wall clock, no thread scheduling, deterministic hashers, no output
  that depends on hash-map order.
* **Fingerprint.** `Simulation::digest()` hashes every engine event and every agent callback with its
  time. Tests check that equal seeds give equal digests and different seeds different ones.

## 7. Known simplifications

* The compute delay is applied per callback, but agents are not modelled as busy servers: a second
  packet arriving while the agent is still "thinking" is handled immediately.
* Market data is market-by-order only; conflated L1/L2 feeds for slower subscribers are not built yet.
* Members of a shared feed group receive each packet at exactly the same instant (no per-member
  offset inside a location).
* Exchange internals (gateway, hop, engine) have constant service times; real systems have jitter
  there too. The hooks exist; distributions can be added the same way as for links.
* One gateway session per agent, with no cross-session risk aggregation.
* Market orders have no price protection yet (RISKS K14), so they can sweep a thin book. The phase-2
  agents send IOC limits with price protection instead.
* When the phase-1 `ZeroIntelligence` test agent draws a cancel but has nothing to cancel, the draw
  falls through to a marketable order. This is kept because the demo golden digest locks its
  behaviour; the phase-2 `NoiseTrader` does nothing in that case.
* Simulations are single-threaded and `!Send` (the kernel shares feed packets with `Rc`). Parallel
  Monte Carlo will build each simulation inside its own worker thread.
