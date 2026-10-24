"""Göbel squares, generated (ML_LAB_PLAN.md B2).

After Frits Göbel, generalised on Ellsworth's "Göbel squares" page: if
a - 1 < b/√2 < a + 1, then n = 2a(a+1) + b² unit squares fit in a square of
side s = a + 1 + b/√2: a b×b block turned 45° in the centre, and an a-step
staircase of unrotated squares in each corner, touching the block's edges.
Several of these for n > 324 are on the page without an SVG, so they are
built here and checked by the metric like everything else.

    python -m squares.gobel /data
"""

from __future__ import annotations

import math
import sys

import mpmath

from .metric import MAX_N
from .packing import DPS, Packing, Store, drop_to, fmt


def gobel_square(a: int, b: int) -> Packing:
    with mpmath.workdps(DPS):
        r = mpmath.sqrt(2)
        s = a + 1 + b / r
        c = s / 2
        sq = []
        # the four staircases: cells (i, j) with i + j <= a - 1 from each corner
        for i in range(a):
            for j in range(a - i):
                for sx, sy in ((0, 0), (1, 0), (0, 1), (1, 1)):
                    x = (i + mpmath.mpf("0.5")) if sx == 0 else s - i - mpmath.mpf("0.5")
                    y = (j + mpmath.mpf("0.5")) if sy == 0 else s - j - mpmath.mpf("0.5")
                    sq.append((fmt(x), fmt(y), "0"))
        # the b×b block at 45°
        q = fmt(mpmath.pi / 4)
        for i in range(b):
            for j in range(b):
                u = -mpmath.mpf(b) / 2 + mpmath.mpf("0.5") + i
                v = -mpmath.mpf(b) / 2 + mpmath.mpf("0.5") + j
                sq.append((fmt(c + (u - v) / r), fmt(c + (u + v) / r), q))
    return Packing(len(sq), sq, f"Göbel square a={a} b={b}")


def families() -> list[tuple[int, float, int, int]]:
    """(n, s, a, b) for every Göbel square that beats the grid, n <= MAX_N + ..."""
    out = []
    for a in range(1, 40):
        for b in range(1, 60):
            if a - 1 < b / math.sqrt(2) < a + 1:
                n = 2 * a * (a + 1) + b * b
                s = a + 1 + b / math.sqrt(2)
                if s < math.ceil(math.sqrt(n)):
                    out.append((n, s, a, b))
    return out


def main(root: str) -> None:
    store = Store(root)
    fams = families()
    tried = 0
    for n in range(1, MAX_N + 1):
        fits = [(s, a, b) for N, s, a, b in fams if N >= n and s < store.L(n) - 1e-9]
        if not fits:
            continue
        s, a, b = min(fits)
        ok, why = store.offer(drop_to(gobel_square(a, b), n), "gobel")
        tried += 1
        print(f"n={n}: a={a} b={b} s={s:.6f}: {why}", flush=True)
    print(f"tried {tried}; local total {store.total():.6f}")


if __name__ == "__main__":
    main(sys.argv[1])
