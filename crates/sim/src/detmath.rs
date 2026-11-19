//! Deterministic elementary functions.
//!
//! `f64::ln` and `f64::exp` call the platform's math library, and different libraries (glibc,
//! macOS, mingw, MSVC) may differ in the last bit. One differing bit in a sampled latency can
//! reorder two messages and change an entire simulation. Everything random in the kernel
//! therefore goes through these functions. They use only IEEE-754 basic operations (+ − × ÷,
//! `sqrt`, `round`, bit manipulation), each exactly specified, so results are bit-identical on
//! every conforming platform. Rust never contracts `a * b + c` into a fused multiply-add on its
//! own, so the expression order below is the computation order. Accuracy: a few ulps.

use core::f64::consts::{LOG2_E, SQRT_2};

use lob::rng::Rng;

/// High part of ln 2 (fdlibm `ln2_hi`): its low mantissa bits are zero, so `k * LN2_HI` is exact.
const LN2_HI: f64 = f64::from_bits(0x3fe6_2e42_fee0_0000);
/// ln 2 − `LN2_HI` (fdlibm `ln2_lo`).
const LN2_LO: f64 = f64::from_bits(0x3dea_39ef_3579_3c76);

/// Natural logarithm. `ln(0) = −∞`; negative or NaN input gives NaN.
pub fn ln(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x.is_infinite() {
        return f64::INFINITY;
    }
    let mut bits = x.to_bits();
    let mut k: i64 = 0;
    if bits < 0x0010_0000_0000_0000 {
        // Subnormal: scale by 2^54 to normalise.
        bits = (x * f64::from_bits(0x4350_0000_0000_0000)).to_bits();
        k -= 54;
    }
    k += ((bits >> 52) & 0x7ff) as i64 - 1023;
    // Mantissa in [1, 2), then fold into [1/sqrt2, sqrt2) so |s| below stays under 0.172.
    let mut m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000);
    if m > SQRT_2 {
        m *= 0.5;
        k += 1;
    }
    // ln m = 2 atanh(s) = 2 (s + s^3/3 + s^5/5 + ...), s = (m − 1)/(m + 1). Eleven terms
    // leave a truncation error below 1e-16 relative.
    let s = (m - 1.0) / (m + 1.0);
    let s2 = s * s;
    let mut p = 1.0 / 21.0;
    let mut n = 19.0;
    while n >= 1.0 {
        p = p * s2 + 1.0 / n;
        n -= 2.0;
    }
    let kf = k as f64;
    kf * LN2_HI + (kf * LN2_LO + 2.0 * s * p)
}

/// Exponential. Overflows to +∞ above about 709.78 and underflows to 0 below about −745.13.
pub fn exp(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x > 709.782_712_893_384 {
        return f64::INFINITY;
    }
    if x < -745.133_219_101_941_1 {
        return 0.0;
    }
    // x = k ln2 + r with |r| <= ln2/2; e^x = 2^k e^r.
    let k = (x * LOG2_E).round();
    let r = (x - k * LN2_HI) - k * LN2_LO;
    // Taylor series of e^r to degree 13 in Horner form; truncation error < 1e-17.
    let mut p = 1.0;
    let mut n = 13.0;
    while n >= 1.0 {
        p = 1.0 + r * p / n;
        n -= 1.0;
    }
    scale_by_pow2(p, k as i32)
}

/// `x * 2^k`, exact whenever the result is a normal number.
fn scale_by_pow2(x: f64, k: i32) -> f64 {
    if k > 1023 {
        return scale_by_pow2(x * f64::from_bits(0x7fe0_0000_0000_0000), k - 1023);
    }
    if k < -1022 {
        return scale_by_pow2(x * f64::from_bits(0x0010_0000_0000_0000), k + 1022);
    }
    x * f64::from_bits(((k + 1023) as u64) << 52)
}

/// Standard normal deviate (Marsaglia's polar method; uses only `ln` above and `sqrt`).
pub fn standard_normal(rng: &mut Rng) -> f64 {
    loop {
        let u = 2.0 * rng.f64() - 1.0;
        let v = 2.0 * rng.f64() - 1.0;
        let s = u * u + v * v;
        if s > 0.0 && s < 1.0 {
            return u * (-2.0 * ln(s) / s).sqrt();
        }
    }
}

/// Exponential deviate with the given mean (inverse transform).
pub fn exponential(mean: f64, rng: &mut Rng) -> f64 {
    // 1 − U lies in (0, 1], so the logarithm is finite.
    -mean * ln(1.0 - rng.f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel_err(a: f64, b: f64) -> f64 {
        if b == 0.0 { a.abs() } else { ((a - b) / b).abs() }
    }

    #[test]
    fn ln_matches_the_platform_to_a_few_ulps() {
        let mut worst: f64 = 0.0;
        let mut x: f64 = 1e-300;
        while x < 1e300 {
            for f in [1.0_f64, 1.000_000_1, 1.37, 2.0, 3.14159, 7.5] {
                let y = x * f;
                if (y - 1.0).abs() > 1e-3 {
                    worst = worst.max(rel_err(ln(y), y.ln()));
                }
            }
            x *= 13.7;
        }
        assert!(worst < 2e-15, "worst relative error {worst:e}");
        // Near 1, compare absolute error (the logarithm itself is tiny there).
        for d in [1e-12, 1e-9, 1e-6, 1e-3, -1e-6, -1e-3] {
            let y: f64 = 1.0 + d;
            assert!((ln(y) - y.ln()).abs() < 1e-18 + 1e-15 * y.ln().abs(), "ln({y})");
        }
        assert_eq!(ln(1.0), 0.0);
        assert!(ln(0.0).is_infinite() && ln(-1.0).is_nan());
        assert!(rel_err(ln(f64::from_bits(1)), (f64::from_bits(1)).ln()) < 2e-15, "subnormal");
    }

    #[test]
    fn exp_matches_the_platform_to_a_few_ulps() {
        let mut worst: f64 = 0.0;
        let mut x = -700.0;
        while x < 700.0 {
            worst = worst.max(rel_err(exp(x), x.exp()));
            x += 0.371_3;
        }
        assert!(worst < 4e-15, "worst relative error {worst:e}");
        assert_eq!(exp(0.0), 1.0);
        assert_eq!(exp(1000.0), f64::INFINITY);
        assert_eq!(exp(-1000.0), 0.0);
        assert!(rel_err(exp(ln(12_345.678)), 12_345.678) < 1e-14);
    }

    #[test]
    fn samplers_have_the_right_moments() {
        let mut rng = Rng::new(3);
        let n = 200_000;
        let (mut s1, mut s2) = (0.0, 0.0);
        for _ in 0..n {
            let z = standard_normal(&mut rng);
            s1 += z;
            s2 += z * z;
        }
        let mean = s1 / n as f64;
        let var = s2 / n as f64 - mean * mean;
        assert!(mean.abs() < 0.01 && (var - 1.0).abs() < 0.02, "mean {mean} var {var}");
        let m: f64 = (0..n).map(|_| exponential(5.0, &mut rng)).sum::<f64>() / n as f64;
        assert!((m - 5.0).abs() < 0.05, "exponential mean {m}");
    }

    #[test]
    fn samples_are_reproducible_bit_for_bit() {
        let a: Vec<u64> = {
            let mut r = Rng::new(11);
            (0..1000).map(|_| standard_normal(&mut r).to_bits()).collect()
        };
        let b: Vec<u64> = {
            let mut r = Rng::new(11);
            (0..1000).map(|_| standard_normal(&mut r).to_bits()).collect()
        };
        assert_eq!(a, b);
    }
}
