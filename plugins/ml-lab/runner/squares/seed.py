"""Seed the store: grid for every n, then each published packing (B1).

A packing of N squares also packs every n < N in the same container, so for
each n the smallest published side over all N >= n is tried, dropping squares.

    python -m squares.seed /data /data/known/parsed
"""

from __future__ import annotations

import math
import sys
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

from .metric import MAX_N
from .packing import Packing, Store, drop_to, from_parsed, grid


def _work(args) -> tuple[int, bool, str, str]:
    root, n, path = args
    store = Store(root)
    if path is None:
        p = grid(n)
    else:
        p = drop_to(from_parsed(Path(path)), n)
    ok, why = store.offer(p, "seed")
    return n, ok, p.source, why


def main(root: str, parsed_dir: str, workers: int = 12) -> None:
    store = Store(Path(root))
    known: list[tuple[int, float, str]] = []  # (N, side, path)
    for f in sorted(Path(parsed_dir).glob("*.txt")):
        text = f.read_text()
        if not text.startswith("Final s:"):
            continue
        s = float(text.split("\n", 1)[0].split(":", 1)[1])
        N = text.count("Square ")
        known.append((N, s, str(f)))

    jobs = []
    for n in range(1, MAX_N + 1):
        if store.get(n) is None:
            jobs.append((root, n, None))
        fits = [(s, p) for N, s, p in known if N >= n and s < store.L(n) - 1e-12]
        if fits:
            s, p = min(fits)
            if s < math.ceil(math.sqrt(n)):
                jobs.append((root, n, p))

    grids = [j for j in jobs if j[2] is None]
    rest = [j for j in jobs if j[2] is not None]
    with ProcessPoolExecutor(workers) as pool:
        for batch in (grids, rest):  # grids first, so every n has a file
            for n, ok, src, why in pool.map(_work, batch, chunksize=4):
                if not ok and src != "grid":
                    print(f"n={n}: {src}: {why}", flush=True)
    print(f"seeded; local total {store.total():.6f}")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 12)
