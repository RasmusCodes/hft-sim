//! The simulation kernel: a single-threaded discrete-event loop.
//!
//! Path of one order (every delay explicit; see docs/SIMULATION.md):
//!
//! ```text
//! agent callback at t ─ compute delay ─▶ order_out link ─▶ gateway session (FIFO server:
//! throttle, risk, id mapping) ─ hop ─▶ engine queue (FIFO server per instrument; the command
//! takes effect when service starts) ─ service time ─▶ dispatch:
//!     private events → owner's gateway session → execution reports → order_in link → agent
//!     public events  → one packet per command → each subscriber's feed link → agent's MboBook
//! ```

use std::any::Any;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use lob::rng::Rng;
use lob::{
    Command, Event, EventKind, InstrumentSpec, MatchingEngine, MboBook, OrderId, Phase, SpecError,
    Timestamp,
};

use crate::accounting::{FeeSchedule, Ledger};
use crate::agent::{Agent, AgentState, Ctx, MyOrders};
use crate::detmath;
use crate::feed::{Feed, FeedGroupId, FeedId, FeedMode, Subscription};
use crate::gateway::{Origin, Outcome, RiskLimits, Session};
use crate::latency::{LatencyModel, LatencyProfile, Link};
use crate::messages::{AgentId, ExecReport, InstrumentId, ReportKind, Request, SignalId};
use crate::queue::EventQueue;
use crate::session::{SessionEvent, SessionTime};
use crate::signal::SignalProcess;

/// Exchange-side processing times. The defaults are illustrative orders of magnitude that put
/// a co-located round trip in the tens of microseconds; calibrate before drawing conclusions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExchangeTiming {
    /// Gateway processing time per message (each session is a FIFO server).
    pub gateway_ns: i64,
    /// Gateway → matching engine hop. A constant, so the engine sees messages in the order
    /// the gateways forwarded them.
    pub hop_ns: i64,
    /// Matching-engine service time per command.
    pub engine_base_ns: i64,
    /// Extra service time per event produced (sweeps and uncrosses take longer).
    pub engine_per_event_ns: i64,
}

impl Default for ExchangeTiming {
    fn default() -> Self {
        ExchangeTiming { gateway_ns: 8_000, hop_ns: 2_000, engine_base_ns: 3_000, engine_per_event_ns: 150 }
    }
}

impl ExchangeTiming {
    /// No exchange-side delay at all (for tests).
    pub fn zero() -> Self {
        ExchangeTiming { gateway_ns: 0, hop_ns: 0, engine_base_ns: 0, engine_per_event_ns: 0 }
    }
}

/// One instrument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstrumentConfig {
    /// Trading rules.
    pub spec: InstrumentSpec,
    /// Initial phase.
    pub phase: Phase,
    /// Fee schedule.
    pub fees: FeeSchedule,
}

impl InstrumentConfig {
    /// Continuous trading, no fees.
    pub fn new(spec: InstrumentSpec) -> Self {
        InstrumentConfig { spec, phase: Phase::Continuous, fees: FeeSchedule::default() }
    }

    /// Set the initial phase.
    pub fn with_phase(mut self, phase: Phase) -> Self {
        self.phase = phase;
        self
    }

    /// Set the fee schedule.
    pub fn with_fees(mut self, fees: FeeSchedule) -> Self {
        self.fees = fees;
        self
    }
}

/// How one agent connects to the exchange.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentConfig {
    /// Latency of each path.
    pub latency: LatencyProfile,
    /// Gateway limits.
    pub limits: RiskLimits,
    /// Market-data subscriptions (at most one per instrument; later duplicates are ignored).
    pub market_data: Vec<Subscription>,
    /// Signals the agent receives.
    pub signals: Vec<SignalId>,
}

impl AgentConfig {
    /// No subscriptions, no limits.
    pub fn new(latency: LatencyProfile) -> Self {
        AgentConfig { latency, limits: RiskLimits::default(), market_data: Vec::new(), signals: Vec::new() }
    }

    /// Private market-data feed over the agent's own `feed` link, with `on_market_data` for
    /// every packet.
    pub fn subscribe(self, instrument: InstrumentId) -> Self {
        self.subscribe_with(Subscription { instrument, mode: FeedMode::Callback, group: None })
    }

    /// Market data through a shared feed group (one delivery and one book view for all members;
    /// the group's latency replaces the agent's own `feed` latency).
    pub fn subscribe_group(self, instrument: InstrumentId, group: FeedGroupId, mode: FeedMode) -> Self {
        self.subscribe_with(Subscription { instrument, mode, group: Some(group) })
    }

    /// Any subscription.
    pub fn subscribe_with(mut self, subscription: Subscription) -> Self {
        self.market_data.push(subscription);
        self
    }

    /// Subscribe to a signal.
    pub fn with_signal(mut self, signal: SignalId) -> Self {
        self.signals.push(signal);
        self
    }

    /// Set gateway limits.
    pub fn with_limits(mut self, limits: RiskLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// Why a simulation could not be built.
#[derive(Clone, Debug, PartialEq)]
pub enum BuildError {
    /// Invalid instrument spec.
    Spec {
        /// Instrument.
        instrument: InstrumentId,
        /// Problem.
        error: SpecError,
    },
    /// Invalid latency model.
    Latency {
        /// Agent.
        agent: AgentId,
        /// Problem.
        error: String,
    },
    /// Invalid signal process.
    Signal {
        /// Signal.
        signal: SignalId,
        /// Problem.
        error: String,
    },
    /// Negative exchange timing.
    Timing,
    /// Reference to an instrument that does not exist.
    UnknownInstrument(InstrumentId),
    /// Reference to a signal that does not exist.
    UnknownSignal(SignalId),
    /// Reference to a feed group that does not exist.
    UnknownFeedGroup(FeedGroupId),
    /// Invalid feed-group latency model.
    FeedGroupLatency {
        /// Group.
        group: FeedGroupId,
        /// Problem.
        error: String,
    },
    /// Invalid session event.
    Session(String),
    /// More than 2^28 agents, instruments or signals.
    TooMany,
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot build simulation: {self:?}")
    }
}

impl std::error::Error for BuildError {}

/// Counters of one run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SimStats {
    /// Events taken off the scheduler.
    pub scheduler_events: u64,
    /// Agent callbacks invoked.
    pub agent_callbacks: u64,
    /// Requests sent by agents.
    pub requests: u64,
    /// Requests refused by gateways (throttle and risk).
    pub gateway_rejects: u64,
    /// Commands processed by matching engines (including session commands).
    pub engine_commands: u64,
    /// Events produced by matching engines.
    pub engine_events: u64,
    /// Trades printed.
    pub trades: u64,
    /// Execution reports delivered to agents.
    pub reports_delivered: u64,
    /// Market-data packets delivered to feeds (once per private feed or shared group).
    pub feed_packets_delivered: u64,
    /// `on_market_data` callbacks invoked.
    pub market_data_callbacks: u64,
    /// Signal updates generated.
    pub signal_updates: u64,
    /// Longest matching-engine queue observed.
    pub max_engine_queue: usize,
}

// Random-stream identifiers: one independent stream per (kind, entity), so adding an agent
// or changing one link's model never shifts anyone else's random numbers.
const S_AGENT: u64 = 1;
const S_COMPUTE: u64 = 2;
const S_ORDER_OUT: u64 = 3;
const S_ORDER_IN: u64 = 4;
const S_FEED: u64 = 5;
const S_SIGNAL_LINK: u64 = 6;
const S_SIGNAL: u64 = 7;
const S_SESSION: u64 = 8;
const S_GROUP_FEED: u64 = 9;
const MAX_ENTITIES: usize = 1 << 28;

fn stream(kind: u64, a: usize, b: usize) -> u64 {
    (kind << 56) | ((a as u64 & 0x0fff_ffff) << 28) | (b as u64 & 0x0fff_ffff)
}

/// Builds a [`Simulation`].
pub struct SimBuilder {
    seed: u64,
    timing: ExchangeTiming,
    record: bool,
    instruments: Vec<InstrumentConfig>,
    signals: Vec<SignalProcess>,
    groups: Vec<LatencyModel>,
    agents: Vec<(Box<dyn Agent>, AgentConfig)>,
    sessions: Vec<SessionEvent>,
}

impl SimBuilder {
    /// Empty market with a master seed and default exchange timing.
    pub fn new(seed: u64) -> Self {
        SimBuilder {
            seed,
            timing: ExchangeTiming::default(),
            record: false,
            instruments: Vec::new(),
            signals: Vec::new(),
            groups: Vec::new(),
            agents: Vec::new(),
            sessions: Vec::new(),
        }
    }

    /// Add a shared feed group: subscribers at one location receiving the same multicast feed.
    /// Each packet reaches all members at once after a delay drawn from `latency` (one draw per
    /// packet and instrument), and all members read one book view.
    pub fn add_feed_group(&mut self, latency: LatencyModel) -> FeedGroupId {
        self.groups.push(latency);
        (self.groups.len() - 1) as FeedGroupId
    }

    /// Exchange processing times.
    pub fn timing(&mut self, timing: ExchangeTiming) -> &mut Self {
        self.timing = timing;
        self
    }

    /// Keep every engine event (needed for [`Simulation::events`]).
    pub fn record_events(&mut self, on: bool) -> &mut Self {
        self.record = on;
        self
    }

    /// Add an instrument.
    pub fn add_instrument(&mut self, cfg: InstrumentConfig) -> InstrumentId {
        self.instruments.push(cfg);
        (self.instruments.len() - 1) as InstrumentId
    }

    /// Add a signal source.
    pub fn add_signal(&mut self, process: SignalProcess) -> SignalId {
        self.signals.push(process);
        (self.signals.len() - 1) as SignalId
    }

    /// Add an agent. Its id is also its owner id at the exchange.
    pub fn add_agent<A: Agent>(&mut self, agent: A, cfg: AgentConfig) -> AgentId {
        self.agents.push((Box::new(agent), cfg));
        (self.agents.len() - 1) as AgentId
    }

    /// Schedule a phase change.
    pub fn add_session_event(&mut self, event: SessionEvent) -> &mut Self {
        self.sessions.push(event);
        self
    }

    /// Validate and assemble.
    pub fn build(self) -> Result<Simulation, BuildError> {
        let SimBuilder { seed, timing, record, instruments, signals, groups, agents, sessions } = self;
        let t = timing;
        if t.gateway_ns < 0 || t.hop_ns < 0 || t.engine_base_ns < 0 || t.engine_per_event_ns < 0 {
            return Err(BuildError::Timing);
        }
        if [agents.len(), instruments.len(), signals.len(), groups.len()].iter().any(|&n| n >= MAX_ENTITIES) {
            return Err(BuildError::TooMany);
        }
        for (g, model) in groups.iter().enumerate() {
            model
                .validate()
                .map_err(|error| BuildError::FeedGroupLatency { group: g as FeedGroupId, error })?;
        }
        let n = instruments.len();

        let mut venues = Vec::with_capacity(n);
        let mut specs = Vec::with_capacity(n);
        for (k, cfg) in instruments.iter().enumerate() {
            let engine = MatchingEngine::new(cfg.spec, cfg.phase)
                .map_err(|error| BuildError::Spec { instrument: k as InstrumentId, error })?;
            specs.push(cfg.spec);
            venues.push(Venue {
                engine,
                fees: cfg.fees,
                queue: VecDeque::new(),
                in_service: None,
                out: Vec::new(),
                feeds: Vec::new(),
                log: Vec::new(),
            });
        }
        let mut feeds: Vec<Feed> = Vec::new();
        // Feed of each (group, instrument), created when its first member subscribes.
        let mut group_feeds: Vec<Vec<Option<FeedId>>> = vec![vec![None; n]; groups.len()];

        let mut signal_states: Vec<SignalState> = Vec::with_capacity(signals.len());
        for (k, process) in signals.into_iter().enumerate() {
            process
                .validate(k as SignalId)
                .map_err(|error| BuildError::Signal { signal: k as SignalId, error })?;
            let mut value = process.start_value();
            if let SignalProcess::Tracking { leader, .. } = process {
                value += signal_states[leader as usize].value;
                signal_states[leader as usize].followers.push(k as SignalId);
            }
            signal_states.push(SignalState {
                value,
                deviation: 0,
                rng: Rng::stream(seed, stream(S_SIGNAL, k, 0)),
                next_point: 0,
                subscribers: Vec::new(),
                followers: Vec::new(),
                process,
            });
        }

        let mut gateway = Vec::with_capacity(agents.len());
        let mut states = Vec::with_capacity(agents.len());
        let mut boxed = Vec::with_capacity(agents.len());
        for (a, (agent, cfg)) in agents.into_iter().enumerate() {
            let id = a as AgentId;
            cfg.latency.validate().map_err(|error| BuildError::Latency { agent: id, error })?;
            let mut views: Vec<Option<FeedId>> = vec![None; n];
            for sub in &cfg.market_data {
                let i = sub.instrument as usize;
                let venue = venues.get_mut(i).ok_or(BuildError::UnknownInstrument(sub.instrument))?;
                if views[i].is_some() {
                    continue;
                }
                let feed = match sub.group {
                    None => {
                        let link = Link::new(cfg.latency.feed.clone(), Rng::stream(seed, stream(S_FEED, a, i)));
                        feeds.push(Feed { instrument: sub.instrument, link, book: MboBook::new(), members: Vec::new() });
                        let f = (feeds.len() - 1) as FeedId;
                        venue.feeds.push(f);
                        f
                    }
                    Some(g) => {
                        let slot = group_feeds
                            .get_mut(g as usize)
                            .ok_or(BuildError::UnknownFeedGroup(g))?
                            .get_mut(i)
                            .expect("instrument checked above");
                        match *slot {
                            Some(f) => f,
                            None => {
                                let rng = Rng::stream(seed, stream(S_GROUP_FEED, g as usize, i));
                                let link = Link::new(groups[g as usize].clone(), rng);
                                feeds.push(Feed { instrument: sub.instrument, link, book: MboBook::new(), members: Vec::new() });
                                let f = (feeds.len() - 1) as FeedId;
                                venue.feeds.push(f);
                                *slot = Some(f);
                                f
                            }
                        }
                    }
                };
                feeds[feed as usize].members.push((id, sub.mode));
                views[i] = Some(feed);
            }
            for &s in &cfg.signals {
                let st = signal_states.get_mut(s as usize).ok_or(BuildError::UnknownSignal(s))?;
                if !st.subscribers.iter().any(|(x, _)| *x == id) {
                    let link =
                        Link::new(cfg.latency.signal.clone(), Rng::stream(seed, stream(S_SIGNAL_LINK, a, s as usize)));
                    st.subscribers.push((id, link));
                }
            }
            states.push(AgentState {
                id,
                rng: Rng::stream(seed, stream(S_AGENT, a, 0)),
                compute_rng: Rng::stream(seed, stream(S_COMPUTE, a, 0)),
                compute: cfg.latency.compute.clone(),
                order_out: Link::new(cfg.latency.order_out.clone(), Rng::stream(seed, stream(S_ORDER_OUT, a, 0))),
                order_in: Link::new(cfg.latency.order_in.clone(), Rng::stream(seed, stream(S_ORDER_IN, a, 0))),
                views,
                orders: MyOrders::new(n),
                outbox: Vec::new(),
                timers: Vec::new(),
                next_client_id: 1,
            });
            gateway.push(Session::new(id, cfg.limits, n));
            boxed.push(agent);
        }

        let mut schedule = Vec::with_capacity(sessions.len());
        for (k, ev) in sessions.iter().enumerate() {
            if ev.instrument as usize >= n {
                return Err(BuildError::UnknownInstrument(ev.instrument));
            }
            let at = match ev.at {
                SessionTime::At(t) => t,
                SessionTime::Between(lo, hi) => {
                    if lo > hi {
                        return Err(BuildError::Session(format!("event {k}: window {lo}..{hi} is empty")));
                    }
                    lo + Rng::stream(seed, stream(S_SESSION, k, 0)).range_i64(0, hi - lo)
                }
            };
            if at < 0 {
                return Err(BuildError::Session(format!("event {k}: negative time {at}")));
            }
            schedule.push((at, ev.instrument, ev.phase));
        }

        // Initial events. At equal times, earlier pushes run first: agents start (in id order)
        // before signals move and before session events.
        let mut queue = EventQueue::new();
        for a in 0..boxed.len() {
            queue.push(0, Pending::Start(a as AgentId));
        }
        for (s, st) in signal_states.iter_mut().enumerate() {
            let first = match (&st.process, st.process.mean_interval_ns()) {
                (SignalProcess::Path { points, .. }, _) => points.first().map(|p| p.0),
                (_, Some(mean)) => Some((detmath::exponential(mean, &mut st.rng).round() as i64).max(1)),
                (_, None) => None,
            };
            if let Some(t) = first {
                queue.push(t, Pending::SignalTick { signal: s as SignalId });
            }
        }
        for (k, &(at, _, _)) in schedule.iter().enumerate() {
            queue.push(at, Pending::Session { index: k });
        }

        Ok(Simulation {
            now: 0,
            timing,
            record,
            queue,
            specs,
            venues,
            next_ids: vec![1; n],
            gateway,
            agents: boxed,
            states,
            feeds,
            signals: signal_states,
            schedule,
            stats: SimStats::default(),
            digest: Fnv::default(),
            warnings: Vec::new(),
            gateway_log: Vec::new(),
        })
    }
}

/// A command waiting for, or in, matching-engine service.
#[derive(Clone, Copy, Debug)]
struct Job {
    cmd: Command,
    origin: Option<Origin>,
}

struct Venue {
    engine: MatchingEngine,
    fees: FeeSchedule,
    queue: VecDeque<Job>,
    in_service: Option<Job>,
    out: Vec<Event>,
    /// Feeds of this instrument, in creation order (the order packets are handed to them).
    feeds: Vec<FeedId>,
    log: Vec<Event>,
}

struct SignalState {
    process: SignalProcess,
    value: i64,
    /// Gap from the leader (`Tracking` only).
    deviation: i64,
    rng: Rng,
    next_point: usize,
    subscribers: Vec<(AgentId, Link)>,
    /// `Tracking` signals that follow this one.
    followers: Vec<SignalId>,
}

enum Pending {
    Start(AgentId),
    Timer { agent: AgentId, token: u64 },
    GatewayDone { agent: AgentId, req: Request },
    AtEngine { instrument: InstrumentId, job: Job },
    EngineDone { instrument: InstrumentId },
    Reports { agent: AgentId, reports: Vec<ExecReport> },
    Feed { feed: FeedId, packet: Rc<[Event]> },
    SignalTick { signal: SignalId },
    SignalArrive { agent: AgentId, signal: SignalId, value: i64 },
    Session { index: usize },
}

enum Callback<'p> {
    Start,
    Timer(u64),
    MarketData(InstrumentId, &'p [Event]),
    Report(&'p ExecReport),
    Signal(SignalId, i64),
}

/// FNV-1a over everything observable: engine events and every agent callback.
#[derive(Clone, Copy, Debug)]
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

impl Fnv {
    fn mix<T: Hash>(&mut self, value: &T) {
        value.hash(self);
    }
}

/// A running market.
pub struct Simulation {
    now: Timestamp,
    timing: ExchangeTiming,
    record: bool,
    queue: EventQueue<Pending>,
    specs: Vec<InstrumentSpec>,
    venues: Vec<Venue>,
    next_ids: Vec<OrderId>,
    gateway: Vec<Session>,
    agents: Vec<Box<dyn Agent>>,
    states: Vec<AgentState>,
    feeds: Vec<Feed>,
    signals: Vec<SignalState>,
    schedule: Vec<(Timestamp, InstrumentId, Phase)>,
    stats: SimStats,
    digest: Fnv,
    warnings: Vec<String>,
    gateway_log: Vec<GatewayRecord>,
}

/// A request the gateway answered itself (throttle, pre-trade risk, or a cancel or modify of an
/// order it already knew was done), recorded only when event recording is on. Together with the
/// engine log this shows every outcome of every request; latency-race measurement needs it,
/// because a cancel that loses a race is often refused at the gateway, not by the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GatewayRecord {
    /// Gateway time.
    pub ts: Timestamp,
    /// Requesting agent.
    pub agent: AgentId,
    /// The request.
    pub request: Request,
    /// The answer sent back.
    pub report: ReportKind,
}

impl Simulation {
    /// Process every event up to and including time `end`.
    pub fn run_until(&mut self, end: Timestamp) -> SimStats {
        while let Some(t) = self.queue.peek_time() {
            if t > end {
                break;
            }
            let (t, p) = self.queue.pop().expect("peeked");
            self.now = t;
            self.stats.scheduler_events += 1;
            self.handle(p);
        }
        self.now = self.now.max(end);
        self.stats
    }

    fn handle(&mut self, p: Pending) {
        match p {
            Pending::Start(agent) => self.call(agent, Callback::Start),
            Pending::Timer { agent, token } => self.call(agent, Callback::Timer(token)),
            Pending::GatewayDone { agent, req } => self.at_gateway(agent, req),
            Pending::AtEngine { instrument, job } => self.enqueue(instrument, job),
            Pending::EngineDone { instrument } => self.finish_job(instrument),
            Pending::Reports { agent, reports } => {
                for r in &reports {
                    self.stats.reports_delivered += 1;
                    self.states[agent as usize].orders.on_report(r);
                    self.call(agent, Callback::Report(r));
                }
            }
            Pending::Feed { feed, packet } => {
                self.stats.feed_packets_delivered += 1;
                let now = self.now;
                let f = &mut self.feeds[feed as usize];
                for e in packet.iter() {
                    if let Err(err) = f.book.apply(e) {
                        self.warnings.push(format!("t={now}: feed {feed} inconsistency {err:?}"));
                    }
                }
                let instrument = f.instrument;
                // The view is updated once; callback members then react in subscription order.
                for k in 0..self.feeds[feed as usize].members.len() {
                    let (agent, mode) = self.feeds[feed as usize].members[k];
                    if mode == FeedMode::Callback {
                        self.stats.market_data_callbacks += 1;
                        self.call(agent, Callback::MarketData(instrument, &packet));
                    }
                }
            }
            Pending::SignalTick { signal } => self.signal_tick(signal),
            Pending::SignalArrive { agent, signal, value } => self.call(agent, Callback::Signal(signal, value)),
            Pending::Session { index } => {
                let (_, instrument, phase) = self.schedule[index];
                self.enqueue(instrument, Job { cmd: Command::SetPhase { phase }, origin: None });
            }
        }
    }

    fn call(&mut self, agent: AgentId, cb: Callback<'_>) {
        let a = agent as usize;
        let now = self.now;
        self.stats.agent_callbacks += 1;
        match &cb {
            Callback::Start => self.digest.mix(&(now, agent, 0u8)),
            Callback::Timer(token) => self.digest.mix(&(now, agent, 1u8, *token)),
            Callback::MarketData(i, ev) => self.digest.mix(&(now, agent, 2u8, *i, ev.len() as u64)),
            Callback::Report(r) => self.digest.mix(&(now, agent, 3u8, **r)),
            Callback::Signal(s, v) => self.digest.mix(&(now, agent, 4u8, *s, *v)),
        }
        {
            let mut ctx = Ctx { now, state: &mut self.states[a], specs: &self.specs, feeds: &self.feeds };
            let ag = &mut self.agents[a];
            match cb {
                Callback::Start => ag.on_start(&mut ctx),
                Callback::Timer(token) => ag.on_timer(&mut ctx, token),
                Callback::MarketData(i, ev) => ag.on_market_data(&mut ctx, i, ev),
                Callback::Report(r) => ag.on_report(&mut ctx, r),
                Callback::Signal(s, v) => ag.on_signal(&mut ctx, s, v),
            }
        }
        self.flush(agent);
    }

    /// Schedule what the agent asked for during its callback.
    fn flush(&mut self, agent: AgentId) {
        let a = agent as usize;
        let now = self.now;
        let st = &mut self.states[a];
        for (at, token) in st.timers.drain(..) {
            self.queue.push(at.max(now), Pending::Timer { agent, token });
        }
        if st.outbox.is_empty() {
            return;
        }
        // The agent thinks once, then its messages leave back to back, in order.
        let depart = now.saturating_add(st.compute.sample(&mut st.compute_rng));
        for req in st.outbox.drain(..) {
            let arrive = st.order_out.deliver(depart);
            let session = &mut self.gateway[a];
            let done = arrive.max(session.free_at).saturating_add(self.timing.gateway_ns);
            session.free_at = done;
            self.queue.push(done, Pending::GatewayDone { agent, req });
            self.stats.requests += 1;
        }
    }

    fn at_gateway(&mut self, agent: AgentId, req: Request) {
        match self.gateway[agent as usize].handle(self.now, &req, &mut self.next_ids) {
            Outcome::Reply(report) => {
                if matches!(report.kind, ReportKind::GatewayRejected { .. }) {
                    self.stats.gateway_rejects += 1;
                }
                if self.record {
                    self.gateway_log.push(GatewayRecord { ts: self.now, agent, request: req, report: report.kind });
                }
                self.send_reports(agent, vec![report]);
            }
            Outcome::Forward(cmd, origin) => {
                let job = Job { cmd, origin: Some(origin) };
                self.queue.push(self.now + self.timing.hop_ns, Pending::AtEngine { instrument: req.instrument(), job });
            }
        }
    }

    fn send_reports(&mut self, agent: AgentId, reports: Vec<ExecReport>) {
        let at = self.states[agent as usize].order_in.deliver(self.now);
        self.queue.push(at, Pending::Reports { agent, reports });
    }

    fn enqueue(&mut self, instrument: InstrumentId, job: Job) {
        let v = &mut self.venues[instrument as usize];
        v.queue.push_back(job);
        self.stats.max_engine_queue = self.stats.max_engine_queue.max(v.queue.len());
        if v.in_service.is_none() {
            self.start_job(instrument);
        }
    }

    /// The command takes effect now; its results leave the engine after the service time.
    fn start_job(&mut self, instrument: InstrumentId) {
        let now = self.now;
        let v = &mut self.venues[instrument as usize];
        let Some(job) = v.queue.pop_front() else { return };
        v.out.clear();
        if let Err(e) = v.engine.process(now, &job.cmd, &mut v.out) {
            self.warnings.push(format!("t={now}: instrument {instrument}: {e}"));
        }
        self.stats.engine_commands += 1;
        self.stats.engine_events += v.out.len() as u64;
        for e in &v.out {
            self.digest.mix(&(instrument, e));
        }
        if self.record {
            v.log.extend_from_slice(&v.out);
        }
        let service = self.timing.engine_base_ns + self.timing.engine_per_event_ns * v.out.len() as i64;
        v.in_service = Some(job);
        self.queue.push(now + service, Pending::EngineDone { instrument });
    }

    /// Service complete: publish the public events and route the private ones.
    fn finish_job(&mut self, instrument: InstrumentId) {
        let now = self.now;
        let i = instrument as usize;
        let job = self.venues[i].in_service.take().expect("engine was busy");
        let events = std::mem::take(&mut self.venues[i].out);
        let fees = self.venues[i].fees;

        let public: Vec<Event> = events.iter().filter(|e| e.kind.is_public()).copied().collect();
        if !public.is_empty() {
            self.stats.trades += public.iter().filter(|e| matches!(e.kind, EventKind::Trade { .. })).count() as u64;
            let packet: Rc<[Event]> = Rc::from(public);
            for &feed in &self.venues[i].feeds {
                let at = self.feeds[feed as usize].link.deliver(now);
                self.queue.push(at, Pending::Feed { feed, packet: Rc::clone(&packet) });
            }
        }

        let mut batches: Vec<(AgentId, Vec<ExecReport>)> = Vec::new();
        for e in &events {
            let Some(owner) = e.kind.owner() else { continue };
            let report = self
                .gateway
                .get_mut(owner as usize)
                .and_then(|s| s.on_engine_event(instrument, e, job.origin.as_ref(), &fees));
            match report {
                Some(r) => match batches.iter_mut().find(|(a, _)| *a == owner) {
                    Some((_, batch)) => batch.push(r),
                    None => batches.push((owner, vec![r])),
                },
                None => self.warnings.push(format!("t={now}: unroutable engine event {e:?}")),
            }
        }
        self.venues[i].out = events;
        for (agent, reports) in batches {
            self.send_reports(agent, reports);
        }
        if !self.venues[i].queue.is_empty() {
            self.start_job(instrument);
        }
    }

    fn signal_tick(&mut self, signal: SignalId) {
        let now = self.now;
        let s = signal as usize;
        let st = &mut self.signals[s];
        let next = match &st.process {
            SignalProcess::RandomWalk { step, mean_interval_ns, .. } => {
                st.value += if st.rng.chance(0.5) { *step } else { -*step };
                Some(now + (detmath::exponential(*mean_interval_ns, &mut st.rng).round() as i64).max(1))
            }
            SignalProcess::JumpDiffusion { step, mean_interval_ns, jump_probability, jump, .. } => {
                let size = if st.rng.chance(*jump_probability) { *jump } else { *step };
                st.value += if st.rng.chance(0.5) { size } else { -size };
                Some(now + (detmath::exponential(*mean_interval_ns, &mut st.rng).round() as i64).max(1))
            }
            SignalProcess::Tracking { step, reversion, mean_interval_ns, .. } => {
                let pulled = ((1.0 - reversion) * st.deviation as f64).round() as i64;
                st.deviation = pulled + if st.rng.chance(0.5) { *step } else { -*step };
                Some(now + (detmath::exponential(*mean_interval_ns, &mut st.rng).round() as i64).max(1))
            }
            SignalProcess::Path { points, .. } => {
                st.value = points[st.next_point].1;
                st.next_point += 1;
                points.get(st.next_point).map(|p| p.0)
            }
        };
        if let SignalProcess::Tracking { leader, offset, .. } = self.signals[s].process {
            self.signals[s].value = self.signals[leader as usize].value + offset + self.signals[s].deviation;
        }
        self.publish(signal);
        // Tracking signals move with their leader (followers always have larger ids, so a
        // queue in id order handles chains deterministically).
        let mut pending: std::collections::VecDeque<SignalId> = self.signals[s].followers.iter().copied().collect();
        while let Some(f) = pending.pop_front() {
            if let SignalProcess::Tracking { leader, offset, .. } = self.signals[f as usize].process {
                self.signals[f as usize].value =
                    self.signals[leader as usize].value + offset + self.signals[f as usize].deviation;
            }
            self.publish(f);
            pending.extend(self.signals[f as usize].followers.iter().copied());
        }
        if let Some(t) = next {
            self.queue.push(t.max(now), Pending::SignalTick { signal });
        }
    }

    /// Send a signal's current value to every subscriber over its own link.
    fn publish(&mut self, signal: SignalId) {
        let now = self.now;
        self.stats.signal_updates += 1;
        let st = &mut self.signals[signal as usize];
        let value = st.value;
        for (agent, link) in st.subscribers.iter_mut() {
            let at = link.deliver(now);
            self.queue.push(at, Pending::SignalArrive { agent: *agent, signal, value });
        }
    }

    // ---- inspection ---------------------------------------------------------------------

    /// Current simulation time.
    pub fn now(&self) -> Timestamp {
        self.now
    }

    /// The true book of an instrument (what no agent sees directly).
    pub fn engine(&self, instrument: InstrumentId) -> &MatchingEngine {
        &self.venues[instrument as usize].engine
    }

    /// Ground-truth ledger of an agent on an instrument.
    pub fn ledger(&self, agent: AgentId, instrument: InstrumentId) -> &Ledger {
        &self.gateway[agent as usize].ledgers[instrument as usize]
    }

    /// Open orders the gateway attributes to an agent on an instrument (incl. in flight).
    pub fn gateway_open_orders(&self, agent: AgentId, instrument: InstrumentId) -> u32 {
        self.gateway[agent as usize].open_orders(instrument)
    }

    /// An agent's own view of its orders and positions.
    pub fn orders(&self, agent: AgentId) -> &MyOrders {
        &self.states[agent as usize].orders
    }

    /// An agent's view of an instrument's book (from its delayed private or group feed).
    pub fn book_view(&self, agent: AgentId, instrument: InstrumentId) -> Option<&MboBook> {
        let feed = (*self.states[agent as usize].views.get(instrument as usize)?)?;
        Some(&self.feeds[feed as usize].book)
    }

    /// The agent object, if it has type `T`.
    pub fn agent<T: Agent>(&self, agent: AgentId) -> Option<&T> {
        let any: &dyn Any = &**self.agents.get(agent as usize)?;
        any.downcast_ref::<T>()
    }

    /// The agent object, mutably, if it has type `T`.
    pub fn agent_mut<T: Agent>(&mut self, agent: AgentId) -> Option<&mut T> {
        let any: &mut dyn Any = &mut **self.agents.get_mut(agent as usize)?;
        any.downcast_mut::<T>()
    }

    /// Counters.
    pub fn stats(&self) -> &SimStats {
        &self.stats
    }

    /// Fingerprint of everything observable so far (engine events and all agent callbacks
    /// with their times). Equal digests mean identical runs (up to hash collisions).
    pub fn digest(&self) -> u64 {
        self.digest.0
    }

    /// Every engine event of an instrument (only if recording was enabled).
    pub fn events(&self, instrument: InstrumentId) -> &[Event] {
        &self.venues[instrument as usize].log
    }

    /// Requests the gateway answered itself, in time order (only if recording was enabled).
    pub fn gateway_log(&self) -> &[GatewayRecord] {
        &self.gateway_log
    }

    /// Current true value of a signal.
    pub fn signal_value(&self, signal: SignalId) -> i64 {
        self.signals[signal as usize].value
    }

    /// Resolved session schedule: `(time, instrument, phase)`.
    pub fn session_schedule(&self) -> &[(Timestamp, InstrumentId, Phase)] {
        &self.schedule
    }

    /// Problems recorded instead of panicking (e.g. an invalid scheduled phase change).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Events still scheduled (0 = nothing more will happen).
    pub fn pending_events(&self) -> usize {
        self.queue.len()
    }

    /// Number of agents.
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    /// Number of instruments.
    pub fn instrument_count(&self) -> usize {
        self.venues.len()
    }
}
