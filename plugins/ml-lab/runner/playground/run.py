"""One Playground competition: fetch, cross-validate a ladder, submit the best.

The Playground Series is Kaggle's monthly tabular competition, where Track A
meets a real target. Each rung of the ladder is one experiment, scored by
5-fold cross-validation with the competition's metric on the training set
(out-of-fold), so every rung says what it bought before Kaggle sees anything.

A rung is data, never code: a name, options from MENU and model parameters
within PARAMS' bounds. The first rungs are LADDER; more come from the
brain's playground-study (POST /playground/rung, checked by `validate`) and
wait in queue.jsonl for the next window. Options:

- counts: frequency of each value (categoricals, low-cardinality numerics)
  over train + test;
- ratings_as_category: integer columns with at most 10 distinct values
  (1-5 survey ratings) treated as categories;
- target_encoding: each categorical's out-of-fold target mean, fitted inside
  every training fold (cross-fitted by sklearn's TargetEncoder);
- original: the dataset the synthetic one was generated from (COMPETITIONS),
  added to every training fold; scoring stays on competition rows.

Every experiment goes to experiments.jsonl. The best by CV is written as
submission.csv and submitted when it beats the CV of the last one sent
(ML_SUBMIT=off stops it; one a day). Data lives in /playground/<slug>.

    python -m playground.run playground-series-s6e10 [--no-submit]
"""

from __future__ import annotations

import json
import os
import re
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
from sklearn.preprocessing import TargetEncoder
from threadpoolctl import threadpool_limits

ROOT = Path(os.environ.get("ML_PLAYGROUND_DIR", "/playground"))
THREADS = int(os.environ.get("ML_RUNNER_WORKERS", "12"))
FOLDS = 5
SEED = 42
MAX_QUEUED = 3  # proposals waiting per competition

# Per competition: the metric (from its overview page) and the original
# dataset it was generated from, if public (the competition allows external
# data; the dataset's own Kaggle page names its license).
COMPETITIONS = {
    "playground-series-s6e10": {
        "metric": "auc",
        "original": {"dataset": "teejmahal20/airline-passenger-satisfaction",
                     "target": "satisfaction", "positive": "satisfied"},
    },
}

MENU = {
    "counts": "frequency encodings of categoricals and low-cardinality numerics",
    "ratings_as_category": "integer columns with <= 10 distinct values as categories",
    "target_encoding": "out-of-fold target mean of each categorical, fitted inside each fold",
    "original": "add the original dataset to every training fold",
}
# HistGradientBoosting parameters a rung may set: (type, low, high).
PARAMS = {
    "learning_rate": (float, 0.005, 0.5),
    "max_iter": (int, 10, 5000),
    "max_leaf_nodes": (int, 2, 1023),
    "max_depth": (int, 1, 64),
    "min_samples_leaf": (int, 1, 5000),
    "l2_regularization": (float, 0.0, 100.0),
    "max_features": (float, 0.1, 1.0),
    "max_bins": (int, 16, 255),
    "n_iter_no_change": (int, 5, 500),
}
TUNED = {"learning_rate": 0.05, "max_iter": 2000, "max_leaf_nodes": 63, "min_samples_leaf": 40,
         "l2_regularization": 1.0, "n_iter_no_change": 50}
LADDER = [
    {"name": "baseline", "options": {}, "params": {}},
    {"name": "tuned", "options": {}, "params": TUNED},
    {"name": "counts", "options": {"counts": True}, "params": TUNED},
]


def log(msg: str) -> None:
    print(time.strftime("%Y-%m-%d %H:%M:%S"), msg, flush=True)


def validate(spec) -> tuple[dict | None, str]:
    """A rung proposal checked against MENU and PARAMS: (rung, "") or (None, why)."""
    if isinstance(spec, str):
        try:
            spec = json.loads(spec)
        except ValueError:
            return None, "not JSON"
    if not isinstance(spec, dict):
        return None, "not an object"
    name = spec.get("name")
    if not isinstance(name, str) or not re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,39}", name):
        return None, "name: 1-40 of a-z 0-9 _ -"
    opts = spec.get("options") or {}
    if not isinstance(opts, dict) or any(k not in MENU or not isinstance(v, bool) for k, v in opts.items()):
        return None, f"options: booleans among {sorted(MENU)}"
    params = spec.get("params") or {}
    if not isinstance(params, dict):
        return None, "params: an object"
    clean = {}
    for k, v in params.items():
        if k not in PARAMS:
            return None, f"params: {k} is not one of {sorted(PARAMS)}"
        t, lo, hi = PARAMS[k]
        if isinstance(v, bool) or not isinstance(v, (int, float)) or not lo <= v <= hi:
            return None, f"params: {k} must be a number in [{lo}, {hi}]"
        clean[k] = t(v)
    rung = {"name": name, "options": {k: v for k, v in opts.items() if v}, "params": clean}
    for k in ("proposed_by", "reason", "source"):
        if isinstance(spec.get(k), str):
            rung[k] = spec[k][:500]
    return rung, ""


def queue(slug: str, spec) -> tuple[bool, str]:
    """Add a proposal to the competition's queue (POST /playground/rung)."""
    if slug not in COMPETITIONS:
        return False, f"unknown competition {slug!r}"
    rung, why = validate(spec)
    if rung is None:
        return False, why
    d = ROOT / slug
    d.mkdir(parents=True, exist_ok=True)
    names = {e["name"] for e in history(d, "experiments.jsonl")} | {r["name"] for r in LADDER}
    waiting = [r for r in history(d, "queue.jsonl") if r["name"] not in names]
    if rung["name"] in names or rung["name"] in {r["name"] for r in waiting}:
        return False, f"a rung named {rung['name']!r} exists"
    if len(waiting) >= MAX_QUEUED:
        return False, f"{len(waiting)} rungs already wait for the next window"
    rung["queued_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    with open(d / "queue.jsonl", "a") as f:
        f.write(json.dumps(rung) + "\n")
    return True, "queued for the next window"


def _kaggle(*args, timeout=900) -> str:
    r = subprocess.run(["kaggle", *args], capture_output=True, text=True, timeout=timeout)
    return r.stdout + r.stderr


def _unzip(d: Path) -> None:
    for z in d.glob("*.zip"):
        with zipfile.ZipFile(z) as zf:
            zf.extractall(d)
        z.unlink()


def fetch(slug: str) -> Path:
    d = ROOT / slug
    d.mkdir(parents=True, exist_ok=True)
    if not (d / "train.csv").exists():
        out = _kaggle("competitions", "download", slug, "-p", str(d))
        if not list(d.glob("*.zip")):
            raise RuntimeError(f"download failed (joined the competition?): {out[-300:]}")
        _unzip(d)
    return d


def original(slug: str, d: Path) -> pd.DataFrame | None:
    """The original dataset (every CSV in it), downloaded once."""
    cfg = COMPETITIONS.get(slug, {}).get("original")
    if not cfg:
        return None
    od = d / "original"
    if not od.is_dir() or not list(od.glob("*.csv")):
        od.mkdir(exist_ok=True)
        out = _kaggle("datasets", "download", cfg["dataset"], "-p", str(od))
        _unzip(od)
        if not list(od.glob("*.csv")):
            raise RuntimeError(f"original dataset download failed: {out[-300:]}")
    return pd.concat([pd.read_csv(f) for f in sorted(od.glob("*.csv"))], ignore_index=True)


def load(slug: str, d: Path, with_original: bool):
    train, test = pd.read_csv(d / "train.csv"), pd.read_csv(d / "test.csv")
    sample = pd.read_csv(d / "sample_submission.csv")
    idc, target = sample.columns[0], sample.columns[1]
    y = train[target]
    X = train.drop(columns=[idc, target])
    T = test.drop(columns=[idc])
    O, yo = None, None
    if with_original:
        raw = original(slug, d)
        cfg = COMPETITIONS[slug]["original"]
        missing = [c for c in X.columns if c not in raw.columns]
        if missing or not (pd.api.types.is_bool_dtype(y) or pd.api.types.is_integer_dtype(y)):
            raise RuntimeError(f"original data does not match: missing {missing}, target {y.dtype}")
        O = raw[list(X.columns)].reset_index(drop=True)  # the competition's columns, in its order
        # The competition's target is True/1 for the original's positive label.
        yo = (raw[cfg["target"]] == cfg["positive"]).astype(y.dtype).values
    frames = [X, T] + ([O] if O is not None else [])
    for c in X.columns:
        if not pd.api.types.is_numeric_dtype(X[c]) and not pd.api.types.is_bool_dtype(X[c]):
            cats = pd.CategoricalDtype(sorted(set().union(*(set(f[c].dropna()) for f in frames))))
            for f in frames:
                f[c] = f[c].astype(cats)
    return X, y, T, O, yo, test[idc], idc, target


def encode_target(y, metric: str):
    y = pd.Series(y)
    if metric == "auc" and not pd.api.types.is_numeric_dtype(y):
        pos = sorted(y.unique())[-1]
        return (y == pos).astype(int).values, pos
    return y.values.astype(float) if metric != "auc" else y.values.astype(int), None


def ratings_as_category(frames: list[pd.DataFrame]) -> list[pd.DataFrame]:
    out = [f.copy() for f in frames]
    both = pd.concat(frames)
    for c in frames[0].columns:
        if pd.api.types.is_integer_dtype(both[c]) and both[c].nunique() <= 10:
            cats = pd.CategoricalDtype(sorted(both[c].dropna().unique()))
            for f in out:
                f[c] = f[c].astype(cats)
    return out


def counts(frames: list[pd.DataFrame]) -> list[pd.DataFrame]:
    """Frequency of each value over every frame, for categoricals and numeric
    columns with few distinct values."""
    out = [f.copy() for f in frames]
    both = pd.concat(frames)
    for c in list(frames[0].columns):
        if isinstance(both[c].dtype, pd.CategoricalDtype) or both[c].nunique() <= 200:
            freq = both[c].value_counts()
            for f, g in zip(out, frames):
                f[f"{c}_count"] = g[c].map(freq).astype(float).values
    return out


def cv(X, y, T, O, yo, metric, params, te: bool):
    """Out-of-fold score and the fold-averaged test prediction. O (the
    original data) joins every training fold and is never scored."""
    clf = metric == "auc"
    split = (StratifiedKFold if clf else KFold)(FOLDS, shuffle=True, random_state=SEED)
    oof, pred = np.zeros(len(X)), np.zeros(len(T))
    cats = [c for c in X.columns if isinstance(X[c].dtype, pd.CategoricalDtype)]
    for k, (tr, va) in enumerate(split.split(X, y)):
        Xtr, ytr = X.iloc[tr], y[tr]
        if O is not None:
            Xtr, ytr = pd.concat([Xtr, O], ignore_index=True), np.concatenate([ytr, yo])
        Xva, Tk = X.iloc[va], T
        if te and cats:
            inner = (StratifiedKFold if clf else KFold)(5, shuffle=True, random_state=SEED + k)
            enc = TargetEncoder(target_type="binary" if clf else "continuous", cv=inner)
            names = [f"{c}_te" for c in cats]
            Xtr = Xtr.reset_index(drop=True).assign(**dict(zip(names, enc.fit_transform(Xtr[cats].astype(str), ytr).T)))
            Xva = Xva.assign(**dict(zip(names, enc.transform(Xva[cats].astype(str)).T)))
            Tk = T.assign(**dict(zip(names, enc.transform(T[cats].astype(str)).T)))
        Model = HistGradientBoostingClassifier if clf else HistGradientBoostingRegressor
        stop = {"early_stopping": True, "validation_fraction": 0.1} if params.get("max_iter", 100) > 100 else {}
        m = Model(categorical_features="from_dtype", random_state=SEED + k, **stop, **params)
        m.fit(Xtr, ytr)
        if clf:
            oof[va] = m.predict_proba(Xva)[:, 1]
            pred += m.predict_proba(Tk)[:, 1] / FOLDS
        else:
            oof[va] = m.predict(Xva)
            pred += m.predict(Tk) / FOLDS
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
        text = _kaggle("competitions", "submit", slug, "-f", str(d / "submission.csv"), "-m",
                       f"ml-lab {rec['name']} cv {rec['cv']:.6f}", timeout=600)
        out["status"] = "sent" if "Successfully submitted" in text else "refused"
        out["kaggle"] = text.strip().splitlines()[-1][:300] if text.strip() else ""
    with open(d / "submissions.jsonl", "a") as f:
        f.write(json.dumps(out) + "\n")
    return out


def run_rung(slug: str, d: Path, rung: dict, metric: str):
    opts = rung["options"]
    X, y, T, O, yo, ids, idc, target = load(slug, d, bool(opts.get("original")))
    yv, _ = encode_target(y, metric)
    yov = None if O is None else encode_target(yo, metric)[0]
    frames = [X, T] + ([O] if O is not None else [])
    if opts.get("ratings_as_category"):
        frames = ratings_as_category(frames)
    if opts.get("counts"):
        frames = counts(frames)
    X, T = frames[0], frames[1]
    O = frames[2] if O is not None else None
    te = bool(opts.get("target_encoding"))
    with threadpool_limits(THREADS):
        score, pred = cv(X, yv, T, O, yov, metric, rung["params"], te)
    n_te = sum(isinstance(X[c].dtype, pd.CategoricalDtype) for c in X.columns) if te else 0
    return score, pred, X.shape[1] + n_te, ids, idc, target


def run(slug: str, send: bool = True) -> dict:
    metric = COMPETITIONS.get(slug, {}).get("metric", "auc")
    d = fetch(slug)
    done = {e["name"]: e for e in history(d, "experiments.jsonl")}
    best = max(done.values(), key=lambda e: e["cv"], default=None)
    rungs = LADDER + [r for r in history(d, "queue.jsonl") if validate(r)[0] is not None]
    for rung in rungs:
        if rung["name"] in done:
            continue
        t0 = time.time()
        try:
            score, pred, nf, ids, idc, target = run_rung(slug, d, rung, metric)
        except Exception as e:  # one bad rung must not stop the ladder
            rec = {"at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "name": rung["name"],
                   "error": f"{type(e).__name__}: {str(e)[:300]}", "options": rung["options"], "params": rung["params"]}
            with open(d / "experiments.jsonl", "a") as f:
                f.write(json.dumps(rec) + "\n")
            done[rung["name"]] = rec
            log(f"{rung['name']}: failed, {rec['error']}")
            continue
        rec = {"at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "name": rung["name"], "cv": score,
               "metric": metric, "features": nf, "options": rung["options"], "params": rung["params"],
               "seconds": round(time.time() - t0)}
        for k in ("proposed_by", "reason", "source"):
            if k in rung:
                rec[k] = rung[k]
        with open(d / "experiments.jsonl", "a") as f:
            f.write(json.dumps(rec) + "\n")
        done[rung["name"]] = rec
        log(f"{rung['name']}: cv {score:.6f} ({rec['seconds']} s)")
        if best is None or score > best.get("cv", -1e9):
            best = rec
            pd.DataFrame({idc: ids, target: pred}).to_csv(d / "submission.csv", index=False)
    if send and best is not None and (d / "submission.csv").exists():
        log(f"submit: {submit(slug, d, best)['status']}")
    return {"slug": slug, "best": best}


def sources(slug: str) -> dict:
    """The competition's top-voted public notebooks (refreshed daily) and its
    forum, for the brain's playground-study."""
    d = ROOT / slug
    d.mkdir(parents=True, exist_ok=True)
    cache = d / "sources.json"
    if cache.exists() and time.time() - cache.stat().st_mtime < 86400:
        return json.loads(cache.read_text())
    out = _kaggle("kernels", "list", "--competition", slug, "--sort-by", "voteCount", "--page-size", "20", "-v", timeout=120)
    notebooks = []
    lines = out.strip().splitlines()
    if lines and lines[0].startswith("ref"):
        import csv
        for row in csv.DictReader(lines):
            notebooks.append({"url": f"https://www.kaggle.com/code/{row['ref']}", "title": row.get("title", ""),
                              "votes": int(row.get("totalVotes") or 0)})
    res = {"forum": f"https://www.kaggle.com/competitions/{slug}/discussion?sort=votes", "notebooks": notebooks}
    if notebooks:
        cache.write_text(json.dumps(res))
    return res


def status() -> dict:
    """Every competition under ROOT: its experiments, the queue, submissions,
    and the menu a proposal may use."""
    out = {}
    for d in sorted(p for p in ROOT.glob("*") if p.is_dir()):
        done = {e["name"] for e in history(d, "experiments.jsonl")}
        out[d.name] = {"experiments": history(d, "experiments.jsonl")[-15:],
                       "queued": [r for r in history(d, "queue.jsonl") if r["name"] not in done],
                       "submissions": history(d, "submissions.jsonl")[-5:]}
    return out


def menu() -> dict:
    return {"options": MENU, "params": {k: [t.__name__, lo, hi] for k, (t, lo, hi) in PARAMS.items()},
            "ladder": LADDER}


if __name__ == "__main__":
    run(sys.argv[1], "--no-submit" not in sys.argv)
