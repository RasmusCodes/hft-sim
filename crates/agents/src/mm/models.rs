//! Optimal market-making quotes: the Avellaneda–Stoikov model and its exact solution with an
//! inventory bound by Guéant, Lehalle & Fernandez-Tapia (GLFT, 2013).
//!
//! **Model** (GLFT §2, transcribed from arXiv:1105.3115 v4 as rendered by ar5iv). The reference
//! price follows dS = σ dW. A quote at distance δ from it is filled at rate λ(δ) = A·e^(−kδ), on each
//! side, one unit at a time. Inventory stays within ±Q. The market maker maximises
//! E[−exp(−γ(X_T + q_T·S_T))].
//!
//! **Exact solution** (Propositions 1–2, Theorem 1). The value function is
//! u = −exp(−γ(x + qs))·v_q(t)^(−γ/k). Here v(t) = exp(−M(T − t))·1, and M is the
//! (2Q+1)×(2Q+1) tridiagonal matrix with α·q² on the diagonal and −η beside it, where
//! α = kγσ²/2 and η = A(1 + γ/k)^−(1 + k/γ). The optimal depths are
//! δᵇ = (1/k)·ln(v_q/v_{q+1}) + (1/γ)·ln(1 + γ/k) (no bid at q = Q) and
//! δᵃ = (1/k)·ln(v_q/v_{q−1}) + (1/γ)·ln(1 + γ/k) (no ask at q = −Q).
//!
//! **Asymptotics** (Theorem 2). As T − t → ∞ the quotes converge; the limit uses the eigenvector of M's
//! smallest eigenvalue in place of v. The closed-form approximation replaces that eigenvector by a
//! Gaussian: δᵇ ≈ (1/γ)ln(1 + γ/k) + ((2q + 1)/2)·√(σ²γ/(2kA)·(1 + γ/k)^(1 + k/γ)), and
//! δᵃ ≈ (1/γ)ln(1 + γ/k) − ((2q − 1)/2)·√(…).
//!
//! **Avellaneda–Stoikov (2008).** The reservation price is r = s − qγσ²τ, and the total spread is
//! γσ²τ + (2/γ)·ln(1 + γ/k), with τ = T − t. The paper itself was not read here. Instead, the tests
//! show these formulas are exactly the first-order expansion in τ of the GLFT solution above (for
//! inventories at least two units inside the bound). The GLFT solution is in turn checked against a
//! direct numerical solution of the Hamilton–Jacobi–Bellman equation.
//!
//! Units: prices in ticks, time in seconds, inventory in quote units (one fill = one unit), γ per
//! tick·unit, σ in ticks/√s, A per second, k per tick. Transcendental functions come from
//! `sim::detmath`, so quotes are bit-identical across platforms.

use sim::detmath::{exp, ln};

use super::linalg::{SymmetricEigen, symmetric_eigen};

/// Parameters of the Avellaneda–Stoikov / GLFT model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MmModel {
    /// Volatility of the reference price, ticks per √second.
    pub sigma: f64,
    /// Fill intensity of a quote at the reference price, fills per second.
    pub a: f64,
    /// Decay of fill intensity with distance, per tick.
    pub k: f64,
    /// Absolute risk aversion, per tick per unit.
    pub gamma: f64,
}

/// Distances of the optimal bid and ask from the reference price, in ticks (positive = away from
/// it). `None` where the model posts no quote.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Depths {
    /// Reference minus bid.
    pub bid: Option<f64>,
    /// Ask minus reference.
    pub ask: Option<f64>,
}

impl MmModel {
    /// α = kγσ²/2.
    pub fn alpha(&self) -> f64 {
        0.5 * self.k * self.gamma * self.sigma * self.sigma
    }

    /// η = A·(1 + γ/k)^−(1 + k/γ).
    pub fn eta(&self) -> f64 {
        self.a * exp(-(1.0 + self.k / self.gamma) * ln(1.0 + self.gamma / self.k))
    }

    /// (1/γ)·ln(1 + γ/k): each side's depth at zero inventory as τ → 0.
    pub fn base_depth(&self) -> f64 {
        ln(1.0 + self.gamma / self.k) / self.gamma
    }

    /// Avellaneda–Stoikov quotes at inventory `q` with `tau` seconds to the horizon:
    /// δᵇ = qγσ²τ + ψ/2 and δᵃ = −qγσ²τ + ψ/2, with ψ = γσ²τ + (2/γ)ln(1 + γ/k).
    pub fn avellaneda_stoikov(&self, q: f64, tau: f64) -> Depths {
        let skew = q * self.gamma * self.sigma * self.sigma * tau;
        let half = 0.5 * self.gamma * self.sigma * self.sigma * tau + self.base_depth();
        Depths { bid: Some(skew + half), ask: Some(-skew + half) }
    }

    /// The closed-form approximation of the asymptotic (T → ∞) GLFT quotes (GLFT §4).
    pub fn glft_closed_form(&self, q: f64) -> Depths {
        let c = (self.alpha() / self.eta()).sqrt() / (2.0 * self.k);
        Depths { bid: Some(self.base_depth() + c * (2.0 * q + 1.0)), ask: Some(self.base_depth() - c * (2.0 * q - 1.0)) }
    }
}

/// Exact GLFT solution for one parameter set and inventory bound, prepared once (an
/// eigen-decomposition of M) and then evaluated at any time to the horizon.
#[derive(Clone, Debug, PartialEq)]
pub struct GlftSolution {
    /// Parameters.
    pub model: MmModel,
    /// Inventory bound Q, in units.
    pub q_max: i64,
    eigen: SymmetricEigen,
    /// Vᵀ·1, the weights of the eigenvectors in v(T) = 1.
    weights: Vec<f64>,
}

impl GlftSolution {
    /// Decompose M for `model` with inventory bound `q_max` (≥ 1).
    ///
    /// # Panics
    /// If `q_max` < 1 or a parameter is not finite and positive.
    pub fn new(model: MmModel, q_max: i64) -> Self {
        assert!(q_max >= 1, "inventory bound must be at least 1");
        for (name, x) in [("sigma", model.sigma), ("a", model.a), ("k", model.k), ("gamma", model.gamma)] {
            assert!(x.is_finite() && x > 0.0, "{name} must be finite and positive, got {x}");
        }
        let n = (2 * q_max + 1) as usize;
        let (alpha, eta) = (model.alpha(), model.eta());
        let mut m = vec![vec![0.0; n]; n];
        for (i, row) in m.iter_mut().enumerate() {
            let q = i as i64 - q_max;
            row[i] = alpha * (q * q) as f64;
            if i > 0 {
                row[i - 1] = -eta;
            }
            if i + 1 < n {
                row[i + 1] = -eta;
            }
        }
        let eigen = symmetric_eigen(&m);
        let weights: Vec<f64> = (0..n).map(|j| (0..n).map(|i| eigen.vectors[i][j]).sum()).collect();
        GlftSolution { model, q_max, eigen, weights }
    }

    /// v_q(τ) times the common factor e^(λ_min·τ), which cancels in every ratio. Scaling by the
    /// smallest eigenvalue keeps it finite for any τ.
    fn v_scaled(&self, tau: f64, q: i64) -> f64 {
        let i = (q + self.q_max) as usize;
        let l0 = self.eigen.values[0];
        (0..self.weights.len()).map(|j| self.eigen.vectors[i][j] * self.weights[j] * exp(-(self.eigen.values[j] - l0) * tau)).sum()
    }

    /// Optimal depths at inventory `q` with `tau` seconds to the horizon (Theorem 1). `None` on the
    /// side that would breach the bound, and on both sides when |q| exceeds it.
    pub fn depths(&self, tau: f64, q: i64) -> Depths {
        if q.abs() > self.q_max {
            return Depths { bid: None, ask: None };
        }
        let tau = tau.max(0.0);
        let base = self.model.base_depth();
        let vq = self.v_scaled(tau, q);
        let side = |other: i64| -> Option<f64> {
            (other.abs() <= self.q_max).then(|| ln(vq / self.v_scaled(tau, other)) / self.model.k + base)
        };
        Depths { bid: side(q + 1), ask: side(q - 1) }
    }

    /// The T → ∞ limit of the optimal depths (Theorem 2), from the eigenvector of the smallest
    /// eigenvalue of M.
    pub fn asymptotic_depths(&self, q: i64) -> Depths {
        if q.abs() > self.q_max {
            return Depths { bid: None, ask: None };
        }
        let f = |q: i64| self.eigen.vectors[(q + self.q_max) as usize][0].abs();
        let base = self.model.base_depth();
        let side = |other: i64| (other.abs() <= self.q_max).then(|| ln(f(q) / f(other)) / self.model.k + base);
        Depths { bid: side(q + 1), ask: side(q - 1) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GLFT's Figure 1 parameters: σ = 0.3 tick/√s, A = 0.9 /s, k = 0.3 /tick, γ = 0.01 /tick.
    fn fig1() -> MmModel {
        MmModel { sigma: 0.3, a: 0.9, k: 0.3, gamma: 0.01 }
    }

    /// Direct numerical solution of the HJB, independent of GLFT's change of variables. With CARA
    /// utility and arithmetic Brownian motion the value function factors as
    /// u = −exp(−γ(x + qs))·θ_q(t). Substituting into the HJB gives, in τ = T − t,
    /// dθ_q/dτ = ½σ²γ²q²θ_q + min_δ A e^(−kδ)(e^(−γδ)θ_{q+1} − θ_q) + min_δ A e^(−kδ)(e^(−γδ)θ_{q−1} − θ_q),
    /// θ_q(0) = 1, with the bid term absent at q = Q and the ask term absent at q = −Q.
    /// Integrated by RK4; each minimum is found by golden-section search (no closed form used).
    fn hjb_by_numerical_search(m: MmModel, q_max: i64, horizon: f64, dt: f64) -> Vec<f64> {
        let n = (2 * q_max + 1) as usize;
        let side_min = |own: f64, next: f64| -> f64 {
            let f = |d: f64| m.a * exp(-m.k * d) * (exp(-m.gamma * d) * next - own);
            let (mut lo, mut hi) = (-50.0 / m.k, 200.0 / m.k);
            let g = (5f64.sqrt() - 1.0) / 2.0;
            let (mut x1, mut x2) = (hi - g * (hi - lo), lo + g * (hi - lo));
            let (mut f1, mut f2) = (f(x1), f(x2));
            for _ in 0..100 {
                if f1 <= f2 {
                    hi = x2;
                    (x2, f2) = (x1, f1);
                    x1 = hi - g * (hi - lo);
                    f1 = f(x1);
                } else {
                    lo = x1;
                    (x1, f1) = (x2, f2);
                    x2 = lo + g * (hi - lo);
                    f2 = f(x2);
                }
            }
            f1.min(f2)
        };
        let rhs = |th: &[f64]| -> Vec<f64> {
            (0..n)
                .map(|i| {
                    let q = i as i64 - q_max;
                    let mut d = 0.5 * m.sigma * m.sigma * m.gamma * m.gamma * (q * q) as f64 * th[i];
                    if q < q_max {
                        d += side_min(th[i], th[i + 1]);
                    }
                    if q > -q_max {
                        d += side_min(th[i], th[i - 1]);
                    }
                    d
                })
                .collect()
        };
        let mut th = vec![1.0; n];
        let steps = (horizon / dt).round() as usize;
        for _ in 0..steps {
            let k1 = rhs(&th);
            let y2: Vec<f64> = th.iter().zip(&k1).map(|(y, k)| y + 0.5 * dt * k).collect();
            let k2 = rhs(&y2);
            let y3: Vec<f64> = th.iter().zip(&k2).map(|(y, k)| y + 0.5 * dt * k).collect();
            let k3 = rhs(&y3);
            let y4: Vec<f64> = th.iter().zip(&k3).map(|(y, k)| y + dt * k).collect();
            let k4 = rhs(&y4);
            for i in 0..n {
                th[i] += dt / 6.0 * (k1[i] + 2.0 * k2[i] + 2.0 * k3[i] + k4[i]);
            }
        }
        th
    }

    #[test]
    fn exact_solution_matches_a_direct_numerical_solution_of_the_hjb() {
        let (m, q_max, tau) = (fig1(), 6, 30.0);
        let theta = hjb_by_numerical_search(m, q_max, tau, 0.01);
        let exact = GlftSolution::new(m, q_max);
        for q in -q_max..=q_max {
            let i = (q + q_max) as usize;
            // Optimal depth from the numerical θ: argmin of A e^(−kδ)(e^(−γδ)θ' − θ) is
            // δ* = (1/γ) ln((1 + γ/k) θ'/θ), a calculus fact independent of GLFT.
            let num = |other: usize| ln((1.0 + m.gamma / m.k) * theta[other] / theta[i]) / m.gamma;
            let d = exact.depths(tau, q);
            if q < q_max {
                let e = d.bid.expect("bid inside the bound");
                assert!((e - num(i + 1)).abs() < 1e-6, "bid depth at q = {q}: exact {e}, numerical {}", num(i + 1));
            } else {
                assert_eq!(d.bid, None, "no bid at the upper bound");
            }
            if q > -q_max {
                let e = d.ask.expect("ask inside the bound");
                assert!((e - num(i - 1)).abs() < 1e-6, "ask depth at q = {q}: exact {e}, numerical {}", num(i - 1));
            } else {
                assert_eq!(d.ask, None, "no ask at the lower bound");
            }
        }
    }

    #[test]
    fn avellaneda_stoikov_is_the_first_order_expansion_of_the_exact_solution() {
        let m = MmModel { sigma: 1.0, a: 5.0, k: 1.0, gamma: 0.05 };
        let exact = GlftSolution::new(m, 10);
        for q in -5..=5 {
            let err = |tau: f64| -> f64 {
                let (e, a) = (exact.depths(tau, q), m.avellaneda_stoikov(q as f64, tau));
                (e.bid.unwrap() - a.bid.unwrap()).abs().max((e.ask.unwrap() - a.ask.unwrap()).abs())
            };
            let (e1, e2) = (err(1e-2), err(1e-3));
            // The error vanishes faster than τ: a ten times shorter horizon gives at least a
            // ninety times smaller error (second-order terms of ln v_q are the same for interior
            // q and cancel, so it is in fact third order here).
            assert!(e2 < 1e-6 && e1 / e2 > 90.0, "q = {q}: errors {e1:e} at 0.01 s, {e2:e} at 0.001 s");
        }
        // At the horizon both give the base depth.
        let d = exact.depths(0.0, 3);
        assert!((d.bid.unwrap() - m.base_depth()).abs() < 1e-12 && (d.ask.unwrap() - m.base_depth()).abs() < 1e-12);
    }

    #[test]
    fn quotes_converge_to_the_asymptotic_solution_and_skew_against_inventory() {
        let exact = GlftSolution::new(fig1(), 6);
        for q in -6..=6i64 {
            let (far, asym) = (exact.depths(2_000.0, q), exact.asymptotic_depths(q));
            for (x, y) in [(far.bid, asym.bid), (far.ask, asym.ask)] {
                match (x, y) {
                    (Some(x), Some(y)) => assert!((x - y).abs() < 1e-9, "q = {q}: {x} vs {y}"),
                    (None, None) => {}
                    other => panic!("q = {q}: sides differ {other:?}"),
                }
            }
        }
        // Long inventory: bid further away, ask closer (and the reverse when short); flat is symmetric.
        let a = |q| exact.asymptotic_depths(q);
        assert!(a(2).bid.unwrap() > a(0).bid.unwrap() && a(2).ask.unwrap() < a(0).ask.unwrap());
        assert!((a(0).bid.unwrap() - a(0).ask.unwrap()).abs() < 1e-9);
        assert!((a(3).bid.unwrap() - a(-3).ask.unwrap()).abs() < 1e-9, "mirror symmetry");
    }

    #[test]
    fn closed_form_approximates_the_asymptotic_quotes_when_alpha_over_eta_is_small() {
        // The Gaussian replaces the discrete eigenvector; it is accurate when α/η is small (the
        // eigenvector varies slowly with q) and away from the bound. Compared on the part that
        // depends on inventory (depth minus the base depth), which is what the approximation adds.
        let m = MmModel { sigma: 0.3, a: 0.9, k: 0.3, gamma: 0.01 };
        let exact = GlftSolution::new(m, 30);
        let base = m.base_depth();
        for q in -3..=3i64 {
            let (e, c) = (exact.asymptotic_depths(q), m.glft_closed_form(q as f64));
            for (x, y) in [(e.bid.unwrap(), c.bid.unwrap()), (e.ask.unwrap(), c.ask.unwrap())] {
                assert!(((x - base) - (y - base)).abs() <= 0.02 * (y - base).abs(), "q = {q}: exact {e:?}, closed form {c:?}");
            }
        }
    }
}
