"""Edge LP polish: tighten a packing by linear programs (ML_LAB_PLAN.md B3).

The public method behind the best notebook (squares-study S01, "fixed-axis
centre LP"). With every angle fixed, two squares are apart exactly when some
edge normal u of either one separates them: σ u·(c_j - c_i) >= r_i(u) + r_j(u),
r(u) the half-width of a square along u. Fix that axis per pair and the
condition is linear in the centres, so one LP moves every centre at once to
the smallest side L. Its solutions are feasible by construction (a fixed axis
only narrows the feasible set), up to the solver's tolerance, which MARGIN
covers.

`repair` is that exact LP. `slp` also lets the angles move, with every
constraint linearised in them; its proposal is only a direction, which
`repair` then makes feasible at the new angles. `squeeze` holds the side
below the current one and minimises the total overlap (one slack per pair),
which lets the whole packing rearrange. `polish` hops: from the best packing
it perturbs a few squares, moves one elsewhere, or squeezes, repairs, descends, and keeps what is
smaller. Two squares farther apart than √2 + 2t cannot meet after moving t
each, so only nearer pairs get a constraint.
"""

from __future__ import annotations

import math
import time

import numpy as np
from scipy.optimize import linprog
from scipy.sparse import coo_matrix
from scipy.spatial import cKDTree

MARGIN = 5e-10  # covers HiGHS's feasibility tolerance (1e-10, its floor)
SQRT2 = math.sqrt(2)
_deadline = math.inf  # polish's end time; the inner loops stop there too
_OPTS = {"primal_feasibility_tolerance": 1e-10, "dual_feasibility_tolerance": 1e-10, "presolve": True}


def _r(psi):
    """Half-width of a unit square along a direction at angle psi to its edges."""
    return 0.5 * (np.abs(np.cos(psi)) + np.abs(np.sin(psi)))


def _dr(psi):
    return 0.5 * (-np.sign(np.cos(psi)) * np.sin(psi) + np.sign(np.sin(psi)) * np.cos(psi))


def side(x, y, a) -> float:
    h = _r(a)
    return float(max((x + h).max() - (x - h).min(), (y + h).max() - (y - h).min()))


def _axes(x, y, a, i, j):
    """Per pair, the edge normal (of i or j) that separates them most now:
    (phi, owner, sigma, separation)."""
    dx, dy = x[j] - x[i], y[j] - y[i]
    best = None
    for owner, base in ((0, a[i]), (1, a[j])):
        for k in (0, 1):
            phi = base + k * math.pi / 2
            proj = np.cos(phi) * dx + np.sin(phi) * dy
            sep = np.abs(proj) - _r(a[i] - phi) - _r(a[j] - phi)
            cand = (phi, np.full(len(i), owner), np.where(proj >= 0, 1.0, -1.0), sep)
            if best is None:
                best = list(cand)
            else:
                m = sep > best[3]
                for b, c in zip(best, cand):
                    b[m] = c[m]
    return best


def _pairs(x, y, t):
    pts = np.column_stack([x, y])
    pr = cKDTree(pts).query_pairs(SQRT2 + 2 * t + 1e-6, output_type="ndarray")
    return pr[:, 0], pr[:, 1]


def overlaps(x, y, a, tol=0.0) -> int:
    """How many pairs overlap by more than tol (float SAT)."""
    i, j = _pairs(x, y, 0.0)
    if len(i) == 0:
        return 0
    return int((_axes(x, y, a, i, j)[3] < -tol).sum())


def _kink(psi):
    """True where a half-width has its corner (an edge along the direction):
    there it grows whichever way the square turns."""
    q = np.mod(psi, math.pi / 2)
    return (q < 1e-9) | (q > math.pi / 2 - 1e-9)


def _solve(x, y, a, t, ta, L_fixed=None):
    """One LP from (x, y, a) with position trust t and angle trust ta (0: fixed
    angles). Returns new (x, y, a) or None if infeasible. With L_fixed the
    side is held there and each pair may overlap by a slack, whose sum is
    minimised instead (the squeeze); the slack total is returned fourth.

    With ta, each half-width r(psi) is linearised in the angles; at a kink
    (psi a multiple of 90°) r grows as |d psi| / 2 either way, so an auxiliary
    z >= |d psi| stands in for it (a slope of 0 there would make turning free)."""
    n = len(x)
    i, j = _pairs(x, y, t + ta)
    phi, owner, sig, _ = _axes(x, y, a, i, j)
    m = len(i)
    A0 = 2 * n + 1  # angle variables start here
    Lc = 2 * n
    nv = A0 + (n if ta else 0)
    rows, cols, vals, rhs = [], [], [], []
    r = 0

    def add(rr, cc, vv):
        rows.append(np.asarray(rr)), cols.append(np.asarray(cc)), vals.append(np.broadcast_to(vv, np.shape(rr)).astype(float))

    def absvar(count):
        nonlocal nv
        z = np.arange(nv, nv + count)
        nv += count
        return z

    def bound_abs(z, plus, minus):
        """z >= +-(a[plus] - a[minus]) as rows (minus may be None); returns rhs."""
        nonlocal r
        out = []
        for sgn in (1.0, -1.0):
            k = np.arange(len(z))
            add(r + k, z, -1.0)
            add(r + k, A0 + plus, sgn)
            c0 = sgn * a[plus]
            if minus is not None:
                add(r + k, A0 + minus, -sgn)
                c0 = c0 - sgn * a[minus]
            out.append(c0)
            r += len(z)
        return out

    # Pairs: sig u·(c_j - c_i) - r_i - r_j >= MARGIN, as -(...) <= -MARGIN.
    if m:
        ux, uy = np.cos(phi), np.sin(phi)
        pidx = np.arange(m)
        for v, sg in ((i, -1.0), (j, 1.0)):
            add(pidx, v, -sig * sg * ux)
            add(pidx, n + v, -sig * sg * uy)
        ri, rj = _r(a[i] - phi), _r(a[j] - phi)
        b = -(ri + rj) - MARGIN
        if ta:
            dx, dy = x[j] - x[i], y[j] - y[i]
            # The axis turns with its owner: d(sig u·d)/d(owner) = sig u'·d.
            # The other square's half-width depends on psi = other - owner.
            du = sig * (-np.sin(phi) * dx + np.cos(phi) * dy)
            own = np.where(owner == 0, i, j)
            oth = np.where(owner == 0, j, i)
            psi = a[oth] - phi
            kink = _kink(psi)
            dro = np.where(kink, 0.0, _dr(psi))
            add(pidx, A0 + own, -du - dro)
            add(pidx, A0 + oth, dro)
            b = b + (-du - dro) * a[own] + dro * a[oth]
            kk = np.flatnonzero(kink)
            if len(kk):
                z = absvar(len(kk))
                add(kk, z, 0.5)
        if L_fixed is not None:
            slack = absvar(m)
            add(pidx, slack, -1.0)
        rhs.append(b)
        r = m
        if ta and len(kk):
            rhs += bound_abs(z, oth[kk], own[kk])
    # Walls: h_k - x_k <= 0 ; x_k + h_k - L <= 0 (same in y).
    h = _r(a)
    k = np.arange(n)
    if ta:
        kink = _kink(a)
        dh = np.where(kink, 0.0, _dr(a))
        kw = np.flatnonzero(kink)
        zw = absvar(len(kw)) if len(kw) else None
    for off in (0, n):
        for upper in (False, True):
            add(r + k, off + k, 1.0 if upper else -1.0)
            if upper:
                add(r + k, np.full(n, Lc), -1.0)
            b = -h
            if ta:
                add(r + k, A0 + k, dh)
                b = b + dh * a
                if zw is not None:
                    add(r + kw, zw, 0.5)
            rhs.append(b)
            r += n
    if ta and zw is not None:
        rhs += bound_abs(zw, kw, None)
    A = coo_matrix((np.concatenate(vals), (np.concatenate(rows), np.concatenate(cols))), shape=(r, nv)).tocsr()
    c = np.zeros(nv)
    if L_fixed is None:
        c[Lc] = 1.0
    else:
        c[slack] = 1.0
    bounds = [(v - t, v + t) for v in x] + [(v - t, v + t) for v in y]
    bounds += [(0, None) if L_fixed is None else (L_fixed, L_fixed)]
    if ta:
        bounds += [(v - ta, v + ta) for v in a]
    bounds += [(0, None)] * (nv - len(bounds))
    res = linprog(c, A_ub=A, b_ub=np.concatenate(rhs), bounds=bounds, method="highs", options=_OPTS)
    if res.status != 0:
        return None
    z = res.x
    out = z[:n].copy(), z[n:2 * n].copy(), (z[A0:A0 + n].copy() if ta else a.copy())
    return out if L_fixed is None else (*out, float(z[slack].sum()) if m else 0.0)


def repair(x, y, a, t=0.05, rounds=30):
    """The exact fixed-angle LP, repeated (each re-picks the axes) until the
    side stops shrinking. Returns a feasible (x, y, a, L) or None."""
    x, y, a = normalise(x, y, a)  # the LP's box has its corner at the origin
    cur = None
    for _ in range(rounds):
        out = _solve(x, y, a, t, 0.0)
        if out is None:
            break
        x, y, _ = out
        L = side(x, y, a)
        improved = cur is None or L < cur[3] - 1e-13
        if cur is None or L < cur[3]:
            cur = (x, y, a, L)
        if not improved:
            break
    return cur


def slp(x, y, a, ta=1e-3, steps=20):
    """Descend with the angles free: linearised step, then exact repair."""
    best = repair(x, y, a)
    if best is None:
        return None
    for _ in range(steps):
        if time.time() > _deadline:
            break
        prop = _solve(best[0], best[1], best[2], ta * 4, ta)
        if prop is None:
            ta /= 2
            continue
        fix = repair(*prop)
        if fix is not None and fix[3] < best[3] - 1e-12 and overlaps(fix[0], fix[1], fix[2]) == 0:
            best = fix
        else:
            ta /= 2
        if ta < 1e-7:
            break
    return best


def squeeze(x, y, a, Lt, rounds=60, t=0.02, ta=5e-3, scale=True):
    """Into side Lt: scale the centres in, then minimise the total overlap by
    slack LPs, re-picking the axes each round. Returns a feasible
    (x, y, a, L) with L < the start, or None."""
    x, y, a = normalise(x, y, a)
    L0 = side(x, y, a)
    h = _r(a)
    if scale:
        f = (Lt - 2 * h.mean()) / (L0 - 2 * h.mean())
        x, y = h + (x - h) * f, h + (y - h) * f
    x, y = np.clip(x, h, Lt - h), np.clip(y, h, Lt - h)
    best_tot, flat = math.inf, 0
    for _ in range(rounds):
        if time.time() > _deadline:
            return None
        out = _solve(x, y, a, t, ta, L_fixed=Lt)
        if out is None:
            return None
        x, y, a, tot = out
        if tot < 1e-11:
            break
        if tot < best_tot * 0.995:
            best_tot, flat = tot, 0
        else:
            flat += 1  # the axes flip between rounds near a minimum
            if flat >= 6:
                t, ta, flat = t / 2, ta / 2, 0
                if t < 1e-4:
                    return None
    else:
        return None
    fix = repair(x, y, a)
    if fix is None or fix[3] >= L0 - 1e-12 or overlaps(*fix[:3]):
        return None
    return fix


def normalise(x, y, a):
    h = _r(a)
    return x - (x - h).min(), y - (y - h).min(), a


def polish(x, y, a, seconds: float, seed: int = 0):
    """Basin hopping from the given packing; returns (x, y, a, L) of the best
    feasible packing found, or None if nothing beat the start."""
    x, y, a = (np.asarray(v, dtype=float) for v in (x, y, a))
    L0 = side(x, y, a)
    global _deadline
    rng = np.random.default_rng(seed)
    t_end = _deadline = time.time() + seconds
    best = slp(x, y, a)
    if best is None:
        best = (x, y, a, L0)
    n = len(x)
    while time.time() < t_end:
        bx, by, ba, _ = best
        px, py, pa = bx.copy(), by.copy(), ba.copy()
        k = rng.integers(1, min(4, n) + 1)
        idx = rng.choice(n, k, replace=False)
        kind = rng.integers(5)
        if kind == 4:  # pick one square up, drop it elsewhere, absorb at this side
            q = rng.integers(n)
            px[q], py[q] = rng.uniform(0.5, best[3] - 0.5, 2)
            pa[q] = rng.choice([0.0, math.pi / 4, rng.uniform(0, math.pi / 2)])
            out = squeeze(px, py, pa, best[3] * (1 - 1e-6), rounds=40, t=0.1, ta=2e-2, scale=False)
            if out is not None and out[3] < best[3] - 1e-12:
                best = slp(*out[:3], ta=1e-3, steps=6) or out
            continue
        if kind == 3:  # squeeze the whole packing into a smaller side
            Lt = best[3] * (1 - rng.choice([1e-5, 1e-4, 1e-3, 3e-3]))
            out = squeeze(bx, by, ba, Lt)
            if out is not None and out[3] < best[3] - 1e-12:
                best = slp(*out[:3], ta=1e-3, steps=6) or out
            continue
        if kind == 0:  # small rotations
            pa[idx] += rng.normal(0, rng.choice([1e-3, 1e-2, 5e-2]), k)
        elif kind == 1:  # snap to upright or 45°
            pa[idx] = rng.choice([0.0, math.pi / 4], k)
        else:  # a nudge in position and angle
            px[idx] += rng.normal(0, 0.02, k)
            py[idx] += rng.normal(0, 0.02, k)
            pa[idx] += rng.normal(0, 0.01, k)
        fix = repair(px, py, pa, t=0.1)
        if fix is None:
            continue
        out = slp(*fix[:3], ta=1e-3, steps=6)
        if out is not None and out[3] < best[3] - 1e-12 and overlaps(out[0], out[1], out[2]) == 0:
            best = out
    if best[3] >= L0 - 1e-10:
        return None
    nx, ny, na = normalise(*best[:3])
    return nx, ny, na, side(nx, ny, na)
