"""Statistics for paired Monte Carlo comparisons, standard library only.

* Student-t quantiles from the regularized incomplete beta function (continued fraction,
  modified Lentz), inverted by bisection;
* confidence intervals for a mean and for paired differences (common random numbers);
* the Wilson score interval for a proportion;
* a seeded percentile bootstrap.
"""

from __future__ import annotations

import math
import random
import statistics
from typing import Callable, Sequence


def _betacf(a: float, b: float, x: float) -> float:
    """Continued fraction for the incomplete beta function (modified Lentz)."""
    tiny, eps = 1e-300, 1e-16
    qab, qap, qam = a + b, a + 1.0, a - 1.0
    c, d = 1.0, 1.0 - qab * x / qap
    d = 1.0 / (d if abs(d) > tiny else tiny)
    h = d
    for m in range(1, 1000):
        m2 = 2 * m
        aa = m * (b - m) * x / ((qam + m2) * (a + m2))
        d = 1.0 + aa * d
        d = 1.0 / (d if abs(d) > tiny else tiny)
        c = 1.0 + aa / c
        c = c if abs(c) > tiny else tiny
        h *= d * c
        aa = -(a + m) * (qab + m) * x / ((a + m2) * (qap + m2))
        d = 1.0 + aa * d
        d = 1.0 / (d if abs(d) > tiny else tiny)
        c = 1.0 + aa / c
        c = c if abs(c) > tiny else tiny
        delta = d * c
        h *= delta
        if abs(delta - 1.0) < eps:
            break
    return h


def betainc(a: float, b: float, x: float) -> float:
    """Regularized incomplete beta function I_x(a, b)."""
    if x <= 0.0:
        return 0.0
    if x >= 1.0:
        return 1.0
    log_bt = math.lgamma(a + b) - math.lgamma(a) - math.lgamma(b) + a * math.log(x) + b * math.log1p(-x)
    bt = math.exp(log_bt)
    if x < (a + 1.0) / (a + b + 2.0):
        return bt * _betacf(a, b, x) / a
    return 1.0 - bt * _betacf(b, a, 1.0 - x) / b


def t_cdf(t: float, df: float) -> float:
    """Student-t cumulative distribution function."""
    x = df / (df + t * t)
    tail = 0.5 * betainc(df / 2.0, 0.5, x)
    return 1.0 - tail if t > 0 else tail


def t_ppf(q: float, df: float) -> float:
    """Student-t quantile, by bisection on the CDF."""
    if not 0.0 < q < 1.0:
        raise ValueError("q must be in (0, 1)")
    lo, hi = -1.0, 1.0
    while t_cdf(lo, df) > q:
        lo *= 2.0
    while t_cdf(hi, df) < q:
        hi *= 2.0
    for _ in range(200):
        mid = 0.5 * (lo + hi)
        if t_cdf(mid, df) < q:
            lo = mid
        else:
            hi = mid
    return 0.5 * (lo + hi)


def mean(xs: Sequence[float]) -> float:
    return sum(xs) / len(xs)


def stdev(xs: Sequence[float]) -> float:
    """Sample standard deviation (n − 1)."""
    if len(xs) < 2:
        return 0.0
    m = mean(xs)
    return math.sqrt(sum((x - m) ** 2 for x in xs) / (len(xs) - 1))


def mean_ci(xs: Sequence[float], level: float = 0.95) -> tuple[float, float, float]:
    """Mean and its two-sided Student-t confidence interval: (mean, low, high)."""
    m = mean(xs)
    if len(xs) < 2:
        return m, math.nan, math.nan
    half = t_ppf(0.5 + level / 2.0, len(xs) - 1) * stdev(xs) / math.sqrt(len(xs))
    return m, m - half, m + half


def paired_ci(a: Sequence[float], b: Sequence[float], level: float = 0.95) -> tuple[float, float, float]:
    """Mean of a − b over paired observations (same seeds) with its t interval."""
    if len(a) != len(b):
        raise ValueError("paired samples must have the same length")
    return mean_ci([x - y for x, y in zip(a, b)], level)


def wilson_ci(successes: int, n: int, level: float = 0.95) -> tuple[float, float, float]:
    """Proportion with its Wilson score interval: (p, low, high)."""
    if n == 0:
        return math.nan, math.nan, math.nan
    z = statistics.NormalDist().inv_cdf(0.5 + level / 2.0)
    p = successes / n
    denom = 1.0 + z * z / n
    centre = (p + z * z / (2 * n)) / denom
    half = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / denom
    # At the extremes the bounds are exactly 0 and 1; rounding would leave ~1e-17 there.
    lo = 0.0 if successes == 0 else centre - half
    hi = 1.0 if successes == n else centre + half
    return p, lo, hi


def bootstrap_ci(xs: Sequence[float], stat: Callable[[Sequence[float]], float] = mean, n: int = 2000, level: float = 0.95, seed: int = 0) -> tuple[float, float, float]:
    """Statistic with a seeded percentile-bootstrap interval."""
    rng = random.Random(seed)
    draws = sorted(stat([xs[rng.randrange(len(xs))] for _ in xs]) for _ in range(n))
    k_lo = int(math.floor((0.5 - level / 2.0) * n))
    k_hi = min(n - 1, int(math.ceil((0.5 + level / 2.0) * n)) - 1)
    return stat(xs), draws[k_lo], draws[k_hi]
