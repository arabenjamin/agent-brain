"""A small packing in a corner, upright squares everywhere else (B2).

If m squares fit in side t, then in side s = t + k they fit with
N² - (N - k)² upright squares more (N = ⌊s⌋), the upright ones aligned to the
far corner. Several values on the strongest public curve (Chris Deotte's
gallery, 2026-10-22) are of this form, e.g. s(335) = 14 + s(18). The small
packing comes from our own store, so a second pass builds on the first.

    python -m squares.composite /data
"""

from __future__ import annotations

import math
import sys
from concurrent.futures import ProcessPoolExecutor

import mpmath

from .metric import MAX_N, compute_corners_mp
from .packing import DPS, Packing, Store, drop_to, fmt


def build(store: Store, m: int, k: int) -> Packing:
    small = store.packing(m)
    with mpmath.workdps(DPS):
        pts = [(mpmath.mpf(x), mpmath.mpf(y), mpmath.mpf(a)) for x, y, a in small.squares]
        xs, ys = [], []
        for x, y, a in pts:
            for cx, cy in compute_corners_mp(x, y, a):
                xs.append(cx)
                ys.append(cy)
        x0, y0 = min(xs), min(ys)
        t = max(max(xs) - x0, max(ys) - y0)
        s = t + k
        N = int(mpmath.floor(s))
        h = mpmath.mpf("0.5")
        sq = [(fmt(x - x0), fmt(y - y0), fmt(a)) for x, y, a in pts]
        for i in range(N):
            for j in range(N):
                if i < k or j < k:  # outside the small packing's corner
                    sq.append((fmt(s - i - h), fmt(s - j - h), "0"))
    return Packing(len(sq), sq, f"s({m}) + {k}: {small.source}")


def _offer(args):
    root, n, m, k = args
    store = Store(root)
    p = build(store, m, k)
    if p.n < n:
        return n, False, f"built {p.n}, wanted {n}"
    return (n, *store.offer(drop_to(p, n), "composite"))


def main(root: str, workers: int = 12) -> None:
    store = Store(root)
    L = {n: store.L(n) for n in range(1, MAX_N + 1)}
    best: dict[int, tuple] = {}
    for m in range(2, MAX_N):
        t = L[m]
        if t >= math.ceil(math.sqrt(m)) - 1e-9:
            continue  # a grid corner adds nothing over the grid
        for k in range(1, 32):
            s = t + k
            N = math.floor(s + 1e-9)
            n = m + N * N - (N - k) ** 2
            if n <= MAX_N and s < best.get(n, (1e9,))[0]:
                best[n] = (s, m, k)
    jobs, cur = [], None
    for n in range(MAX_N, 0, -1):
        if n in best and (cur is None or best[n][0] < cur[0]):
            cur = best[n]
        if cur is not None and cur[0] < L[n] - 1e-9:
            jobs.append((root, n, cur[1], cur[2]))
    stored = 0
    with ProcessPoolExecutor(workers) as pool:
        for n, ok, why in pool.map(_offer, jobs):
            stored += ok
            if not ok:
                print(f"n={n}: {why}", flush=True)
    print(f"tried {len(jobs)}, stored {stored}; local total {store.total():.6f}")


if __name__ == "__main__":
    main(sys.argv[1])
