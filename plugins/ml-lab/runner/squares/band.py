"""A tilted band along the diagonal, upright staircases on both sides (B2).

Göbel's strip is one row of 45° squares along the diagonal between two
staircases; this widens it to w rows and shifts it by o off the diagonal.
For w = 1 it is the Göbel strip (n = 27 in s = 5 + √2/2). Side lengths are
scanned as s = (i + p/√2)/2; the band offset o is chosen where a staircase
gains a row.

    python -m squares.band /data
"""

from __future__ import annotations

import math
import sys
from concurrent.futures import ProcessPoolExecutor

import mpmath

from .metric import MAX_N
from .packing import DPS, Packing, Store, drop_to, fmt


def _tri(q: int) -> int:
    return q * (q + 1) // 2 if q > 0 else 0


def layout(s, w, o, r2, eps, floor):
    """(n, rows, q1, q2, N): rows = [(v0, u0, count)] of the band."""
    vt, vb = o + w / 2, o - w / 2
    rows = []
    for r in range(w):
        v0 = vb + r
        m = max(abs(v0), abs(v0 + 1))
        rows.append((v0, m, max(0, floor(s * r2 - 2 * m + eps))))
    N = floor(s + eps)
    q1 = min(floor(s - 2 - vt * r2 + eps) + 1, N)
    q2 = min(floor(s - 2 + vb * r2 + eps) + 1, N)
    return sum(c for _, _, c in rows) + _tri(q1) + _tri(q2), rows, q1, q2, N


def _offsets(s, w, r2):
    out = [0 * s, 0 * s + 0.5]
    for q in range(33):
        out += [(s - 2 - q) / r2 - w / 2 * (1 + 0 * s), w / 2 * (1 + 0 * s) - (s - 2 - q) / r2]
    return [o for o in out if abs(o) <= s]


def _search(w: int) -> dict[int, tuple]:
    r2 = math.sqrt(2)
    found: dict[int, tuple] = {}
    for i in range(70):
        for p in range(90):
            s = (i + p / r2) / 2
            if not 2 <= s <= 32:
                continue
            for k, o in enumerate(_offsets(s, w, r2)):
                n = layout(s, w, o, r2, 1e-9, math.floor)[0]
                if 0 < n <= MAX_N and s < found.get(n, (1e9,))[0]:
                    found[n] = (s, i, p, w, k)
    return found


def build(i: int, p: int, w: int, k: int) -> Packing:
    with mpmath.workdps(DPS):
        r2 = mpmath.sqrt(2)
        s = (i + p / r2) / 2
        o = _offsets(s, w, r2)[k]
        eps = mpmath.mpf(10) ** (-DPS + 10)
        n, rows, q1, q2, N = layout(s, w, o, r2, eps, lambda x: int(mpmath.floor(x)))
        h = mpmath.mpf("0.5")
        q = fmt(mpmath.pi / 4)
        sq = []
        for v0, u0, cnt in rows:
            v = v0 + h
            for c in range(cnt):
                u = u0 + c + h
                sq.append((fmt((u - v) / r2), fmt((u + v) / r2), q))
        for a in range(N):
            for b in range(N):
                if a + b <= q1 - 1:  # top-left staircase
                    sq.append((fmt(a + h), fmt(s - b - h), "0"))
                if a + b <= q2 - 1:  # bottom-right staircase
                    sq.append((fmt(s - a - h), fmt(b + h), "0"))
    return Packing(len(sq), sq, f"diagonal band w={w}, s=({i}+{p}/sqrt2)/2")


def _offer(args):
    root, n, i, p, w, k = args
    store = Store(root)
    pk = build(i, p, w, k)
    if pk.n < n:
        return n, False, f"built {pk.n}, wanted {n}"
    return (n, *store.offer(drop_to(pk, n), "band"))


def main(root: str, workers: int = 12) -> None:
    store = Store(root)
    found: dict[int, tuple] = {}
    with ProcessPoolExecutor(workers) as pool:
        for part in pool.map(_search, range(1, 40)):
            for n, v in part.items():
                if v[0] < found.get(n, (1e9,))[0]:
                    found[n] = v
    jobs, cur = [], None
    for n in range(MAX_N, 0, -1):
        if n in found and (cur is None or found[n][0] < cur[0]):
            cur = found[n]
        if cur is not None and cur[0] < store.L(n) - 1e-9:
            jobs.append((root, n, *cur[1:]))
    stored = 0
    with ProcessPoolExecutor(workers) as pool:
        for n, ok, why in pool.map(_offer, jobs):
            stored += ok
            if not ok:
                print(f"n={n}: {why}", flush=True)
    print(f"tried {len(jobs)}, stored {stored}; local total {store.total():.6f}")


if __name__ == "__main__":
    main(sys.argv[1])
