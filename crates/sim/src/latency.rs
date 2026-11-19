//! Latency models and FIFO links.
//!
//! A [`Link`] is one directed network path (an agent's order-entry session, its private
//! report channel, its market-data feed, a signal channel). Each link draws delays from its own
//! random stream, so changing one participant's latency never perturbs anyone else's draws.
//! Links never reorder: like a TCP session or a sequenced multicast channel, a message cannot
//! overtake the one sent before it on the same link.

use lob::Timestamp;
use lob::rng::Rng;

use crate::detmath;

/// Distribution of one-way delay in nanoseconds. Every sample is an integer ≥ 0.
#[derive(Clone, Debug, PartialEq)]
pub enum LatencyModel {
    /// Always `ns`.
    Constant {
        /// Delay.
        ns: i64,
    },
    /// Uniform integer in `min_ns..=max_ns`.
    Uniform {
        /// Lower bound.
        min_ns: i64,
        /// Upper bound.
        max_ns: i64,
    },
    /// `base_ns` plus a log-normal jitter with the given median and log-standard deviation:
    /// the usual shape of network and processing delay (a hard floor, then a right-skewed body).
    LogNormal {
        /// Fixed propagation/processing floor.
        base_ns: i64,
        /// Median of the jitter part.
        median_ns: f64,
        /// Standard deviation of log(jitter).
        sigma: f64,
    },
    /// `base_ns` plus a Pareto (Lomax) jitter, `scale · (U^(−1/alpha) − 1)`: a power-law tail
    /// for congestion and retransmission. Draws are capped at 1,000 s.
    Pareto {
        /// Fixed floor.
        base_ns: i64,
        /// Scale of the jitter.
        scale_ns: f64,
        /// Tail index (smaller = heavier tail; mean is infinite for alpha ≤ 1).
        alpha: f64,
    },
    /// A normal model plus, with probability `probability`, an extra spike delay
    /// (garbage-collection pauses, microbursts, retransmits).
    Spiky {
        /// Delay of every message.
        normal: Box<LatencyModel>,
        /// Probability of a spike per message.
        probability: f64,
        /// Extra delay when a spike happens.
        spike: Box<LatencyModel>,
    },
}

const MAX_DRAW_NS: f64 = 1e12;

fn to_ns(x: f64) -> i64 {
    x.clamp(0.0, MAX_DRAW_NS).round() as i64
}

impl LatencyModel {
    /// Constant delay.
    pub fn constant(ns: i64) -> Self {
        LatencyModel::Constant { ns }
    }

    /// No delay.
    pub fn zero() -> Self {
        LatencyModel::Constant { ns: 0 }
    }

    /// Floor plus log-normal jitter.
    pub fn log_normal(base_ns: i64, median_ns: f64, sigma: f64) -> Self {
        LatencyModel::LogNormal { base_ns, median_ns, sigma }
    }

    /// Draw one delay.
    pub fn sample(&self, rng: &mut Rng) -> i64 {
        match self {
            LatencyModel::Constant { ns } => *ns,
            LatencyModel::Uniform { min_ns, max_ns } => rng.range_i64(*min_ns, *max_ns),
            LatencyModel::LogNormal { base_ns, median_ns, sigma } => {
                let z = detmath::standard_normal(rng);
                base_ns.saturating_add(to_ns(median_ns * detmath::exp(sigma * z)))
            }
            LatencyModel::Pareto { base_ns, scale_ns, alpha } => {
                let u = 1.0 - rng.f64(); // (0, 1]
                let tail = detmath::exp(-detmath::ln(u) / alpha) - 1.0;
                base_ns.saturating_add(to_ns(scale_ns * tail))
            }
            LatencyModel::Spiky { normal, probability, spike } => {
                let d = normal.sample(rng);
                if rng.chance(*probability) { d.saturating_add(spike.sample(rng)) } else { d }
            }
        }
    }

    /// Check the parameters.
    pub fn validate(&self) -> Result<(), String> {
        let finite_nonneg = |x: f64, what: &str| {
            if x.is_finite() && x >= 0.0 { Ok(()) } else { Err(format!("{what} must be finite and >= 0, got {x}")) }
        };
        match self {
            LatencyModel::Constant { ns } if *ns < 0 => Err(format!("negative constant latency {ns}")),
            LatencyModel::Constant { .. } => Ok(()),
            LatencyModel::Uniform { min_ns, max_ns } if *min_ns < 0 || min_ns > max_ns => {
                Err(format!("uniform latency needs 0 <= min <= max, got {min_ns}..{max_ns}"))
            }
            LatencyModel::Uniform { .. } => Ok(()),
            LatencyModel::LogNormal { base_ns, median_ns, sigma } => {
                if *base_ns < 0 {
                    return Err(format!("negative base latency {base_ns}"));
                }
                finite_nonneg(*median_ns, "median_ns")?;
                finite_nonneg(*sigma, "sigma")
            }
            LatencyModel::Pareto { base_ns, scale_ns, alpha } => {
                if *base_ns < 0 {
                    return Err(format!("negative base latency {base_ns}"));
                }
                finite_nonneg(*scale_ns, "scale_ns")?;
                if !(alpha.is_finite() && *alpha > 0.0) {
                    return Err(format!("alpha must be finite and > 0, got {alpha}"));
                }
                Ok(())
            }
            LatencyModel::Spiky { normal, probability, spike } => {
                if !(0.0..=1.0).contains(probability) {
                    return Err(format!("spike probability must be in [0, 1], got {probability}"));
                }
                normal.validate()?;
                spike.validate()
            }
        }
    }
}

/// One directed, order-preserving network path.
#[derive(Clone, Debug)]
pub struct Link {
    model: LatencyModel,
    rng: Rng,
    last_arrival: Timestamp,
    messages: u64,
}

impl Link {
    /// Link with its own random stream.
    pub fn new(model: LatencyModel, rng: Rng) -> Self {
        Link { model, rng, last_arrival: Timestamp::MIN, messages: 0 }
    }

    /// Arrival time of a message sent at `sent`. Never earlier than the previous arrival on
    /// this link (no overtaking); equal arrival times keep send order in the scheduler.
    pub fn deliver(&mut self, sent: Timestamp) -> Timestamp {
        let arrival = sent.saturating_add(self.model.sample(&mut self.rng)).max(self.last_arrival);
        self.last_arrival = arrival;
        self.messages += 1;
        arrival
    }

    /// The link's latency model.
    pub fn model(&self) -> &LatencyModel {
        &self.model
    }

    /// Messages carried so far.
    pub fn messages(&self) -> u64 {
        self.messages
    }
}

/// Latency profile of one participant: one model per path.
#[derive(Clone, Debug, PartialEq)]
pub struct LatencyProfile {
    /// Time from receiving information to sending the resulting orders ("thinking").
    pub compute: LatencyModel,
    /// Agent → exchange gateway (order entry).
    pub order_out: LatencyModel,
    /// Exchange → agent (execution reports).
    pub order_in: LatencyModel,
    /// Exchange market-data publisher → agent.
    pub feed: LatencyModel,
    /// Signal source → agent (news, a leading market, the latent fundamental value).
    pub signal: LatencyModel,
}

impl LatencyProfile {
    /// Every path instantaneous (for tests).
    pub fn zero() -> Self {
        Self::constant(0, 0, 0, 0, 0)
    }

    /// Constant delays per path.
    pub fn constant(compute: i64, order_out: i64, order_in: i64, feed: i64, signal: i64) -> Self {
        LatencyProfile {
            compute: LatencyModel::constant(compute),
            order_out: LatencyModel::constant(order_out),
            order_in: LatencyModel::constant(order_in),
            feed: LatencyModel::constant(feed),
            signal: LatencyModel::constant(signal),
        }
    }

    /// Illustrative co-located participant: about 1 µs network paths and sub-microsecond
    /// reaction time. Orders of magnitude only, not a calibration.
    pub fn colocated() -> Self {
        LatencyProfile {
            compute: LatencyModel::log_normal(300, 200.0, 0.5),
            order_out: LatencyModel::log_normal(1_000, 150.0, 0.6),
            order_in: LatencyModel::log_normal(1_000, 150.0, 0.6),
            feed: LatencyModel::log_normal(1_500, 200.0, 0.6),
            signal: LatencyModel::log_normal(1_500, 200.0, 0.6),
        }
    }

    /// Illustrative participant in the same metro area but not co-located: tens of
    /// microseconds per path and a few microseconds of reaction time.
    pub fn proximity() -> Self {
        LatencyProfile {
            compute: LatencyModel::log_normal(2_000, 3_000.0, 0.8),
            order_out: LatencyModel::log_normal(50_000, 5_000.0, 0.8),
            order_in: LatencyModel::log_normal(50_000, 5_000.0, 0.8),
            feed: LatencyModel::log_normal(60_000, 5_000.0, 0.8),
            signal: LatencyModel::log_normal(60_000, 5_000.0, 0.8),
        }
    }

    /// Illustrative remote participant: about 4 ms one way (the order of the Chicago–New
    /// Jersey microwave route) with a heavier tail.
    pub fn remote() -> Self {
        LatencyProfile {
            compute: LatencyModel::log_normal(10_000, 20_000.0, 1.0),
            order_out: LatencyModel::log_normal(4_000_000, 50_000.0, 1.0),
            order_in: LatencyModel::log_normal(4_000_000, 50_000.0, 1.0),
            feed: LatencyModel::log_normal(4_000_000, 50_000.0, 1.0),
            signal: LatencyModel::log_normal(4_000_000, 50_000.0, 1.0),
        }
    }

    /// Check every model.
    pub fn validate(&self) -> Result<(), String> {
        for (name, m) in [
            ("compute", &self.compute),
            ("order_out", &self.order_out),
            ("order_in", &self.order_in),
            ("feed", &self.feed),
            ("signal", &self.signal),
        ] {
            m.validate().map_err(|e| format!("{name}: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(model: &LatencyModel, seed: u64, n: usize) -> Vec<i64> {
        let mut rng = Rng::new(seed);
        let mut v: Vec<i64> = (0..n).map(|_| model.sample(&mut rng)).collect();
        v.sort_unstable();
        v
    }

    fn quantile(sorted: &[i64], q: f64) -> f64 {
        sorted[((sorted.len() - 1) as f64 * q).round() as usize] as f64
    }

    #[test]
    fn constant_and_uniform() {
        assert!(samples(&LatencyModel::constant(1234), 1, 1000).iter().all(|&x| x == 1234));
        let u = samples(&LatencyModel::Uniform { min_ns: 100, max_ns: 200 }, 2, 100_000);
        assert_eq!((u[0], u[u.len() - 1]), (100, 200));
        let mean = u.iter().sum::<i64>() as f64 / u.len() as f64;
        assert!((mean - 150.0).abs() < 1.0, "mean {mean}");
    }

    #[test]
    fn log_normal_reproduces_its_median_and_sigma() {
        let m = LatencyModel::log_normal(1_000, 5_000.0, 0.7);
        let s = samples(&m, 3, 200_000);
        assert!(s[0] >= 1_000);
        let median = quantile(&s, 0.5) - 1_000.0;
        assert!((median / 5_000.0 - 1.0).abs() < 0.02, "median jitter {median}");
        // For a log-normal, q(0.8413) / q(0.5) = e^sigma.
        let ratio = (quantile(&s, 0.841_344_7) - 1_000.0) / median;
        assert!((ratio.ln() - 0.7).abs() < 0.02, "sigma estimate {}", ratio.ln());
    }

    #[test]
    fn pareto_has_a_power_law_tail() {
        let alpha = 1.5;
        let s = samples(&LatencyModel::Pareto { base_ns: 0, scale_ns: 1_000.0, alpha }, 4, 400_000);
        // P(X > x) = (1 + x/scale)^(−alpha): the tail fraction above 9 scales is 10^(−1.5).
        let above = s.iter().filter(|&&x| x > 9_000).count() as f64 / s.len() as f64;
        let expected = 10f64.powf(-alpha);
        assert!((above / expected - 1.0).abs() < 0.05, "tail {above} vs {expected}");
    }

    #[test]
    fn spikes_occur_at_the_configured_rate() {
        let m = LatencyModel::Spiky {
            normal: Box::new(LatencyModel::constant(10)),
            probability: 0.01,
            spike: Box::new(LatencyModel::constant(1_000_000)),
        };
        let s = samples(&m, 5, 200_000);
        let spikes = s.iter().filter(|&&x| x > 10).count() as f64 / s.len() as f64;
        assert!((spikes - 0.01).abs() < 0.001, "spike rate {spikes}");
    }

    #[test]
    fn links_never_reorder_even_with_heavy_jitter() {
        let mut link = Link::new(
            LatencyModel::Pareto { base_ns: 100, scale_ns: 10_000.0, alpha: 0.8 },
            Rng::new(6),
        );
        let mut last = Timestamp::MIN;
        for i in 0..100_000 {
            let a = link.deliver(i * 10);
            assert!(a >= last && a >= i * 10);
            last = a;
        }
    }

    #[test]
    fn invalid_models_are_rejected() {
        assert!(LatencyModel::constant(-1).validate().is_err());
        assert!(LatencyModel::Uniform { min_ns: 5, max_ns: 1 }.validate().is_err());
        assert!(LatencyModel::log_normal(0, f64::NAN, 1.0).validate().is_err());
        assert!(LatencyModel::Pareto { base_ns: 0, scale_ns: 1.0, alpha: 0.0 }.validate().is_err());
        assert!(LatencyProfile::colocated().validate().is_ok());
    }
}
