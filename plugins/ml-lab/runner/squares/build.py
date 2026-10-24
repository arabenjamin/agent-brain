"""Build the submission from the store and score that exact file with
Kaggle's metric, the way Kaggle will (ML_LAB_PLAN.md B1).

    python -m squares.build /data
"""

from __future__ import annotations

import csv
import io
import json
import sys
import time
import zipfile
from pathlib import Path

from . import metric
from .packing import Store


def build(root: Path, workers: int = 12) -> dict:
    root = Path(root)
    store = Store(root)
    buf = io.StringIO()
    w = csv.writer(buf, lineterminator="\n")
    w.writerow(["id", "n", "square_id", "x", "y", "angle"])
    row = 0
    for n in range(1, metric.MAX_N + 1):
        for i, (x, y, a) in enumerate(store.packing(n).squares):
            w.writerow([row, n, i, x, y, a])
            row += 1
    text = buf.getvalue()

    # Score the text we are about to upload, not the store it came from.
    metric.KAGGLE_CORES = workers
    puzzles = metric._parse_submission_csv(text)
    total, results = metric._evaluate_puzzles_parallel(puzzles, set(range(1, metric.MAX_N + 1)))
    bad = [{"n": n, "reason": why} for n, _, _, why in results if why != "ok"]

    out = root / "submission.zip"
    with zipfile.ZipFile(out.with_suffix(".tmp"), "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("submission.csv", text)
    out.with_suffix(".tmp").replace(out)
    info = {"built_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "score": total,
            "rows": row, "rejected": bad, "file": str(out)}
    (root / "build.json").write_text(json.dumps(info, indent=1))
    return info


if __name__ == "__main__":
    info = build(Path(sys.argv[1]))
    print(json.dumps({k: v for k, v in info.items() if k != "rejected"}), f"rejected={len(info['rejected'])}")
