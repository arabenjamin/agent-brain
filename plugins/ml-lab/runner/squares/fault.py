"""Fault-line composition: gadgets along a staircase between two grids (B2).

The method of the public notebooks behind squares-study S02 and S03
(Deotte's multi-gadget fault DP), rebuilt here. In a box of side K + f:

- a staircase from the top-left to the bottom-right splits a K×K grid of
  cells. Cells below it hold upright squares on the integer lattice, flush
  with the left and bottom walls ("00"); cells above hold upright squares
  shifted by (+f, +f), flush with the right and top walls ("11"). Below is
  a down-set, so no 00 square meets an 11 square: K² squares, for any f > 0.
- Along the staircase, gadgets replace w×h blocks of cells. A gadget is a
  corner crop of a dense packing: w·h + Δ squares inside (w + f_m)×(h + f_m).
  In a gadget's columns the 00 cells end below it and the 11 cells start
  above it; in its rows, 00 to the left and 11 to the right. Every gadget
  therefore sees the same (+f, +f) jump, and the gains add up:
  n = K² + ΣΔ in side K + max f_m.
- Gadgets march from the top-left (x from 0, y down from K). Two adjacent
  gadgets could share an f×f corner, so a chain places one buffer cell
  between them, in x or in y: Σw + x-buffers <= K, Σh + y-buffers <= K.

The library: for every donor packing (the store's, and the MIT registers'
records for n up to 3500 under external/, the best file per n) and every
symmetry of the square, the crop of footprint (w, h) needs, for square i, the bloat
r_i = max(0, x_hi - w, y_hi - h); sorting the r_i gives the least f for each
count. The DP over (Σw, Σh, ΣΔ) keeps the least max f.

    python -m squares.fault /data
"""

from __future__ import annotations

import math
import sys
from concurrent.futures import ProcessPoolExecutor

import numpy as np
from numba import njit

from fractions import Fraction
from pathlib import Path

from . import register
from .metric import MAX_N
from .packing import Packing, Store, drop_to, from_floats

WMAX = 31  # gadget footprints and K up to 31 (n <= 1000)
DMAX = 2 * WMAX
F_MAX = 0.999  # a gadget needs f < 1


def _half(a):
    return 0.5 * (np.abs(np.cos(a)) + np.abs(np.sin(a)))


def _sym(x, y, a, L, s):
    """One of the 8 symmetries of the square [0, L]², on centres and angles."""
    if s & 1:
        x, a = L - x, -a
    if s & 2:
        y, a = L - y, -a
    if s & 4:
        x, y, a = y, x, math.pi / 2 - a
    return x, y, a


def _norm(x, y, a):
    h = _half(a)
    x, y = x - (x - h).min(), y - (y - h).min()
    return x, y, a, float(max((x + h).max(), (y + h).max()))


def _crops(args):
    """Per donor: F[w, h, d] the least bloat of a corner crop of footprint
    w×h holding w·h + d squares, and the symmetry that gives it."""
    i, sq = args
    n = len(sq)
    S = WMAX + 1
    F = np.full((S, S, DMAX + 1), np.inf)
    SYM = np.zeros((S, S, DMAX + 1), dtype=np.int8)
    f0 = np.asarray(sq, dtype=float)
    x, y, a, L = _norm(f0[:, 0], f0[:, 1], f0[:, 2])
    top = min(WMAX, int(L) + 1)
    hs = np.arange(1, top + 1)
    ds = np.arange(1, DMAX + 1)
    for s in range(8):
        xs, ys, as_ = _sym(x, y, a, L, s)
        h = _half(as_)
        xhi, yhi = xs + h, ys + h
        for w in range(1, top + 1):
            r = np.maximum(np.maximum(0.0, xhi[None, :] - w), yhi[None, :] - hs[:, None])
            r.sort(axis=1)
            c = w * hs[:, None] + ds[None, :]  # squares wanted, per (h, d)
            ok = c <= n
            f = np.where(ok, r[np.arange(len(hs))[:, None], np.minimum(c, n) - 1], np.inf)
            f[f >= F_MAX] = np.inf
            cur = F[w, 1:top + 1, 1:]
            better = f < cur
            cur[better] = f[better]
            SYM[w, 1:top + 1, 1:][better] = s
    return i, F, SYM


def _floats(path: str):
    """A register file as float (x, y, angle) rows; certificates hold exact
    rationals and the rotation as t = tan(theta/2)."""
    p = Path(path)
    if p.suffix == ".cert":
        lines = [l for l in p.read_text(errors="replace").splitlines() if l.strip() and not l.startswith("#")]
        n = int(lines[0].split()[0])
        return [(float(Fraction(x)), float(Fraction(y)), 2 * math.atan(float(Fraction(t))))
                for x, y, t in (l.split()[:3] for l in lines[1:n + 1])]
    pk = register.read(p)
    return None if pk is None else pk.floats()


def _load(args):
    n, path = args
    try:
        sq = _floats(path)
    except (ValueError, IndexError, ZeroDivisionError, OSError):
        return None
    return (n, sq) if sq and len(sq) == n else None


def donors(store: Store, workers: int, top: int = 3500) -> list[tuple[int, list, str]]:
    """(n, squares, source): the store's packings, then the best register
    file for each n above MAX_N up to `top`."""
    out = [(n, [tuple(map(float, q)) for q in rec["squares"]], f"store n={n}")
           for n in range(2, MAX_N + 1) if (rec := store.get(n)) is not None]
    best: dict[int, tuple[float, str]] = {}
    for f in (f for g in ("*.txt", "*.cert") for f in (store.root / "external").rglob(g)):
        h = register.header(f)
        if h and MAX_N < h[0] <= top and h[1] < best.get(h[0], (9e9,))[0]:
            best[h[0]] = (h[1], str(f))
    with ProcessPoolExecutor(workers) as pool:
        for r in pool.map(_load, [(n, p) for n, (_, p) in sorted(best.items())], chunksize=8):
            if r is not None:
                out.append((r[0], r[1], f"{register._repo(Path(best[r[0]][1]))} (MIT): {Path(best[r[0]][1]).name}"))
    return out


def library(store: Store, workers: int, pool_donors: list) -> dict:
    """(w, h, d) -> (f, donor index, symmetry): the best crop over every
    donor, keeping a gadget only if no smaller footprint holds as many extra
    squares in as little bloat."""
    S = WMAX + 1
    F = np.full((S, S, DMAX + 1), np.inf)
    DON = np.zeros(F.shape, dtype=np.int64)
    SYM = np.zeros(F.shape, dtype=np.int8)
    jobs = [(i, sq) for i, (_, sq, _) in enumerate(pool_donors)]
    with ProcessPoolExecutor(workers) as pool:
        for i, Fn, Sn in pool.map(_crops, jobs, chunksize=4):
            better = Fn < F
            F[better], DON[better], SYM[better] = Fn[better], i, Sn[better]
    # A footprint's gadget is dominated by any smaller footprint (w' <= w,
    # h' <= h) with the same d and no more f: prefix minima over w and h.
    pm = np.minimum.accumulate(np.minimum.accumulate(F, axis=0), axis=1)
    smaller = np.full(F.shape, np.inf)
    smaller[1:, :, :] = pm[:-1, :, :]
    smaller[:, 1:, :] = np.minimum(smaller[:, 1:, :], pm[:, :-1, :])
    keep = {}
    for w, h, d in zip(*np.nonzero(np.isfinite(F) & (F < smaller))):
        keep[(int(w), int(h), int(d))] = (float(F[w, h, d]), int(DON[w, h, d]), int(SYM[w, h, d]))
    return keep


@njit(cache=True)
def _dp(gw, gh, gd, gf):
    """best[W, H, D]: least max f over chains with Σw(+x buffers) = W,
    Σh(+y buffers) = H and ΣΔ = D. par holds the step back."""
    S = WMAX + 1
    best = np.full((S, S, DMAX + 1), np.inf)
    par = np.full((S, S, DMAX + 1, 4), -1, dtype=np.int64)  # gadget, pW, pH, pD
    best[0, 0, 0] = 0.0
    for tot in range(0, 2 * WMAX + 1):
        for W in range(0, min(tot, WMAX) + 1):
            H = tot - W
            if H > WMAX:
                continue
            for D in range(0, DMAX + 1):
                f0 = best[W, H, D]
                if f0 == np.inf:
                    continue
                first = W == 0 and H == 0
                for g in range(len(gw)):
                    nd = D + gd[g]
                    if nd > DMAX:
                        continue
                    f = max(f0, gf[g])
                    for bx in range(0, 1 if first else 2):
                        nW = W + gw[g] + bx
                        nH = H + gh[g] + (0 if first else 1 - bx)
                        if nW > WMAX or nH > WMAX:
                            continue
                        if f < best[nW, nH, nd]:
                            best[nW, nH, nd] = f
                            par[nW, nH, nd, 0] = g
                            par[nW, nH, nd, 1] = W
                            par[nW, nH, nd, 2] = H
                            par[nW, nH, nd, 3] = D
    return best, par


def build(pool_donors: list, K: int, gadgets: list[tuple], f: float) -> Packing:
    """gadgets: [(w, h, donor index, symmetry, extra W, extra H)] top-left
    first; extra W/H is the buffer before it."""
    sq: list[tuple[float, float, float]] = []
    ox, top = 0, K
    blocks = []
    for w, h, dn, s, bx, by in gadgets:
        ox += bx
        top -= by
        oy = top - h
        blocks.append((ox, oy, w, h, dn, s))
        ox += w
        top = oy
    # Column heights of the 00 region (non-increasing), and the gadget rows.
    height = [0] * K
    cur = K
    gcol = {}
    for ox, oy, w, h, *_ in blocks:
        for i in range(ox, ox + w):
            gcol[i] = (oy, oy + h)
    for i in range(K):
        if i in gcol:
            cur = gcol[i][0]
        height[i] = cur
    for i in range(K):
        lo, hi = gcol.get(i, (height[i], height[i]))
        for j in range(K):
            if j < lo:
                sq.append((i + 0.5, j + 0.5, 0.0))
            elif j >= hi:
                sq.append((i + 0.5 + f, j + 0.5 + f, 0.0))
    for ox, oy, w, h, dn, s in blocks:
        f0 = np.array(pool_donors[dn][1])
        x, y, a, L = _norm(f0[:, 0], f0[:, 1], f0[:, 2])
        x, y, a = _sym(x, y, a, L, s)
        hh = _half(a)
        r = np.maximum(np.maximum(0.0, x + hh - w), y + hh - h)
        idx = np.argsort(r, kind="stable")[: w * h + _extra(w, h, r, f)]
        for q in idx:
            sq.append((ox + x[q], oy + y[q], a[q]))
    xs, ys, as_ = (np.array(v) for v in zip(*sq))
    return from_floats(len(sq), xs, ys, as_, f"fault-line K={K}: " + " + ".join(
        f"{w}x{h} from {pool_donors[dn][2]}" for _, _, w, h, dn, _ in blocks))


def _extra(w, h, r, f):
    """How many squares beyond w·h the crop holds within bloat f."""
    return int((r <= f + 1e-12).sum()) - w * h


def plan(store: Store, workers: int, pool_donors: list):
    lib = library(store, workers, pool_donors)
    keys = sorted(lib)
    gw = np.array([k[0] for k in keys], dtype=np.int64)
    gh = np.array([k[1] for k in keys], dtype=np.int64)
    gd = np.array([k[2] for k in keys], dtype=np.int64)
    gf = np.array([lib[k][0] for k in keys])
    best, par = _dp(gw, gh, gd, gf)
    found = {}
    for K in range(2, WMAX + 1):
        sub = best[: K + 1, : K + 1, :]
        for D in range(1, 2 * K + 1):
            n = K * K + D
            if n > MAX_N:
                break
            col = sub[:, :, D:].reshape(-1)
            if not np.isfinite(col.min()):
                continue
            W, H, Dd = np.unravel_index(int(np.argmin(sub[:, :, D:])), sub[:, :, D:].shape)
            f = float(sub[W, H, D + Dd])
            if K + f < found.get(n, (9e9,))[0]:
                found[n] = (K + f, K, int(W), int(H), int(D + Dd))
    return lib, keys, par, found


def _gadgets(par, keys, lib, W, H, D):
    steps = []
    while (W, H, D) != (0, 0, 0):
        g, pW, pH, pD = (int(v) for v in par[W, H, D])
        w, h, d = keys[g]
        bx = (W - pW) - w
        by = (H - pH) - h
        steps.append((w, h, lib[keys[g]][1], lib[keys[g]][2], bx, by))
        W, H, D = pW, pH, pD
    steps.reverse()
    return steps


def main(root: str, workers: int = 12) -> None:
    store = Store(root)
    pool_donors = donors(store, workers)
    lib, keys, par, found = plan(store, workers, pool_donors)
    print(f"{len(pool_donors)} donors, {len(lib)} gadgets; {len(found)} puzzles reachable", flush=True)
    jobs, cur = [], None
    for n in range(MAX_N, 0, -1):
        if n in found and (cur is None or found[n][0] < cur[1][0]):
            cur = (n, found[n])
        if cur is not None and cur[1][0] < store.L(n) - 1e-9:
            jobs.append((n, cur))
    stored = 0
    for n, (nb, (L, K, W, H, D)) in jobs:
        g = _gadgets(par, keys, lib, W, H, D)
        f = L - K
        p = build(pool_donors, K, g, f)
        if p.n < n:
            print(f"n={n}: built {p.n}", flush=True)
            continue
        ok, why = store.offer(drop_to(p, n), "fault")
        stored += ok
        print(f"n={n}: L {L:.9f} vs {store.L(n):.9f} {why}" if not ok else f"n={n}: stored {store.L(n):.9f}", flush=True)
    print(f"tried {len(jobs)}, stored {stored}; local total {store.total():.6f}")


if __name__ == "__main__":
    main(sys.argv[1], int(sys.argv[2]) if len(sys.argv) > 2 else 12)
