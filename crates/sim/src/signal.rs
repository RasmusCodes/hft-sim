//! Signal sources: information that reaches agents with their own delays.
//!
//! A signal stands for anything that moves fair value outside this order book: news, a
//! leading market (the futures contract for an equity), or the latent fundamental value that
//! informed and fundamental traders observe. Every subscriber receives each update over its own
//! link, so who learns first is decided by latency. That is what makes latency-arbitrage races
//! possible to measure rather than assume.
//!
//! All updates use integer arithmetic and the deterministic samplers in `detmath`, so a seed
//! reproduces the same path on every platform.

use lob::Timestamp;

use crate::messages::SignalId;

/// How a signal's value evolves.
#[derive(Clone, Debug, PartialEq)]
pub enum SignalProcess {
    /// Moves by ±`step` (equally likely) at exponentially distributed intervals.
    RandomWalk {
        /// Initial value.
        start: i64,
        /// Size of each move.
        step: i64,
        /// Mean time between moves, in nanoseconds.
        mean_interval_ns: f64,
    },
    /// A random walk with occasional jumps: each move is ±`jump` with probability
    /// `jump_probability`, otherwise ±`step` (news on top of diffusion).
    JumpDiffusion {
        /// Initial value.
        start: i64,
        /// Size of an ordinary move.
        step: i64,
        /// Mean time between moves, in nanoseconds.
        mean_interval_ns: f64,
        /// Probability that a move is a jump.
        jump_probability: f64,
        /// Size of a jump.
        jump: i64,
    },
    /// Follows another signal: value = leader + `offset` + x. The gap x mean-reverts as a
    /// discrete Ornstein–Uhlenbeck process: at exponentially distributed intervals,
    /// x ← round((1 − `reversion`)·x) ± `step`. Republished whenever the leader moves.
    /// The leader must have a smaller signal id (no cycles).
    Tracking {
        /// Signal followed.
        leader: SignalId,
        /// Constant offset from the leader.
        offset: i64,
        /// Size of each innovation of the gap.
        step: i64,
        /// Share of the gap removed at each gap update, in [0, 1].
        reversion: f64,
        /// Mean time between gap updates, in nanoseconds.
        mean_interval_ns: f64,
    },
    /// A fixed path of `(time, value)` points (scripted news, test scenarios).
    Path {
        /// Value before the first point.
        start: i64,
        /// Points in non-decreasing time order.
        points: Vec<(Timestamp, i64)>,
    },
}

impl SignalProcess {
    /// Value at time 0. For `Tracking` this is only the offset; the kernel adds the leader's value.
    pub fn start_value(&self) -> i64 {
        match self {
            SignalProcess::RandomWalk { start, .. }
            | SignalProcess::JumpDiffusion { start, .. }
            | SignalProcess::Path { start, .. } => *start,
            SignalProcess::Tracking { offset, .. } => *offset,
        }
    }

    /// Mean time between the process's own updates (`None` for a scripted path).
    pub(crate) fn mean_interval_ns(&self) -> Option<f64> {
        match self {
            SignalProcess::RandomWalk { mean_interval_ns, .. }
            | SignalProcess::JumpDiffusion { mean_interval_ns, .. }
            | SignalProcess::Tracking { mean_interval_ns, .. } => Some(*mean_interval_ns),
            SignalProcess::Path { .. } => None,
        }
    }

    pub(crate) fn validate(&self, own_id: SignalId) -> Result<(), String> {
        if let Some(m) = self.mean_interval_ns() {
            if !(m.is_finite() && m > 0.0) {
                return Err(format!("mean_interval_ns must be finite and > 0, got {m}"));
            }
        }
        match self {
            SignalProcess::RandomWalk { .. } => Ok(()),
            SignalProcess::JumpDiffusion { jump_probability, .. } => {
                if (0.0..=1.0).contains(jump_probability) {
                    Ok(())
                } else {
                    Err(format!("jump_probability must be in [0, 1], got {jump_probability}"))
                }
            }
            SignalProcess::Tracking { leader, reversion, .. } => {
                if *leader >= own_id {
                    Err(format!("leader {leader} must have a smaller id than the tracking signal {own_id}"))
                } else if !(0.0..=1.0).contains(reversion) {
                    Err(format!("reversion must be in [0, 1], got {reversion}"))
                } else {
                    Ok(())
                }
            }
            SignalProcess::Path { points, .. } => {
                if points.windows(2).any(|w| w[1].0 < w[0].0) || points.first().is_some_and(|p| p.0 < 0) {
                    Err("path points must have non-negative, non-decreasing times".to_string())
                } else {
                    Ok(())
                }
            }
        }
    }
}
