//! Deterministic pseudo-random numbers: xoshiro256** seeded through SplitMix64.
//!
//! Experiments must be reproducible from a seed on any machine and with any compiler
//! version. That rules out `std`'s randomly keyed hashers and external crates whose
//! algorithms may change between releases; this small generator is fixed forever.
//! Only integer arithmetic is used, so outputs do not depend on the platform's libm.

/// One SplitMix64 step (Steele, Lea & Flood 2014), used for seeding and stream derivation.
#[inline]
pub fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// xoshiro256** 1.0 (Blackman & Vigna).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    /// Generator seeded from a single `u64`.
    pub fn new(seed: u64) -> Self {
        let mut sm = seed;
        let s = [
            splitmix64(&mut sm),
            splitmix64(&mut sm),
            splitmix64(&mut sm),
            splitmix64(&mut sm),
        ];
        Rng { s }
    }

    /// Independent stream number `stream` of master seed `seed`, e.g. one per agent,
    /// so adding an agent does not perturb the random numbers of the others.
    pub fn stream(seed: u64, stream: u64) -> Self {
        let mut a = seed;
        let mut b = stream ^ 0x5851_f42d_4c95_7f2d;
        Rng::new(splitmix64(&mut a) ^ splitmix64(&mut b).rotate_left(17))
    }

    /// Generator with an explicit internal state (for known-answer tests).
    pub fn from_state(s: [u64; 4]) -> Self {
        assert!(s != [0; 4], "xoshiro256** state must not be all zero");
        Rng { s }
    }

    /// Next 64 random bits.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform integer in `0..n` (Lemire's unbiased multiply-shift method). `n > 0`.
    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "Rng::below(0)");
        let mut m = u128::from(self.next_u64()) * u128::from(n);
        let mut low = m as u64;
        if low < n {
            let threshold = n.wrapping_neg() % n;
            while low < threshold {
                m = u128::from(self.next_u64()) * u128::from(n);
                low = m as u64;
            }
        }
        (m >> 64) as u64
    }

    /// Uniform integer in `lo..=hi`.
    #[inline]
    pub fn range_i64(&mut self, lo: i64, hi: i64) -> i64 {
        assert!(lo <= hi, "Rng::range_i64: empty range");
        let span = (hi as i128 - lo as i128 + 1) as u128;
        assert!(span <= u128::from(u64::MAX), "Rng::range_i64: range too wide");
        (lo as i128 + i128::from(self.below(span as u64))) as i64
    }

    /// Uniform index in `0..len` (`len > 0`).
    #[inline]
    pub fn index(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }

    /// Uniform `f64` in `[0, 1)` with 53 random bits.
    #[inline]
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// `true` with probability `p`.
    #[inline]
    pub fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }

    /// Number of failures before the first success in Bernoulli(`p`) trials.
    /// Uses no transcendental functions, so it is bit-reproducible across platforms.
    pub fn geometric(&mut self, p: f64) -> u64 {
        assert!(p > 0.0 && p <= 1.0, "Rng::geometric: p must be in (0, 1]");
        let mut k = 0;
        while !self.chance(p) {
            k += 1;
        }
        k
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xoshiro256starstar_known_answers() {
        // Hand-derived from the reference algorithm with state {1, 2, 3, 4}.
        let mut r = Rng::from_state([1, 2, 3, 4]);
        assert_eq!(r.next_u64(), 11_520);
        assert_eq!(r.next_u64(), 0);
        assert_eq!(r.next_u64(), 1_509_978_240);
    }

    #[test]
    fn splitmix64_known_answer() {
        // First output of SplitMix64 from state 0.
        let mut s = 0u64;
        assert_eq!(splitmix64(&mut s), 0xe220_a839_7b1d_cdaf);
    }

    #[test]
    fn below_stays_in_range_and_covers_it() {
        let mut r = Rng::new(7);
        let mut seen = [false; 7];
        for _ in 0..10_000 {
            let x = r.below(7);
            assert!(x < 7);
            seen[x as usize] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    #[test]
    fn streams_are_reproducible_and_distinct() {
        let a1: Vec<u64> = (0..4).map({
            let mut r = Rng::stream(42, 1);
            move |_| r.next_u64()
        })
        .collect();
        let a2: Vec<u64> = (0..4).map({
            let mut r = Rng::stream(42, 1);
            move |_| r.next_u64()
        })
        .collect();
        let b: Vec<u64> = (0..4).map({
            let mut r = Rng::stream(42, 2);
            move |_| r.next_u64()
        })
        .collect();
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }
}
