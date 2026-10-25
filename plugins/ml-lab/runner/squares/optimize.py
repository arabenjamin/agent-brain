"""The overnight search (ML_LAB_PLAN.md B3): pick puzzles, search, keep what
the metric accepts.

The default strategy (ML_SEARCH=lp) is `lp.polish`: edge-LP basin hopping
from the stored packing, for every puzzle not proven or a perfect square.
Puzzles where some public notebook's output is smaller (public/better.json)
go first: a smaller packing is known to exist there. The annealer in `sa.py`
(ML_SEARCH=anneal: squeeze holey packings, random restarts for
n <= RESTART_MAX_N) improved none of 569 jobs on 2026-10-23 and stays only as
the measured baseline.

Every attempt is logged to `attempts.jsonl` with what it tried and found, so
a week of nights says whether the CPU is buying anything.
"""

from __future__ import annotations

import json
import math
import os
import time
from concurrent.futures import ProcessPoolExecutor, as_completed
from concurrent.futures.process import BrokenProcessPool
from pathlib import Path

from . import lp, sa
from .metric import MAX_N
from .packing import Store, from_floats

RESTART_MAX_N = int(os.environ.get("ML_RESTART_MAX_N", "120"))
RESTART_SHRINK = 1e-4  # a restart aims this far (relative) below the stored side
SEARCH = os.environ.get("ML_SEARCH", "lp")


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
    try:  # n -> the smallest side among downloaded public outputs
        public = {int(k): v[0] for k, v in json.loads((store.root / "public" / "better.json").read_text()).items()}
    except (OSError, ValueError):
        public = {}
    scored = []
    for n in range(2, MAX_N + 1):
        r = math.isqrt(n)
        if r * r == n or n in proven:
            continue  # the grid already scores 0, or the record is proven
        rec = store.get(n) or {}
        score = store.L(n) / math.sqrt(n) - 1
        holey = " minus " in rec.get("source", "")
        if SEARCH == "lp":
            gap = max(0.0, (store.L(n) - public.get(n, math.inf)) / math.sqrt(n))
            pri = (score + 100 * gap) / (1 + tried.get(n, 0)) / max(1.0, n / 200)
            scored.append((pri, n, "polish"))
            continue
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
    import faulthandler
    faulthandler.enable()  # a native crash prints where it happened
    root, n, strategy, seconds, seed = args
    store = Store(root)
    L = store.L(n)
    p = store.packing(n)
    t0 = time.time()
    if strategy == "polish":
        f = p.floats()
        r = lp.polish([q[0] for q in f], [q[1] for q in f], [q[2] for q in f], seconds, seed)
        out = {"n": n, "strategy": strategy, "seconds": round(time.time() - t0), "from_L": L}
        return out, None if r is None else (list(r[0]), list(r[1]), list(r[2]))
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


def _record(root, out: dict) -> None:
    out["at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    with open(Path(root) / "attempts.jsonl", "a") as f:
        f.write(json.dumps(out) + "\n")


def run(root: Path, workers: int, seconds: int, until: float, log=print) -> dict:
    """Run jobs on `workers` processes until the time `until`.

    A worker that dies (a signal, not an exception: on 2026-10-24/25 one did
    about hourly and took the whole runner down) breaks the pool. The jobs it
    held are recorded as crashed, so `targets` moves on from them, and a new
    pool carries on; each worker's faulthandler prints the native traceback."""
    store = Store(root)
    done = improved = crashed = 0
    seed = int(time.time())
    while time.time() + seconds <= until:
        pending = set()
        try:
            with ProcessPoolExecutor(workers) as pool:

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
                        fut.n, fut.strategy = n, strategy
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
                        _record(root, out)
                        submit_more()
                        break
            break  # the window is over
        except BrokenProcessPool:
            lost = sorted(getattr(f, "n", -1) for f in pending)
            crashed += 1
            log(f"a worker died; the pool held n={lost}; starting a new pool")
            for f in pending:
                _record(root, {"n": f.n, "strategy": f.strategy, "seconds": 0, "from_L": store.L(f.n),
                               "result": "worker crashed (this job or another in the pool)"})
    return {"jobs": done, "improved": improved, "pool_crashes": crashed}
