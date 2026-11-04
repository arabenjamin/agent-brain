"""Blend public packings into the store (owner's decision, 2026-11-04).

Downloads each public notebook's output and reads only its CSV rows; nothing
from a notebook runs. For each puzzle the donors are tried best first through
`Store.offer`, so a packing enters only if Kaggle's metric accepts it as doubles
and it beats the stored L. Each stored packing names its source.

    python -m squares.blend /data owner/slug [owner/slug ...]
"""

from __future__ import annotations

import csv
import json
import subprocess
import sys
import time
import zipfile
from collections import defaultdict
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

from . import metric
from .packing import Packing, Store

MAX_CSV_BYTES = 400_000_000
TRIES = 3  # donors tried per puzzle before giving up on it
COLUMNS = {"n", "square_id", "x", "y", "angle"}


def fetch(root: Path, ref: str) -> Path:
    d = root / "public" / (ref.replace("/", "_") + time.strftime("-%Y%m%d"))
    d.mkdir(parents=True, exist_ok=True)
    if not any(d.iterdir()):
        p = subprocess.run(["kaggle", "kernels", "output", ref, "-p", str(d)],
                           capture_output=True, timeout=1800)
        if p.returncode:
            print(f"{ref}: download failed: {p.stderr.decode('utf-8', 'replace')[-200:]}", flush=True)
    for z in list(d.rglob("*.zip")):
        try:
            with zipfile.ZipFile(z) as zf:
                for m in zf.namelist():
                    if m.endswith(".csv") and ".." not in m and not m.startswith("/"):
                        zf.extract(m, z.parent)
        except zipfile.BadZipFile:
            pass
    return d


def read_csv(path: Path) -> dict[int, list[tuple[str, str, str]]]:
    """Puzzle -> squares, or nothing if this CSV is not a submission."""
    if path.stat().st_size > MAX_CSV_BYTES:
        return {}
    csv.field_size_limit(1_000_000)
    out: dict[int, dict[int, tuple[str, str, str]]] = defaultdict(dict)
    with open(path, newline="", errors="replace") as f:
        r = csv.DictReader(f)
        if not r.fieldnames or not COLUMNS <= set(r.fieldnames):
            return {}
        for row in r:
            try:
                out[int(row["n"])][int(row["square_id"])] = (row["x"], row["y"], row["angle"])
            except (TypeError, ValueError):
                return {}
    return {n: [s[i] for i in sorted(s)] for n, s in out.items()
            if 1 <= n <= metric.MAX_N and len(s) == n}


def rough_L(squares) -> float:
    """The bounding side in doubles, to rank donors; `offer` is the real check."""
    xs, ys = [], []
    for x, y, a in squares:
        for cx, cy in metric.compute_corners_float(float(x), float(y), float(a)):
            xs.append(cx)
            ys.append(cy)
    return max(max(xs) - min(xs), max(ys) - min(ys))


def try_puzzle(job) -> dict:
    root, n, cands = job
    store = Store(Path(root))
    before = store.L(n)
    res = {"n": n, "gain": 0.0, "source": None, "rejected": 0}
    for ref, squares in cands[:TRIES]:
        ok, why = store.offer(Packing(n, squares, f"public: {ref}"), "blend 2026-11-04")
        if ok:
            res["gain"] = (before - store.L(n)) / n ** 0.5
            res["source"] = ref
            return res
        res["rejected"] += 1
    return res


def main(root: Path, refs: list[str], workers: int = 4) -> dict:
    store = Store(root)
    before_total = store.total()
    best: dict[int, list] = defaultdict(list)
    for ref in refs:
        d = fetch(root, ref)
        puzzles = 0
        for path in d.rglob("*.csv"):
            try:
                parsed = read_csv(path)
            except (OSError, UnicodeError, csv.Error):
                continue
            for n, squares in parsed.items():
                try:
                    L = rough_L(squares)
                except (TypeError, ValueError):
                    continue
                if L < store.L(n) - 1e-13:
                    best[n].append((L, ref, squares))
            puzzles = max(puzzles, len(parsed))
        print(f"{ref}: {puzzles} puzzles read; candidates so far on {len(best)} puzzles", flush=True)
    jobs = [(str(root), n, [(r, s) for _, r, s in sorted(c, key=lambda t: t[0])]) for n, c in sorted(best.items())]
    gained, by_source = 0, defaultdict(lambda: [0, 0.0])
    with ProcessPoolExecutor(workers) as ex:
        for res in ex.map(try_puzzle, jobs):
            if res["source"]:
                gained += 1
                by_source[res["source"]][0] += 1
                by_source[res["source"]][1] += res["gain"]
    out = {"before": before_total, "after": Store(root).total(), "puzzles_improved": gained,
           "puzzles_tried": len(jobs), "by_source": dict(by_source)}
    (root / "blend.json").write_text(json.dumps(out, indent=1))
    return out


if __name__ == "__main__":
    print(json.dumps(main(Path(sys.argv[1]), sys.argv[2:]), indent=1), flush=True)
