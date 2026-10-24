"""The overnight search (ML_LAB_PLAN.md B3): pick puzzles, search, keep what
the metric accepts.

Two strategies, both in `sa.py`:
- squeeze: shrink the stored packing (for the n whose packing is a larger
  one with squares removed, so there is a hole to use);
- restart: random starts in a container just below the stored side (for
  n <= RESTART_MAX_N, where that search has found records before).

Every attempt is logged to `attempts.jsonl` with what it tried and found, so
a week of nights says whether the CPU is buying anything.
"""

from __future__ import annotations

import json
import math
import os
import time
from concurrent.futures import ProcessPoolExecutor, as_completed
from pathlib import Path

from . import sa
from .metric import MAX_N
from .packing import Store, from_floats

RESTART_MAX_N = int(os.environ.get("ML_RESTART_MAX_N", "120"))
RESTART_SHRINK = 1e-4  # a restart aims this far (relative) below the stored side


def _attempts(root: Path) -> dict[int, int]:
    counts: dict[int, int] = {}
    try:
        with open(root / "attempts.jsonl") as f:
            for line in f:
                n = json.loads(line)["n"]
                counts[n] = counts.get(n, 0) + 1
    except OSError:
        pass
    return counts


def targets(store: Store, k: int) -> list[tuple[int, str]]:
    """The k most promising (n, strategy): a large score, few attempts so
    far, and cheap to search (cost grows with n)."""
    tried = _attempts(store.root)
    try:  # proven optimal (or trivial) on the record page: nothing to find
        proven = set(json.loads((store.root / "proven.json").read_text())["n"])
    except (OSError, ValueError, KeyError):
        proven = set()
    scored = []
    for n in range(2, MAX_N + 1):
        r = math.isqrt(n)
        if r * r == n or n in proven:
            continue  # the grid already scores 0, or the record is proven
        rec = store.get(n) or {}
        score = store.L(n) / math.sqrt(n) - 1
        holey = " minus " in rec.get("source", "")
        if n <= RESTART_MAX_N:
            strategy = "restart"
        elif holey:
            strategy = "squeeze"
        else:
            continue
        pri = score / (1 + tried.get(n, 0)) / max(1.0, n / 100)
        scored.append((pri, n, strategy))
    scored.sort(reverse=True)
    return [(n, s) for _, n, s in scored[:k]]


def _job(args):
    root, n, strategy, seconds, seed = args
    store = Store(root)
    L = store.L(n)
    p = store.packing(n)
    t0 = time.time()
    if strategy == "squeeze":
        f = p.floats()
        r = sa.squeeze([q[0] for q in f], [q[1] for q in f], [q[2] for q in f], seconds, seed)
        found = None if r is None else (r[0], r[1], r[2], r[3])
    else:
        Lt = L * (1 - RESTART_SHRINK)
        r = sa.restart(n, Lt, seconds, seed)
        found = None if r is None else (r[0], r[1], r[2], Lt)
    out = {"n": n, "strategy": strategy, "seconds": round(time.time() - t0), "from_L": L}
    if found is None:
        return out, None
    x, y, a, Lf = found
    x, y, a = sa.repair(x, y, a, Lf)
    return out, (list(x), list(y), list(a))


def run(root: Path, workers: int, seconds: int, until: float, log=print) -> dict:
    """Run jobs on `workers` processes until the time `until`."""
    store = Store(root)
    done = improved = 0
    seed = int(time.time())
    with ProcessPoolExecutor(workers) as pool:
        pending = set()

        def submit_more():
            nonlocal seed
            busy = {getattr(f, "n", None) for f in pending}
            for n, strategy in targets(store, workers * 2):
                if len(pending) >= workers:
                    break
                if n in busy or time.time() + seconds > until:
                    continue
                seed += 1
                fut = pool.submit(_job, (str(root), n, strategy, seconds, seed))
                fut.n = n
                pending.add(fut)
                busy.add(n)

        submit_more()
        while pending:
            for fut in as_completed(pending):
                pending.discard(fut)
                out, sol = fut.result()
                done += 1
                if sol is not None:
                    x, y, a = sol
                    ok, why = store.offer(from_floats(out["n"], x, y, a, f"ml-lab {out['strategy']}"), "optimize")
                    out["result"] = why
                    if ok:
                        improved += 1
                        out["to_L"] = store.L(out["n"])
                        log(f"n={out['n']}: improved {out['from_L']:.9f} -> {out['to_L']:.9f}")
                else:
                    out["result"] = "nothing found"
                out["at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
                with open(Path(root) / "attempts.jsonl", "a") as f:
                    f.write(json.dumps(out) + "\n")
                submit_more()
                break
    return {"jobs": done, "improved": improved}
