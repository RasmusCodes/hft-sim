//! Small dense linear algebra for the market-making models: eigen-decomposition of a symmetric
//! matrix by cyclic Jacobi rotations. Only basic IEEE operations and `sqrt`, so results are
//! bit-identical across platforms.

/// Eigenvalues (ascending) and eigenvectors of a symmetric matrix. `vectors[i][j]` is component `i`
/// of the eigenvector for `values[j]`; eigenvectors are orthonormal.
#[derive(Clone, Debug, PartialEq)]
pub struct SymmetricEigen {
    /// Eigenvalues, ascending.
    pub values: Vec<f64>,
    /// Eigenvectors as columns.
    pub vectors: Vec<Vec<f64>>,
}

/// Eigen-decomposition of the symmetric matrix `a` (only its upper triangle is read) by cyclic
/// Jacobi rotations, iterated until the off-diagonal mass is below 1e-15 of the total (or 100
/// sweeps).
///
/// # Panics
/// If `a` is not square.
pub fn symmetric_eigen(a: &[Vec<f64>]) -> SymmetricEigen {
    let n = a.len();
    assert!(a.iter().all(|r| r.len() == n), "matrix must be square");
    let mut m: Vec<Vec<f64>> = (0..n).map(|i| (0..n).map(|j| if j >= i { a[i][j] } else { a[j][i] }).collect()).collect();
    let mut v: Vec<Vec<f64>> = (0..n).map(|i| (0..n).map(|j| if i == j { 1.0 } else { 0.0 }).collect()).collect();
    let total: f64 = m.iter().flatten().map(|x| x * x).sum::<f64>().max(f64::MIN_POSITIVE);
    for _sweep in 0..100 {
        let off: f64 = (0..n).flat_map(|i| (0..n).filter(move |&j| j != i).map(move |j| (i, j))).map(|(i, j)| m[i][j] * m[i][j]).sum();
        if off <= 1e-30 * total {
            break;
        }
        for p in 0..n {
            for q in p + 1..n {
                let apq = m[p][q];
                if apq == 0.0 {
                    continue;
                }
                let theta = (m[q][q] - m[p][p]) / (2.0 * apq);
                let t = if theta.abs() > 1e150 {
                    0.5 / theta
                } else {
                    let s = if theta >= 0.0 { 1.0 } else { -1.0 };
                    s / (theta.abs() + (theta * theta + 1.0).sqrt())
                };
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                for r in 0..n {
                    let (mrp, mrq) = (m[r][p], m[r][q]);
                    m[r][p] = c * mrp - s * mrq;
                    m[r][q] = s * mrp + c * mrq;
                }
                for r in 0..n {
                    let (mpr, mqr) = (m[p][r], m[q][r]);
                    m[p][r] = c * mpr - s * mqr;
                    m[q][r] = s * mpr + c * mqr;
                }
                m[p][q] = 0.0;
                m[q][p] = 0.0;
                for row in v.iter_mut() {
                    let (vrp, vrq) = (row[p], row[q]);
                    row[p] = c * vrp - s * vrq;
                    row[q] = s * vrp + c * vrq;
                }
            }
        }
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&i, &j| m[i][i].total_cmp(&m[j][j]).then(i.cmp(&j)));
    SymmetricEigen {
        values: order.iter().map(|&j| m[j][j]).collect(),
        vectors: (0..n).map(|i| order.iter().map(|&j| v[i][j]).collect()).collect(),
    }
}

/// Solve A·X = B for X by Gaussian elimination with partial pivoting (B has any number of
/// columns). Returns `None` if A is singular to working precision.
///
/// # Panics
/// If the shapes do not match.
pub fn solve(a: &[Vec<f64>], b: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = a.len();
    assert!(a.iter().all(|r| r.len() == n) && b.len() == n, "shape mismatch");
    let cols = b.first().map_or(0, |r| r.len());
    let mut m: Vec<Vec<f64>> = a.iter().zip(b).map(|(ra, rb)| ra.iter().chain(rb).copied().collect()).collect();
    let scale = a.iter().flatten().fold(0.0f64, |s, x| s.max(x.abs())).max(f64::MIN_POSITIVE);
    for col in 0..n {
        let pivot = (col..n).max_by(|&i, &j| m[i][col].abs().total_cmp(&m[j][col].abs()).then(j.cmp(&i)))?;
        if m[pivot][col].abs() <= 1e-13 * scale {
            return None;
        }
        m.swap(col, pivot);
        for r in 0..n {
            if r != col {
                let f = m[r][col] / m[col][col];
                if f != 0.0 {
                    for c in col..n + cols {
                        m[r][c] -= f * m[col][c];
                    }
                }
            }
        }
    }
    Some((0..n).map(|r| (0..cols).map(|c| m[r][n + c] / m[r][r]).collect()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lob::rng::Rng;

    #[test]
    fn solve_recovers_a_known_solution_and_detects_singularity() {
        let a = vec![vec![2.0, 1.0, 0.0], vec![1.0, 3.0, 1.0], vec![0.0, 1.0, 4.0]];
        let x = vec![vec![1.0, -2.0], vec![0.5, 0.0], vec![-1.0, 3.0]];
        let b: Vec<Vec<f64>> = (0..3).map(|i| (0..2).map(|c| (0..3).map(|k| a[i][k] * x[k][c]).sum()).collect()).collect();
        let got = solve(&a, &b).expect("non-singular");
        for i in 0..3 {
            for c in 0..2 {
                assert!((got[i][c] - x[i][c]).abs() < 1e-12);
            }
        }
        assert!(solve(&[vec![1.0, 2.0], vec![2.0, 4.0]], &[vec![1.0], vec![2.0]]).is_none());
    }

    #[test]
    fn decomposition_reconstructs_a_random_symmetric_matrix() {
        let n = 9;
        let mut rng = Rng::new(3);
        let mut a = vec![vec![0.0; n]; n];
        for i in 0..n {
            for j in i..n {
                let x = rng.f64() * 2.0 - 1.0;
                a[i][j] = x;
                a[j][i] = x;
            }
        }
        let e = symmetric_eigen(&a);
        assert!(e.values.windows(2).all(|w| w[0] <= w[1]));
        for j in 0..n {
            for i in 0..n {
                // (A v_j)_i = λ_j v_ij
                let av: f64 = (0..n).map(|k| a[i][k] * e.vectors[k][j]).sum();
                assert!((av - e.values[j] * e.vectors[i][j]).abs() < 1e-12, "residual at ({i}, {j})");
            }
            for k in 0..n {
                let dot: f64 = (0..n).map(|i| e.vectors[i][j] * e.vectors[i][k]).sum();
                assert!((dot - if j == k { 1.0 } else { 0.0 }).abs() < 1e-12, "orthonormality ({j}, {k})");
            }
        }
    }

    #[test]
    fn diagonal_matrix_is_already_decomposed() {
        let a = vec![vec![3.0, 0.0, 0.0], vec![0.0, -1.0, 0.0], vec![0.0, 0.0, 2.0]];
        let e = symmetric_eigen(&a);
        assert_eq!(e.values, vec![-1.0, 2.0, 3.0]);
        assert_eq!(e.vectors[1][0].abs(), 1.0);
    }
}
