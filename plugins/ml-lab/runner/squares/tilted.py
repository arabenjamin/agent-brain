"""A tilted core with upright rows around it (ML_LAB_PLAN.md B2).

Generalises the Göbel square: a b1×b2 block of unit squares turned 45° in
the centre of a container of side s, and rows of upright squares from the
bottom and top walls, each row as long as the core leaves it on the left and
the right. The Göbel square's formula allows only some (a, b); this counts
every b1, b2 and the side lengths where a row gains a square, s = (i + p/√2)/2.
The family behind the strongest public large-n packings (Chris Deotte's
gallery on the competition forum, 2026-10-22: n = 626 as a 16×16 diamond).

    python -m squares.tilted /data
"""

from __future__ import annotations

import math
import sys
from concurrent.futures import ProcessPoolExecutor

import mpmath

from .metric import MAX_N
from .packing import DPS, Packing, Store, drop_to, fmt

F_EPS = 1e-9  # the search's tolerance; the exact build below decides


def _core(s, b1, b2, sqrt2, half):
    c = s / 2
    u = (1 / sqrt2, 1 / sqrt2)
    v = (-1 / sqrt2, 1 / sqrt2)
    return [(c + su * b1 * half * u[0] + sv * b2 * half * v[0],
             c + su * b1 * half * u[1] + sv * b2 * half * v[1])
            for su, sv in ((1, 1), (1, -1), (-1, -1), (-1, 1))]


def _band_x(poly, y0, y1, eps):
    """The x-range of the convex polygon inside y0 <= y <= y1, or None."""
    xs = []
    for i in range(len(poly)):
        (xa, ya), (xb, yb) = poly[i], poly[(i + 1) % len(poly)]
        if y0 - eps <= ya <= y1 + eps:
            xs.append(xa)
        for yy in (y0, y1):
            if (ya - yy) * (yb - yy) < 0:
                xs.append(xa + (yy - ya) * (xb - xa) / (yb - ya))
    return (min(xs), max(xs)) if xs else None


def _row(s, poly, y0, eps, floor):
    """(left, right) upright squares in the row y0..y0+1."""
    r = _band_x(poly, y0, y0 + 1, eps)
    if r is None:
        return floor(s + eps), 0
    xl, xr = r
    return max(0, floor(xl + eps)), max(0, floor(s - xr + eps))


def layout(s, b1, b2, eps=F_EPS, sqrt2=math.sqrt(2), half=0.5, floor=math.floor):
    """(count, J, rows) for the best split: J rows from the bottom, the rest
    from the top. rows = [(y0, left, right)]. count -1: the core does not fit."""
    poly = _core(s, b1, b2, sqrt2, half)
    xs = [p[0] for p in poly]
    if min(xs) < -eps or max(xs) > s + eps:
        return -1, 0, []
    H = floor(s + eps)
    best = (-1, 0, [])
    for J in range(H + 1):
        rows = [(j, *_row(s, poly, j, eps, floor)) for j in range(J)]
        rows += [(s - j - 1, *_row(s, poly, s - j - 1, eps, floor)) for j in range(floor(s - J + eps))]
        k = b1 * b2 + sum(l + r for _, l, r in rows)
        if k > best[0]:
            best = (k, J, rows)
    return best


def _search(b1: int) -> dict[int, tuple]:
    """Every side for one b1: n -> (s, i, p, b1, b2)."""
    r2 = math.sqrt(2)
    found: dict[int, tuple] = {}
    for b2 in range(b1, 46):
        for p in {b1, b2, b1 + b2, b2 - b1, 0}:
            for i in range(70):
                s = (i + p / r2) / 2
                if not 2 <= s <= 32:
                    continue
                n, _, _ = layout(s, b1, b2)
                if 0 < n <= MAX_N + 100 and s < found.get(n, (1e9,))[0]:
                    found[n] = (s, i, p, b1, b2)
    return found


def build(i: int, p: int, b1: int, b2: int) -> Packing:
    """The packing, computed at DPS digits."""
    with mpmath.workdps(DPS):
        r2 = mpmath.sqrt(2)
        s = (i + p / r2) / 2
        eps = mpmath.mpf(10) ** (-DPS + 10)
        n, J, rows = layout(s, b1, b2, eps, r2, mpmath.mpf("0.5"), lambda x: int(mpmath.floor(x)))
        h = mpmath.mpf("0.5")
        sq = []
        for y0, left, right in rows:
            for k in range(left):
                sq.append((fmt(k + h), fmt(y0 + h), "0"))
            for k in range(right):
                sq.append((fmt(s - k - h), fmt(y0 + h), "0"))
        c = s / 2
        q = fmt(mpmath.pi / 4)
        for a in range(b1):
            for b in range(b2):
                uu = -mpmath.mpf(b1) / 2 + h + a
                vv = -mpmath.mpf(b2) / 2 + h + b
                sq.append((fmt(c + (uu - vv) / r2), fmt(c + (uu + vv) / r2), q))
    return Packing(len(sq), sq, f"tilted core {b1}x{b2}, s=({i}+{p}/sqrt2)/2")


def _offer(args):
    root, n, i, p, b1, b2 = args
    store = Store(root)
    pk = build(i, p, b1, b2)
    if pk.n < n:
        return n, False, f"built {pk.n} squares, wanted {n}"
    ok, why = store.offer(drop_to(pk, n), "tilted")
    return n, ok, why


def main(root: str, workers: int = 12) -> None:
    store = Store(root)
    found: dict[int, tuple] = {}
    with ProcessPoolExecutor(workers) as pool:
        for part in pool.map(_search, range(1, 46)):
            for n, v in part.items():
                if v[0] < found.get(n, (1e9,))[0]:
                    found[n] = v
    jobs, cur = [], None
    for n in range(MAX_N + 100, 0, -1):  # a packing of N also packs n < N
        if n in found and (cur is None or found[n][0] < cur[0]):
            cur = found[n]
        if n <= MAX_N and cur is not None and cur[0] < store.L(n) - 1e-9:
            _, i, p, b1, b2 = cur
            jobs.append((root, n, i, p, b1, b2))
    stored = 0
    with ProcessPoolExecutor(workers) as pool:
        for n, ok, why in pool.map(_offer, jobs):
            stored += ok
            if not ok:
                print(f"n={n}: {why}", flush=True)
    print(f"tried {len(jobs)}, stored {stored}; local total {store.total():.6f}")


if __name__ == "__main__":
    main(sys.argv[1])
