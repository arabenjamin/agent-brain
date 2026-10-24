"""Import published record packings in the register text format (B1).

Francisco Couzo's register (github.com/franciscouzo/square-packing, MIT)
keeps record packings as text: `# n = N`, `# s = side`, then `x y theta`
lines (radians). Also read: `.cert` files of exact rationals (centre x, y and t = tan(θ/2); Couzo's,
evand's and TheSnakeFang's) and Ellsworth-parser text (Mishapolk's records). Only MIT
repositories are imported. The 2026-10-24 forum's strongest notebooks start from it.
Every file under the given directories is read; as in the seed, a packing of
N squares is offered for every n <= N it beats, and the metric decides.

    python -m squares.register /data /data/external/<owner>__<repo> ...
"""

from __future__ import annotations

import re
import sys
from fractions import Fraction
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

from .metric import MAX_N
import mpmath

from .packing import DPS, Packing, Store, drop_to, fmt, from_parsed

_HEAD = re.compile(r"#\s*n\s*=\s*(\d+)\s*\n#\s*s\s*=\s*([\d.eE+-]+)")


def _repo(path: Path) -> str:
    parts = path.parts
    return parts[parts.index("external") + 1].replace("__", "/") if "external" in parts else "?"


def header(path: Path) -> tuple[int, float] | None:
    """(n, side) from a file's first lines, in any of the three formats."""
    head = path.read_text(errors="replace")[:600]
    m = _HEAD.search(head)
    if m:
        return int(m.group(1)), float(m.group(2))
    if path.suffix == ".cert":  # exact rationals: a comment, then "n side"
        lines = [l for l in head.splitlines() if l.strip() and not l.startswith("#")]
        if len(lines[0].split()) != 2:
            return None  # `n W H`: a rectangle certificate, not a square
        try:
            n, side = lines[0].split()[:2]
            n, side = int(n), float(Fraction(side))
        except (IndexError, ValueError, OverflowError, ZeroDivisionError):
            return None  # not the "n side" header this reader knows
        return (n, side) if 0 < side < 2 * n else None
    m = re.match(r"\s*(?:Final )?s:\s*([\d.eE+-]+)", head)
    if m and "Square " in head:  # Ellsworth's parser output
        return path.read_text(errors="replace").count("Square "), float(m.group(1))
    return None


def _decimal(r: str) -> str:
    """A rational p/q (or a decimal) as a 40-digit decimal string."""
    if "/" not in r:
        return r
    with mpmath.workdps(DPS):
        p, q = r.split("/")
        return fmt(mpmath.mpf(int(p)) / int(q))


def _angle(t: str) -> str:
    """theta = 2 atan(t), from a certificate's rational t."""
    with mpmath.workdps(DPS):
        p, _, q = t.partition("/")
        v = mpmath.mpf(int(p)) / int(q) if q else mpmath.mpf(p)
        return fmt(2 * mpmath.atan(v)) if v != 0 else "0"


def read(path: Path) -> Packing | None:
    if path.suffix == ".cert":
        lines = [l for l in path.read_text(errors="replace").splitlines() if l.strip() and not l.startswith("#")]
        n = int(lines[0].split()[0])
        # The third column is t = tan(theta/2), the rational rotation every
        # certificate repository uses (c, s = (1-t²)/(1+t²), 2t/(1+t²)).
        sq = [(_decimal(x), _decimal(y), _angle(t)) for x, y, t in (l.split()[:3] for l in lines[1:n + 1])]
        return Packing(len(sq), sq, f"{_repo(path)} (MIT): {path.name}") if len(sq) == n else None
    text = path.read_text(errors="replace")
    if "Square " in text[:600] and not _HEAD.search(text[:400]):
        p = from_parsed(path)
        return Packing(p.n, p.squares, f"{_repo(path)} (MIT): {path.name}")
    m = _HEAD.search(text[:400])
    if not m:
        return None
    sq = []
    for line in text.splitlines():
        if line.startswith("#") or not line.strip():
            continue
        parts = line.split()
        if len(parts) >= 3:
            sq.append((parts[0], parts[1], parts[2]))
    if len(sq) != int(m.group(1)):
        return None
    return Packing(len(sq), sq, f"{_repo(path)} (MIT): {path.name}")


def _offer(args):
    root, n, path = args
    p = read(Path(path))
    return (n, *Store(root).offer(drop_to(p, n), "register"))


def main(root: str, repos: list[str], workers: int = 12) -> None:
    store = Store(root)
    known = []  # (side, N, path)
    for f in (f for repo in repos for g in ("*.txt", "*.cert") for f in Path(repo).rglob(g)):
        h = header(f)
        if h:
            known.append((h[1], h[0], str(f)))
    known.sort()
    jobs = []
    for n in range(1, MAX_N + 1):
        fits = [(s, p) for s, N, p in known if N >= n and s < store.L(n) - 1e-9]
        if fits:
            jobs.append((root, n, min(fits)[1]))
    stored = 0
    with ProcessPoolExecutor(workers) as pool:
        for n, ok, why in pool.map(_offer, jobs, chunksize=2):
            stored += ok
            if not ok:
                print(f"n={n}: {why}", flush=True)
    print(f"{len(known)} files; tried {len(jobs)}, stored {stored}; local total {store.total():.6f}")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2:])
