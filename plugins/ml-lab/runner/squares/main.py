"""The ML Lab runner's loop and status endpoint (ML_LAB_PLAN.md Track B).

Inside the overnight window (ML_RUNNER_HOURS, local time, default 23-7) it
searches on ML_RUNNER_WORKERS processes (default 12, the owner's allowance).
When the window closes it builds the submission, scores it with Kaggle's
metric and submits it if it is better than the last one sent. Outside the
window it only serves GET /status (and /playground), for the brain's report
workflows. Each window starts with the Playground competitions in
ML_PLAYGROUND (playground/run.py), then the squares search.
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

from playground import run as playground

from . import build as builder
from . import band, composite, fault, gobel, optimize, seed, submit, tilted
from .metric import MAX_N
from .packing import Store

ROOT = Path(os.environ.get("ML_DATA_DIR", "/data"))
WORKERS = int(os.environ.get("ML_RUNNER_WORKERS", "12"))
JOB_SECONDS = int(os.environ.get("ML_JOB_SECONDS", "600"))
HOURS = os.environ.get("ML_RUNNER_HOURS", "23-7")
# Playground competitions (comma-separated slugs) run first in each window.
PLAYGROUND = [s for s in os.environ.get("ML_PLAYGROUND", "").split(",") if s.strip()]


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
        elif self.path == "/playground":
            body = json.dumps(playground.status()).encode()
        elif self.path == "/playground/sources":
            body = json.dumps({s: playground.sources(s) for s in PLAYGROUND}).encode()
        elif self.path == "/playground/menu":
            body = json.dumps(playground.menu()).encode()
        elif self.path.startswith("/playground/notebook?"):
            from urllib.parse import parse_qs, urlsplit
            ref = (parse_qs(urlsplit(self.path).query).get("ref") or [""])[0]
            text = playground.notebook(ref) if ref != "none" else ""
            self._send(200, text.encode(), "text/plain; charset=utf-8")
            return
        else:
            self.send_error(404)
            return
        self._send(200, body)

    def do_POST(self):
        """POST /playground/rung {"slug": ..., "spec": <rung JSON or object>}:
        a proposal from the brain's playground-study. Only data from the
        menu is accepted (playground.validate); it runs in the next window."""
        if self.path != "/playground/rung":
            self.send_error(404)
            return
        size = int(self.headers.get("Content-Length") or 0)
        if size > 8192:
            self._send(413, b'{"queued":false,"why":"body over 8 KB"}')
            return
        try:
            req = json.loads(self.rfile.read(size) or b"{}")
        except ValueError:
            req = None
        if not isinstance(req, dict):
            self._send(400, b'{"queued":false,"why":"body is not a JSON object"}')
            return
        slug = req.get("slug") or (PLAYGROUND[0] if PLAYGROUND else "")
        spec = req.get("spec")
        if spec is None or (isinstance(spec, str) and spec.strip().lower() in ("", "none")):
            self._send(200, json.dumps({"queued": False, "why": "nothing proposed", "slug": slug}).encode())
            return  # a study that proposed nothing is not an error
        ok, why = playground.queue(slug, spec)
        log(f"playground rung for {slug}: {why}")
        self._send(200 if ok else 400, json.dumps({"queued": ok, "why": why, "slug": slug}).encode())

    def _send(self, code: int, body: bytes, ctype: str = "application/json"):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def log(msg: str) -> None:
    print(time.strftime("%Y-%m-%d %H:%M:%S"), msg, flush=True)


def morning() -> None:
    # The constructions build on the store, so a night's improvement can
    # open a composite (a small packing plus a grid) or a fault-line gadget
    # (a crop of it) that was not there.
    for family in (tilted, band, composite, fault):
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
        for family in (tilted, band, composite, composite, fault, fault):
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
        for slug in PLAYGROUND:
            try:
                playground.run(slug.strip())
            except Exception as e:  # a missing join or a bad file must not stop the search
                log(f"playground {slug}: {type(e).__name__}: {str(e)[:300]}")
        t0 = time.time()
        before = store.total()
        try:
            res = optimize.run(ROOT, WORKERS, JOB_SECONDS, stop.timestamp(), log)
        except Exception as e:  # the morning build and submit still run
            log(f"search failed: {type(e).__name__}: {str(e)[:300]}")
            res = {"error": f"{type(e).__name__}: {str(e)[:300]}"}
        night = res | {"started": datetime.fromtimestamp(t0).isoformat(timespec="minutes"),
                       "score_before": before, "score_after": store.total()}
        (ROOT / "night.json").write_text(json.dumps(night))
        log(f"window closed: {night}")
        morning()
        while window_end(datetime.now()) is not None:
            time.sleep(60)


if __name__ == "__main__":
    main()
