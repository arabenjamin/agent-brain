"""One Playground competition: fetch, cross-validate a ladder, submit the best.

The Playground Series is Kaggle's monthly tabular competition, where Track A
meets a real target. Each rung of the ladder is one experiment, scored by
5-fold cross-validation with the competition's metric on the training set
(out-of-fold), so every rung says what it bought before Kaggle sees anything:

1. baseline: HistGradientBoosting, defaults (categoricals from dtype);
2. tuned: more trees at a lower learning rate, larger leaves, early stopping;
3. counts: plus frequency encodings of every categorical and low-cardinality
   numeric column (a standard Playground feature: synthetic data copies the
   original's value frequencies).

Every experiment goes to experiments.jsonl. The best by CV is written as
submission.csv and submitted when it beats the CV of the last one sent
(ML_SUBMIT=off stops it; one a day). Data lives in /playground/<slug>.

    python -m playground.run playground-series-s6e10 [--no-submit]
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import zipfile
from pathlib import Path

import numpy as np
import pandas as pd
from sklearn.ensemble import HistGradientBoostingClassifier, HistGradientBoostingRegressor
from sklearn.metrics import mean_squared_error, roc_auc_score
from sklearn.model_selection import KFold, StratifiedKFold
from threadpoolctl import threadpool_limits

ROOT = Path(os.environ.get("ML_PLAYGROUND_DIR", "/playground"))
THREADS = int(os.environ.get("ML_RUNNER_WORKERS", "12"))
FOLDS = 5
SEED = 42

# The metric each competition is scored on (read from its overview page).
METRICS = {"playground-series-s6e10": "auc"}


def log(msg: str) -> None:
    print(time.strftime("%Y-%m-%d %H:%M:%S"), msg, flush=True)


def fetch(slug: str) -> Path:
    d = ROOT / slug
    d.mkdir(parents=True, exist_ok=True)
    if not (d / "train.csv").exists():
        r = subprocess.run(["kaggle", "competitions", "download", slug, "-p", str(d)],
                           capture_output=True, text=True, timeout=900)
        zips = list(d.glob("*.zip"))
        if not zips:
            raise RuntimeError(f"download failed (joined the competition?): {(r.stdout + r.stderr)[-300:]}")
        for z in zips:
            with zipfile.ZipFile(z) as zf:
                zf.extractall(d)
            z.unlink()
    return d


def load(d: Path):
    train, test = pd.read_csv(d / "train.csv"), pd.read_csv(d / "test.csv")
    sample = pd.read_csv(d / "sample_submission.csv")
    idc, target = sample.columns[0], sample.columns[1]
    y = train[target]
    X = train.drop(columns=[idc, target])
    T = test.drop(columns=[idc])
    for c in X.columns:
        if not pd.api.types.is_numeric_dtype(X[c]) and not pd.api.types.is_bool_dtype(X[c]):
            cats = pd.CategoricalDtype(sorted(set(X[c].dropna()) | set(T[c].dropna())))
            X[c], T[c] = X[c].astype(cats), T[c].astype(cats)
    return X, y, T, test[idc], idc, target


def encode_target(y: pd.Series, metric: str):
    if metric == "auc" and not pd.api.types.is_numeric_dtype(y):
        pos = sorted(y.unique())[-1]
        return (y == pos).astype(int).values, pos
    return y.values, None


def counts(X: pd.DataFrame, T: pd.DataFrame):
    """Frequency of each value over train+test, for categoricals and numeric
    columns with few distinct values."""
    X, T = X.copy(), T.copy()
    both = pd.concat([X, T])
    for c in list(X.columns):
        if isinstance(X[c].dtype, pd.CategoricalDtype) or both[c].nunique() <= 200:
            freq = both[c].value_counts()
            X[f"{c}_count"] = X[c].map(freq).astype(float).values
            T[f"{c}_count"] = T[c].map(freq).astype(float).values
    return X, T


LADDER = [
    ("baseline", {}, False),
    ("tuned", {"learning_rate": 0.05, "max_iter": 2000, "max_leaf_nodes": 63, "min_samples_leaf": 40,
               "l2_regularization": 1.0, "early_stopping": True, "validation_fraction": 0.1,
               "n_iter_no_change": 50}, False),
    ("counts", {"learning_rate": 0.05, "max_iter": 2000, "max_leaf_nodes": 63, "min_samples_leaf": 40,
                "l2_regularization": 1.0, "early_stopping": True, "validation_fraction": 0.1,
                "n_iter_no_change": 50}, True),
]


def cv(X, y, T, metric, params):
    """Out-of-fold score and the fold-averaged test prediction."""
    clf = metric == "auc"
    split = (StratifiedKFold if clf else KFold)(FOLDS, shuffle=True, random_state=SEED)
    oof, pred = np.zeros(len(X)), np.zeros(len(T))
    for k, (tr, va) in enumerate(split.split(X, y)):
        Model = HistGradientBoostingClassifier if clf else HistGradientBoostingRegressor
        m = Model(categorical_features="from_dtype", random_state=SEED + k, **params)
        m.fit(X.iloc[tr], y[tr])
        if clf:
            oof[va] = m.predict_proba(X.iloc[va])[:, 1]
            pred += m.predict_proba(T)[:, 1] / FOLDS
        else:
            oof[va] = m.predict(X.iloc[va])
            pred += m.predict(T) / FOLDS
    score = roc_auc_score(y, oof) if clf else -mean_squared_error(y, oof) ** 0.5
    return float(score), pred


def history(d: Path, name: str) -> list[dict]:
    try:
        return [json.loads(line) for line in open(d / name)]
    except OSError:
        return []


def submit(slug: str, d: Path, rec: dict) -> dict:
    sent = [s for s in history(d, "submissions.jsonl") if s.get("status") == "sent"]
    today = time.strftime("%Y-%m-%d", time.gmtime())
    out = {"at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "experiment": rec["name"], "cv": rec["cv"]}
    if os.environ.get("ML_SUBMIT", "on") == "off":
        out["status"] = "skipped: ML_SUBMIT=off"
    elif sent and rec["cv"] <= max(s["cv"] for s in sent):
        out["status"] = "skipped: CV no better than the best sent"
    elif any(s["at"].startswith(today) for s in sent):
        out["status"] = "skipped: one a day"
    else:
        msg = f"ml-lab {rec['name']} cv {rec['cv']:.6f}"
        r = subprocess.run(["kaggle", "competitions", "submit", slug, "-f", str(d / "submission.csv"), "-m", msg],
                           capture_output=True, text=True, timeout=600)
        text = r.stdout + r.stderr
        out["status"] = "sent" if "Successfully submitted" in text else "refused"
        out["kaggle"] = text.strip().splitlines()[-1][:300] if text.strip() else ""
    with open(d / "submissions.jsonl", "a") as f:
        f.write(json.dumps(out) + "\n")
    return out


def run(slug: str, send: bool = True) -> dict:
    metric = METRICS.get(slug, "auc")
    d = fetch(slug)
    X, y, T, ids, idc, target = load(d)
    yv, pos = encode_target(y, metric)
    log(f"{slug}: train {X.shape}, test {T.shape}, metric {metric}, positive class {pos!r}")
    done = {e["name"]: e for e in history(d, "experiments.jsonl")}
    best = max(done.values(), key=lambda e: e["cv"], default=None)
    for name, params, use_counts in LADDER:
        if name in done:
            continue
        t0 = time.time()
        Xe, Te = counts(X, T) if use_counts else (X, T)
        with threadpool_limits(THREADS):
            score, pred = cv(Xe, yv, Te, metric, params)
        rec = {"at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "name": name, "cv": score,
               "metric": metric, "features": Xe.shape[1], "params": params, "seconds": round(time.time() - t0)}
        with open(d / "experiments.jsonl", "a") as f:
            f.write(json.dumps(rec) + "\n")
        log(f"{name}: cv {score:.6f} ({rec['seconds']} s)")
        if best is None or score > best["cv"]:
            best = rec
            pd.DataFrame({idc: ids, target: pred}).to_csv(d / "submission.csv", index=False)
    if send and best is not None and (d / "submission.csv").exists():
        log(f"submit: {submit(slug, d, best)['status']}")
    return {"slug": slug, "best": best}


def status() -> dict:
    """Every competition under ROOT: its experiments and submissions."""
    out = {}
    for d in sorted(p for p in ROOT.glob("*") if p.is_dir()):
        out[d.name] = {"experiments": history(d, "experiments.jsonl")[-10:],
                       "submissions": history(d, "submissions.jsonl")[-5:]}
    return out


if __name__ == "__main__":
    run(sys.argv[1], "--no-submit" not in sys.argv)
