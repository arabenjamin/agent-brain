"""The ML Lab runner's loop and status endpoint (ML_LAB_PLAN.md Track B).

Inside the overnight window (ML_RUNNER_HOURS, local time, default 23-7) it
searches on ML_RUNNER_WORKERS processes (default 12, the owner's allowance).
When the window closes it builds the submission, scores it with Kaggle's
metric and submits it if it is better than the last one sent. Outside the
window it only serves GET /status, for the brain's squares-report workflow.
"""

from __future__ import annotations

import json
import math
import os
import threading
import time
from datetime import datetime, timedelta
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from . import build as builder
from . import band, composite, gobel, optimize, seed, submit, tilted
from .metric import MAX_N
from .packing import Store

ROOT = Path(os.environ.get("ML_DATA_DIR", "/data"))
WORKERS = int(os.environ.get("ML_RUNNER_WORKERS", "12"))
JOB_SECONDS = int(os.environ.get("ML_JOB_SECONDS", "600"))
HOURS = os.environ.get("ML_RUNNER_HOURS", "23-7")


def window_end(now: datetime) -> datetime | None:
    """The end of the window `now` is in, or None outside it."""
    start, end = (int(h) for h in HOURS.split("-"))
    h = now.hour
    inside = start <= h < end if start < end else (h >= start or h < end)
    if not inside:
        return None
    stop = now.replace(hour=end % 24, minute=0, second=0, microsecond=0)
    if stop <= now:
        stop += timedelta(days=1)
    return stop


def read_json(name: str):
    try:
        return json.loads((ROOT / name).read_text())
    except (OSError, ValueError):
        return None


def status() -> dict:
    store = Store(ROOT)
    bands = {"1-100": 0.0, "101-324": 0.0, "325-1000": 0.0}
    for n in range(1, MAX_N + 1):
        s = store.L(n) / math.sqrt(n) - 1
        bands["1-100" if n <= 100 else "101-324" if n <= 324 else "325-1000"] += s
    attempts = []
    try:
        attempts = [json.loads(line) for line in open(ROOT / "attempts.jsonl")]
    except OSError:
        pass
    week = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() - 7 * 86400))
    recent = [a for a in attempts if a.get("at", "") >= week]
    build = read_json("build.json") or {}
    return {
        "local_score": round(store.total(), 9),
        "bands": {k: round(v, 6) for k, v in bands.items()},
        "last_build": {k: build.get(k) for k in ("built_at", "score", "rows")} | {"rejected": len(build.get("rejected", []))},
        "submissions": submit.history(ROOT)[-5:],
        "last_night": read_json("night.json"),
        "search_7d": {
            "attempts": len(recent),
            "improved": [{"n": a["n"], "from_L": a["from_L"], "to_L": a.get("to_L"), "at": a["at"]}
                         for a in recent if "to_L" in a],
            "cpu_hours": round(sum(a.get("seconds", 0) for a in recent) / 3600, 1),
        },
        "window": HOURS, "workers": WORKERS,
        "now_running": window_end(datetime.now()) is not None,
    }


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            body = b'{"ok":true}'
        elif self.path == "/status":
            body = json.dumps(status()).encode()
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def log(msg: str) -> None:
    print(time.strftime("%Y-%m-%d %H:%M:%S"), msg, flush=True)


def morning() -> None:
    # The constructions build on the store, so a night's improvement can
    # open a composite (a small packing plus a grid) that was not there.
    for family in (tilted, band, composite):
        family.main(str(ROOT), WORKERS)
    info = builder.build(ROOT, WORKERS)
    log(f"built: score {info['score']:.9f}, rejected {len(info['rejected'])}")
    rec = submit.submit(ROOT)
    log(f"submit: {rec['status']}")


def main() -> None:
    threading.Thread(target=ThreadingHTTPServer(("0.0.0.0", 8000), Handler).serve_forever, daemon=True).start()
    store = Store(ROOT)
    if store.get(1) is None and (ROOT / "parsed").is_dir():
        log("empty store: seeding")
        seed.main(str(ROOT), str(ROOT / "parsed"), WORKERS)
        gobel.main(str(ROOT))
        for family in (tilted, band, composite, composite):
            family.main(str(ROOT), WORKERS)
    if read_json("build.json") is None:
        builder.build(ROOT, 4)
    log(f"ready: local score {store.total():.9f}, window {HOURS}, {WORKERS} workers")
    while True:
        stop = window_end(datetime.now())
        if stop is None:
            time.sleep(60)
            continue
        log(f"window open until {stop:%H:%M}")
        t0 = time.time()
        before = store.total()
        res = optimize.run(ROOT, WORKERS, JOB_SECONDS, stop.timestamp(), log)
        night = res | {"started": datetime.fromtimestamp(t0).isoformat(timespec="minutes"),
                       "score_before": before, "score_after": store.total()}
        (ROOT / "night.json").write_text(json.dumps(night))
        log(f"window closed: {night}")
        morning()
        while window_end(datetime.now()) is not None:
            time.sleep(60)


if __name__ == "__main__":
    main()
