"""Where we stand on a Kaggle leaderboard: our best public score, rank, field
size, median and top, as Kaggle reports them.

Both status endpoints (Squares and Playground) carry it, because on
2026-10-26 neither report could say where we ranked: they held local scores
and the submit command's output only. The leaderboard is a download, so the
answer is cached in `cache` for MAX_AGE seconds.
"""

from __future__ import annotations

import csv
import io
import json
import statistics
import subprocess
import tempfile
import time
import zipfile
from pathlib import Path

MAX_AGE = 3 * 3600


def _kaggle(*args, timeout=120) -> subprocess.CompletedProcess:
    return subprocess.run(["kaggle", *args], capture_output=True, text=True, timeout=timeout)


def _username() -> str:
    for line in _kaggle("config", "view").stdout.splitlines():
        if line.strip().startswith("- username:"):
            return line.split(":", 1)[1].strip()
    return ""


def _leaderboard(slug: str) -> list[dict]:
    with tempfile.TemporaryDirectory() as tmp:
        p = _kaggle("competitions", "leaderboard", "-c", slug, "--download", "-p", tmp)
        zips = list(Path(tmp).glob("*.zip"))
        if not zips:
            raise RuntimeError((p.stdout + p.stderr).strip()[-300:] or "no leaderboard file")
        with zipfile.ZipFile(zips[0]) as zf:
            text = zf.read(zf.namelist()[0]).decode("utf-8-sig")
    return list(csv.DictReader(io.StringIO(text)))


def fetch(slug: str, lower_is_better: bool) -> dict:
    user = _username()
    rows = _leaderboard(slug)
    scores = [float(r["Score"]) for r in rows if r.get("Score")]
    ours = next((r for r in rows if user and user in r.get("TeamMemberUserNames", "").split(",")), None)
    out = {"at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "teams": len(rows),
           "top": rows[0]["Score"] if rows else None,
           "median": statistics.median(scores) if scores else None,
           "lower_is_better": lower_is_better}
    if ours is None:
        out["rank"] = None
        out["why"] = f"no leaderboard row for {user or 'our username'} (not joined, or nothing scored yet)"
    else:
        out |= {"rank": int(ours["Rank"]), "score": ours["Score"], "last_submission": ours["LastSubmissionDate"]}
    return out


def standing(slug: str, cache: Path, lower_is_better: bool = False) -> dict:
    """The cached standing, refreshed when older than MAX_AGE. A failed
    refresh keeps the old answer and says why."""
    old = None
    try:
        old = json.loads(cache.read_text())
        if time.time() - old.get("fetched", 0) < MAX_AGE:
            return old
    except (OSError, ValueError):
        pass
    try:
        new = fetch(slug, lower_is_better) | {"fetched": time.time()}
    except Exception as e:  # noqa: BLE001 - a status page must still answer
        why = f"{type(e).__name__}: {str(e)[:200]}"
        return (old or {}) | {"refresh_failed": why}
    tmp = cache.with_suffix(".tmp")
    tmp.write_text(json.dumps(new))
    tmp.replace(cache)
    return new
