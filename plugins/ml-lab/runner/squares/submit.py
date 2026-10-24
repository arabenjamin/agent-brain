"""Upload the built submission to Kaggle, only when it beats the last one.

Needs KAGGLE_API_TOKEN and the owner having joined the competition on the
website (accepting its rules cannot be done through the API). Kaggle allows
10 a day; this sends at most ML_SUBMIT_PER_DAY (default 1). Every attempt is
recorded in `submissions.jsonl`, and a refusal is recorded, not retried.

    python -m squares.submit /data [--force]
"""

from __future__ import annotations

import csv
import io
import json
import os
import subprocess
import sys
import time
from pathlib import Path

COMPETITION = "squares-in-squares"


def history(root: Path) -> list[dict]:
    try:
        return [json.loads(line) for line in open(root / "submissions.jsonl")]
    except OSError:
        return []


def kaggle_scores() -> dict[str, float]:
    """Our submissions' public scores as Kaggle reports them, by description."""
    p = subprocess.run(["kaggle", "competitions", "submissions", "-c", COMPETITION, "--csv"],
                       capture_output=True, text=True, timeout=120)
    out = {}
    for row in csv.DictReader(io.StringIO(p.stdout)):
        try:
            out[row["description"]] = float(row["publicScore"])
        except (KeyError, ValueError):
            pass
    return out


def submit(root: Path, force: bool = False) -> dict:
    root = Path(root)
    build = json.loads((root / "build.json").read_text())
    past = history(root)
    sent = [h for h in past if h.get("status") == "sent"]
    # Kaggle's score of what we sent is the one to beat, not our local one:
    # the first upload was 23.92 here and 28.94 there (the doubles lesson).
    if sent:
        try:
            ks = kaggle_scores()
        except Exception:  # noqa: BLE001 - no listing: fall back to local scores
            ks = {}
        for h in sent:
            if h.get("message") in ks:
                h["score"] = ks[h["message"]]
    today = time.strftime("%Y-%m-%d", time.gmtime())
    rec = {"at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "score": build["score"]}

    if os.environ.get("ML_SUBMIT", "on") != "on" and not force:
        rec["status"] = "skipped: ML_SUBMIT is off"
    elif not (os.environ.get("KAGGLE_API_TOKEN") or os.environ.get("KAGGLE_KEY")):
        rec["status"] = "skipped: no KAGGLE_API_TOKEN or KAGGLE_KEY"
    elif build["rejected"]:
        rec["status"] = f"skipped: the metric rejected {len(build['rejected'])} puzzles"
    elif sent and build["score"] >= min(h["score"] for h in sent) - 1e-12 and not force:
        rec["status"] = "skipped: no better than the best sent"
    elif sum(h["at"].startswith(today) for h in sent) >= int(os.environ.get("ML_SUBMIT_PER_DAY", "1")):
        rec["status"] = "skipped: today's submissions used"
    else:
        msg = f"ml-lab {rec['at']} local {build['score']:.9f}"
        rec["message"] = msg
        env = dict(os.environ)
        if env.get("KAGGLE_KEY") and env.get("KAGGLE_USERNAME"):
            env.pop("KAGGLE_API_TOKEN", None)  # the legacy key wins when both are set
        p = subprocess.run(
            ["kaggle", "competitions", "submit", "-c", COMPETITION, "-f", build["file"], "-m", msg],
            capture_output=True, text=True, timeout=900, env=env)
        # The upload's progress bar fills the output: judge the whole text,
        # and keep only the lines that are not the bar.
        text = (p.stdout + "\n" + p.stderr).replace("\r", "\n")
        lines = [l for l in text.splitlines() if l.strip() and "%|" not in l]
        rec["status"] = "sent" if p.returncode == 0 and "Successfully" in text else "refused"
        rec["kaggle"] = "\n".join(lines)[-400:]
    with open(root / "submissions.jsonl", "a") as f:
        f.write(json.dumps(rec) + "\n")
    return rec


def leaderboard_score() -> str:
    """Our latest public score as Kaggle reports it (or the error)."""
    p = subprocess.run(["kaggle", "competitions", "submissions", "-c", COMPETITION, "--csv"],
                       capture_output=True, text=True, timeout=120)
    return (p.stdout or p.stderr).strip()[:2000]


if __name__ == "__main__":
    print(json.dumps(submit(Path(sys.argv[1]), "--force" in sys.argv)))
