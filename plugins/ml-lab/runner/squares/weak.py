"""Where the store is weak: puzzles above the trend of their neighbours.

Between k² and (k+1)² the same structures repeat as k grows, so the excess
e = L - k at the same relative position u = (n - k²) / (2k + 1) should be
about the same in neighbouring intervals (it is a staircase in u). For each
n, the reference is the smallest excess any of the K intervals below k
reaches at or beyond u (more squares at a smaller excess). Only smaller
intervals count: a packing extends one interval up by a strip of upright
squares (composite.py), so the excess achievable at a position does not grow
with k, while bigger intervals have more room and would flag everything.
Proven puzzles are skipped. The potential is what reaching it would gain in score.

Also counts which family each n's packing comes from, and where the score
sits by band. Writes `weak.json`:

    python -m squares.weak /data
"""

from __future__ import annotations

import json
import math
import sys
from collections import defaultdict
from pathlib import Path

from .metric import MAX_N
from .packing import Store

K = 4  # smaller intervals compared


def family(source: str) -> str:
    s = source.split(" minus ")[0]
    for key, name in (("franciscouzo", "register"), ("evand", "register"), ("Mishapolk", "register"),
                      ("TheSnakeFang", "register"), ("dostashev", "register"), ("Ellsworth", "record page"),
                      ("band", "band"), ("tilted", "tilted"), ("s(", "composite"), ("Göbel", "gobel"),
                      ("gobel", "gobel"), ("grid", "grid"), ("ml-lab", "search"), ("Deotte", "deotte")):
        if key in s:
            return name
    return "other"


def analyse(store: Store) -> dict:
    L = {n: store.L(n) for n in range(1, MAX_N + 1)}
    try:
        proven = set(json.loads((store.root / "proven.json").read_text())["n"])
    except (OSError, ValueError, KeyError):
        proven = set()
    pos = {}
    for n in range(2, MAX_N + 1):
        k = math.isqrt(n)
        if k * k == n:
            continue
        pos[n] = (k, (n - k * k) / (2 * k + 1), L[n] - k)
    by_k = defaultdict(list)
    for n, (k, u, e) in pos.items():
        by_k[k].append((u, e, n))
    rows = []
    for n, (k, u, e) in pos.items():
        if n in proven:
            continue
        ref, src = e, n
        for k2 in range(max(1, k - K), k):
            for u2, e2, n2 in by_k.get(k2, ()):
                if u2 >= u - 1e-12 and e2 < ref - 1e-9:
                    ref, src = e2, n2
        if ref < e - 1e-9:
            gain = (e - ref) / math.sqrt(n)
            rec = store.get(n) or {"source": "grid"}
            rows.append({"n": n, "k": k, "u": round(u, 4), "excess": round(e, 6), "ref_excess": round(ref, 6),
                         "ref_n": src, "gain": gain, "family": family(rec["source"]),
                         "ref_family": family((store.get(src) or {"source": "grid"})["source"])})
    rows.sort(key=lambda r: -r["gain"])
    fam_score, fam_count = defaultdict(float), defaultdict(int)
    for n in range(1, MAX_N + 1):
        f = family((store.get(n) or {"source": "grid"})["source"])
        fam_score[f] += L[n] / math.sqrt(n) - 1
        fam_count[f] += 1
    bands = defaultdict(float)
    for r in rows:
        bands["1-100" if r["n"] <= 100 else "101-324" if r["n"] <= 324 else "325-1000"] += r["gain"]
    return {
        "total": store.total(),
        "potential": sum(r["gain"] for r in rows),
        "potential_by_band": dict(bands),
        "families": {f: {"puzzles": fam_count[f], "score": round(fam_score[f], 4)} for f in fam_count},
        "weak": rows,
    }


def main(root: str) -> None:
    out = analyse(Store(root))
    (Path(root) / "weak.json").write_text(json.dumps(out))
    print(f"total {out['total']:.6f}; potential {out['potential']:.4f} over {len(out['weak'])} puzzles")
    print("by band:", {k: round(v, 4) for k, v in out["potential_by_band"].items()})
    print("families:", out["families"])
    for r in out["weak"][:25]:
        print(f"n={r['n']:4d} k={r['k']:2d} u={r['u']:.3f} excess {r['excess']:.4f} vs {r['ref_excess']:.4f} "
              f"(n={r['ref_n']}, {r['ref_family']})  gain {r['gain']:.4f}  ours: {r['family']}")


if __name__ == "__main__":
    main(sys.argv[1])
