"""A packing of n unit squares, and the best-known store (ML_LAB_PLAN.md B1).

Coordinates are decimal strings (centre x, y; angle in radians), the way the
metric reads them, so no precision is lost between the optimizer, the store
and the submission. Every packing is checked by Kaggle's own metric
(`metric.evaluate_single_puzzle`) before it is stored: a packing the metric
would reject never reaches a submission.
"""

from __future__ import annotations

import json
import math
import os
import re
import time
from dataclasses import dataclass
from pathlib import Path

import mpmath

from . import metric

DPS = 50
DIGITS = 40  # significant digits written per coordinate
# Squares that touch exactly are certified as separated only when the decimals
# say so; 40-digit decimals of an irrational contact can overlap by 1e-40.
# Scaling the centres apart by GAP opens every contact by ~GAP and costs
# about GAP * L in the score.
GAP = mpmath.mpf("1e-30")


@dataclass
class Packing:
    n: int
    squares: list[tuple[str, str, str]]  # (x, y, angle) decimal strings
    source: str = ""

    def rows(self) -> list[dict]:
        return [
            {"n": str(self.n), "square_id": str(i), "x": x, "y": y, "angle": a}
            for i, (x, y, a) in enumerate(self.squares)
        ]

    def check(self) -> tuple[bool, float, str | None, str]:
        """(ok, score, L text, reason), exactly as Kaggle scores it."""
        _, score, L, reason = metric.evaluate_single_puzzle(self.n, self.rows())
        return reason == "ok", score, L, reason

    def floats(self) -> list[tuple[float, float, float]]:
        return [(float(x), float(y), float(a)) for x, y, a in self.squares]


def fmt(v) -> str:
    return mpmath.nstr(mpmath.mpf(v), DIGITS, min_fixed=-math.inf, max_fixed=math.inf)


def grid(n: int) -> Packing:
    k = math.ceil(math.sqrt(n))
    sq = [(f"{i % k}.5", f"{i // k}.5", "0") for i in range(n)]
    return Packing(n, sq, "grid")


def with_gap(p: Packing, gap=GAP) -> Packing:
    """Scale the centres away from their centroid by (1 + gap)."""
    with mpmath.workdps(DPS):
        pts = [(mpmath.mpf(x), mpmath.mpf(y), a) for x, y, a in p.squares]
        cx = sum(x for x, _, _ in pts) / len(pts)
        cy = sum(y for _, y, _ in pts) / len(pts)
        f = 1 + mpmath.mpf(gap)
        sq = [(fmt(cx + (x - cx) * f), fmt(cy + (y - cy) * f), a) for x, y, a in pts]
    return Packing(p.n, sq, p.source)


def drop_to(p: Packing, n: int) -> Packing:
    """The same packing with only n of its squares: the n nearest one corner,
    so the removed ones are on the far side and may let L shrink."""
    if n >= p.n:
        return p
    keyed = sorted(p.squares, key=lambda s: float(s[0]) + float(s[1]))
    return Packing(n, keyed[:n], f"{p.source} minus {p.n - n}")


_LINE = re.compile(r"x=([-\d.eE+]+),\s*y=([-\d.eE+]+),\s*deg=([-\d.eE+]+)")


def from_parsed(path: Path) -> Packing:
    """A packing from parse_svg_packing.py's text output (angles in degrees)."""
    sq = []
    with mpmath.workdps(DPS):
        for m in _LINE.finditer(path.read_text()):
            rad = mpmath.mpf(m.group(3)) * mpmath.pi / 180
            sq.append((m.group(1), m.group(2), fmt(rad) if rad != 0 else "0"))
    return Packing(len(sq), sq, f"Ellsworth table: {path.stem}")


def from_floats(n: int, xs, ys, angs, source: str) -> Packing:
    return Packing(n, [(repr(float(x)), repr(float(y)), repr(float(a))) for x, y, a in zip(xs, ys, angs)], source)


class Store:
    """One file per n under `root/best/`, the best packing that passed the metric."""

    def __init__(self, root: Path):
        self.root = Path(root)
        (self.root / "best").mkdir(parents=True, exist_ok=True)

    def path(self, n: int) -> Path:
        return self.root / "best" / f"{n}.json"

    def get(self, n: int) -> dict | None:
        try:
            return json.loads(self.path(n).read_text())
        except (OSError, ValueError):
            return None

    def packing(self, n: int) -> Packing:
        rec = self.get(n)
        if rec is None:
            return grid(n)
        return Packing(n, [tuple(s) for s in rec["squares"]], rec["source"])

    def L(self, n: int) -> float:
        rec = self.get(n)
        return float(rec["L"]) if rec else float(math.ceil(math.sqrt(n)))

    def offer(self, p: Packing, note: str = "") -> tuple[bool, str]:
        """Store p if the metric accepts it and it beats the stored L.

        Tries the packing as given, then with the contact gap opened."""
        cur = self.get(p.n)
        cur_L = float(cur["L"]) if cur else math.inf
        for cand in (p, with_gap(p), with_gap(p, mpmath.mpf("1e-20"))):
            ok, score, L, reason = cand.check()
            if ok:
                break
        if not ok:
            self.log({"n": p.n, "event": "rejected", "source": p.source, "reason": reason})
            return False, reason
        if float(L) >= cur_L:
            return False, f"L {float(L):.12f} not below stored {cur_L:.12f}"
        rec = {"n": p.n, "L": L, "score": score, "source": cand.source, "note": note,
               "found_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               "squares": [list(s) for s in cand.squares]}
        tmp = self.path(p.n).with_suffix(".tmp")
        tmp.write_text(json.dumps(rec))
        os.replace(tmp, self.path(p.n))
        self.log({"n": p.n, "event": "improved", "L": L, "was": None if cur is None else cur["L"],
                  "source": cand.source, "note": note})
        return True, "stored"

    def log(self, entry: dict) -> None:
        entry["at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        with open(self.root / "log.jsonl", "a") as f:
            f.write(json.dumps(entry) + "\n")

    def total(self) -> float:
        return sum(self.L(n) / math.sqrt(n) - 1 for n in range(1, metric.MAX_N + 1))
