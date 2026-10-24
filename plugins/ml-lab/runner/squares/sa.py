"""Squeeze one packing with simulated annealing (ML_LAB_PLAN.md B3).

The container is shrunk by a factor, every overlap and every corner past a
wall is penalised (separating-axis penetration depth, squared), and annealing
moves one square at a time until the penalty is gone or the time is. A
success shrinks the next step further, a failure halves it. Only the metric
decides whether a result is real: `squeeze` returns floats, the caller checks
them with Kaggle's metric before storing anything.
"""

from __future__ import annotations

import math
import time

import numpy as np
from numba import njit

# A state counts as feasible when no overlap or wall excess is above this;
# `repair` then opens what is left, at a cost of about 4 * FEASIBLE * L.
FEASIBLE = 1e-9


@njit(cache=True)
def _half(t, ux, uy):
    c = math.cos(t)
    s = math.sin(t)
    return 0.5 * (abs(c * ux + s * uy) + abs(-s * ux + c * uy))


@njit(cache=True)
def _depth(xi, yi, ai, xj, yj, aj):
    dx = xj - xi
    dy = yj - yi
    if dx * dx + dy * dy >= 2.0:
        return 0.0
    best = 1e9
    for k in range(4):
        t = ai if k < 2 else aj
        c = math.cos(t)
        s = math.sin(t)
        if k % 2 == 0:
            ux, uy = c, s
        else:
            ux, uy = -s, c
        ov = _half(ai, ux, uy) + _half(aj, ux, uy) - abs(dx * ux + dy * uy)
        if ov < best:
            best = ov
            if best <= 0.0:
                return 0.0
    return best


@njit(cache=True)
def _wall(x, y, a, L):
    h = _half(a, 1.0, 0.0)  # the same extent on both axes for a square
    v = 0.0
    if x - h < 0.0:
        v += h - x
    if x + h > L:
        v += x + h - L
    if y - h < 0.0:
        v += h - y
    if y + h > L:
        v += y + h - L
    return v


@njit(cache=True)
def _local(i, x, y, a, L):
    e = _wall(x[i], y[i], a[i], L) ** 2
    for j in range(x.shape[0]):
        if j != i:
            d = _depth(x[i], y[i], a[i], x[j], y[j], a[j])
            e += d * d
    return e


@njit(cache=True)
def worst(x, y, a, L):
    """The largest overlap depth or wall excess: 0 means feasible."""
    m = 0.0
    n = x.shape[0]
    for i in range(n):
        w = _wall(x[i], y[i], a[i], L)
        if w > m:
            m = w
        for j in range(i + 1, n):
            d = _depth(x[i], y[i], a[i], x[j], y[j], a[j])
            if d > m:
                m = d
    return m


@njit(cache=True)
def anneal(x, y, a, L, iters, T0, T1, step, seed):
    np.random.seed(seed)
    n = x.shape[0]
    acc = 0
    for it in range(iters):
        T = T0 * (T1 / T0) ** (it / iters)
        i = np.random.randint(n)
        old = _local(i, x, y, a, L)
        if old == 0.0 and np.random.random() < 0.9:
            continue  # mostly move squares that are in trouble
        xi, yi, ai = x[i], y[i], a[i]
        r = np.random.random()
        if r < 0.03:
            # jump: a square in trouble tries a random place and angle, which
            # is how a jammed row hands one square to a hole elsewhere
            x[i] = 0.5 + np.random.random() * (L - 1.0)
            y[i] = 0.5 + np.random.random() * (L - 1.0)
            a[i] = 0.0 if np.random.random() < 0.5 else np.random.random() * 1.5707963267948966
        elif r < 0.75:
            x[i] += np.random.normal() * step
            y[i] += np.random.normal() * step
        else:
            a[i] += np.random.normal() * step * 2.0
        new = _local(i, x, y, a, L)
        d = new - old
        if d <= 0.0 or np.random.random() < math.exp(-d / T):
            acc += 1
        else:
            x[i], y[i], a[i] = xi, yi, ai
        if it % 2000 == 1999:
            r = acc / 2000.0
            if r > 0.4:
                step = min(step * 1.3, 0.3)
            elif r < 0.15:
                step = max(step * 0.7, 1e-7)
            acc = 0
    return step


def bounds(xs, ys, angs):
    h = np.array([0.5 * (abs(math.cos(t)) + abs(math.sin(t))) for t in angs])
    return (xs - h).min(), (ys - h).min(), (xs + h).max(), (ys + h).max()


def squeeze(xs, ys, angs, seconds: float, seed: int = 0, delta: float = 2e-3):
    """Shrink the container as far as annealing can in `seconds`.

    Returns (x, y, angle, L) of the best feasible state found (feasible to
    FEASIBLE; the caller opens that with `repair` and checks with the
    metric), or None."""
    x = np.array(xs, dtype=np.float64)
    y = np.array(ys, dtype=np.float64)
    a = np.array(angs, dtype=np.float64)
    x0, y0, x1, y1 = bounds(x, y, a)
    x -= x0
    y -= y0
    L = max(x1 - x0, y1 - y0)
    n = x.shape[0]
    best = None
    bx, by, ba, bL = x.copy(), y.copy(), a.copy(), L
    iters = max(20000, 400 * n)
    end = time.time() + seconds
    k = 0
    while time.time() < end and delta > 1e-10:
        k += 1
        f = 1.0 - delta
        x, y, a = bx * f, by * f, ba.copy()
        Lt = bL * f
        step = anneal(x, y, a, Lt, iters, 1e-6, 1e-13, 0.02, seed * 1000 + k)
        anneal(x, y, a, Lt, iters // 2, 1e-14, 1e-18, min(step, 1e-3), seed * 1000 + k + 500)
        if worst(x, y, a, Lt) < FEASIBLE:
            bx, by, ba, bL = x, y, a, Lt
            best = (bx.copy(), by.copy(), ba.copy(), bL)
            delta *= 1.5
        else:
            delta *= 0.5
    return best


def repair(x, y, a, L):
    """Open every residual overlap (<= FEASIBLE) by scaling the centres apart."""
    m = worst(x, y, a, L)
    f = 1.0 + 4.0 * m + 1e-13
    c = L / 2.0
    return c + (x - c) * f, c + (y - c) * f, a


def restart(n: int, L: float, seconds: float, seed: int = 0):
    """Random start in a fixed container of side L, annealed until feasible
    or out of time. Returns (x, y, angle) or None. The way the recent records
    for n near 50 were found (Schadt, Ellsworth 2025-26), at far more compute."""
    rng = np.random.default_rng(seed)
    end = time.time() + seconds
    iters = max(50000, 2000 * n)
    k = 0
    while time.time() < end:
        k += 1
        x = rng.uniform(0.5, L - 0.5, n)
        y = rng.uniform(0.5, L - 0.5, n)
        a = np.where(rng.random(n) < 0.5, 0.0, rng.uniform(0, math.pi / 2, n))
        step = anneal(x, y, a, L, iters, 1e-3, 1e-12, 0.1, seed * 100000 + k)
        anneal(x, y, a, L, iters // 2, 1e-14, 1e-18, min(step, 1e-3), seed * 100000 + k + 50000)
        if worst(x, y, a, L) < FEASIBLE:
            return x, y, a
    return None
