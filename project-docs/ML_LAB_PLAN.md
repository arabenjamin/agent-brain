# ML Lab plan: the brain learns machine learning, then builds and enters its own

Owner's request 2026-10-23 (todo `83199798`): "I'd like the brain to start to
learn ML, and then take what we learn and see if we can build our own models.
Who knows, maybe we can compete, or try and predict the Super Bowl." The todo
names Kaggle's *Squares in Squares* as the first competition. An area of
Research & Development (PROGRAMS_REORG_PLAN.md), next to AI & ML Research:
that one follows the field, this one does the work.

## 1. The first competition, read 2026-10-23

[Squares in Squares](https://www.kaggle.com/competitions/squares-in-squares)
(Kaggle, started 2026-10-21, final deadline 2027-07-18):

- For each n = 1..1000, place n unit squares (centre x, y and angle) without
  overlap in the smallest axis-aligned square. Score per puzzle
  `L/√n − 1`, summed; lower is better. A missing puzzle scores the grid
  packing, so partial submissions are fine. 10 submissions a day, teams of
  up to 5, external data and tools allowed (reasonableness standard); the
  winners license their code CC BY 4.0 or Apache 2.0.
- $50,000: final places $12k/$10k/$8k/$5k/$5k, plus $5,000 to the top of the
  leaderboard at each milestone (2027-01-17, 2027-04-18). No medals or points.
- **It is an optimization problem, not machine learning.** The host suggests
  simulated annealing, gradient descent, evolutionary search or
  reinforcement learning. That makes it a good first project anyway: the
  score is exact and computed locally, the work runs in our own sandbox, and
  search is the part of ML that needs no training data.

Measured on 2026-10-23, two days in:

| Packing | Score |
|---|---|
| Grid for every n (what an empty submission scores) | 28.94 |
| Best known packings ([Ellsworth's table](https://kingbird.myphotos.cc/packing/squares_in_squares.html), 190 entries, almost all n ≤ 324) + grid elsewhere | 25.86 |
| Leaderboard: 1st / 30th (308 teams) | 22.25 / 22.37 |

The puzzles with n > 324 carry 13.57 of the grid's 28.94, and published
records barely cover them. The leaders' extra ~3.6 must come mostly from
there. Reaching the pack (≈22.4) is a matter of known constructions for
large n. Getting to the top means beating published records, which takes
real search time.

## 2. The shape

```
Research & Development
└── ML Lab (ml-lab, core)             NEW
    ├── Track A: learn                ml-study (workflow, every 3 days)
    ├── Track B: compete              squares-in-squares (project, phases B1-B3)
    └── Track C: build our own models after A; candidates filed as todos
```

- **Track A, learn.** A curriculum (a YAML list in the bundle: lesson, source
  URL, exercise) worked through in order. Each run reads one lesson's source
  (articles via `ingest_article`, lectures via the video-learning pipeline),
  writes a study note, then runs the lesson's exercise in the Python sandbox
  and records whether it ran and what it measured. No lesson without an
  exercise that runs. Proposed order: Kaggle Learn (Intro ML, Intermediate ML,
  Feature Engineering), Karpathy's *Neural Networks: Zero to Hero* (micrograd →
  makemore, numpy only), then optimization and search (annealing, gradient
  methods), which feeds Track B.
- **Track B, compete.**
  - **B1, baseline:** import the best-known packings from the table's SVGs
    (`parse_svg_packing.py` keeps full precision); use grid everywhere else.
    Build our own exact scorer and overlap checker that agrees with the
    metric notebook, and write a submission CSV. Expect ≈25.86.
  - **B2, large n:** general constructions for 325–1000: tilted bands and
    Göbel-style families, generated and checked by the scorer.
  - **B3, search:** an annealing optimizer per n that keeps only improvements
    the checker verifies, with each n's best stored in the workspace. It runs
    on the host's 32 CPUs, not inside the 60-CPU-second sandbox. It goes
    after the n where the score gap to √n is largest.
  - **Submitting:** the owner's Kaggle account, by hand at first, uploading
    the CSV the brain writes. A Kaggle API token in `.env` would let the
    brain submit on its own (an outward action, so the owner decides).
- **Track C, our own models.** Once Track A has produced working exercises: a
  tabular model on a public dataset, then a small neural network
  (CPU, or the GPU under `gpu_lease.sh`). The Super Bowl is one candidate (an
  outcome model on public game data); each candidate is a todo the owner
  picks or drops.

Constraints: free models only (paid is owner-started work, see
`routing/a-paid-model-runs-only-on-work-the-owner-started`). The Python sandbox
has no egress, 2 GB, 60 CPU-seconds and 120 s per run. It now carries scipy and
scikit-learn alongside numpy, sympy and pandas. The GPU is the shared 8 GB
card. Code the brain writes in narrow, self-contained units runs well on
gpt-oss (HARNESS2_PLAN.md); Rust stays hand-integrated.

## 3. Owner decisions (2026-10-23)

1. **Kaggle:** the owner's account. A Kaggle API token (`KAGGLE_API_TOKEN`,
   `.env`) lets the runner submit itself; the owner joins the competition on
   the website (accepting the rules cannot be done through the API; until
   then a submission is refused with 403).
2. **Compute:** 12 of the host's 16 cores, overnight. The runner searches
   inside `ML_RUNNER_HOURS` (local, default `23-7`) on `ML_RUNNER_WORKERS`
   (12) processes, and compose caps the container at that many CPUs.
3. **Curriculum:** the order in §2 first (Kaggle Learn, Zero to Hero,
   optimization and search), then fast.ai.
4. **The first model of our own:** chosen once Track A has run (it was
   unclear as asked; it is not a decision yet).

## 4. What runs

**Runner** (`plugins/ml-lab/runner`, compose `ml-runner`, data in
`workspace/ml-lab/squares`, not in git):

- `squares/metric.py` is Kaggle's metric notebook, verbatim (pulled with
  the API). Every packing passes it before it is stored, and the submission
  file is re-scored with it before upload.
- `best/<n>.json` holds the best packing per n. It was seeded from the grid,
  341 of the record page's 367 SVGs (parsed offline with Ellsworth's
  `parse_svg_packing.py`; the other 26 links are dead) and generated
  Göbel squares (`gobel.py`, for the n > 324 drawn without an SVG).
  **Kaggle reads the submission with pandas, so every coordinate reaches
  the metric as a double.** Our first upload (40-digit decimals, contacts
  opened by 1e-30) scored 28.94 there, all grid: rounding closed the gaps
  and every touching pair read as an overlap. Packings are now stored as
  doubles with contacts opened by 1e-12 (a cost of ~3e-10 in all), and
  every check, the store's and the build's, goes through the same pandas
  path.
- Nightly: `optimize.py` runs 10-minute searches on 12 processes. Since
  2026-10-24 that is `lp.py`, the edge LP (below); the annealer (random
  restarts for n ≤ 120, squeezes of holey packings) found nothing in 569
  jobs and stays as `ML_SEARCH=anneal`. It skips proven n (`proven.json`).
  Every attempt goes to `attempts.jsonl`; only metric-checked improvements
  are stored. At the window's close it builds `submission.zip`, scores it,
  and submits when it beats the best one sent (at most 1 a day,
  `ML_SUBMIT=off` stops it).
- Every morning, before the build, the construction families re-run on the
  store: `tilted.py`, `band.py`, `composite.py` and `fault.py` (fault-line
  gadgets, §5 2026-10-24), so a night's find becomes a donor.
- `weak.py` writes `weak.json`: per puzzle, how far the side is above what
  the four smaller intervals reach at the same relative position (the
  excess achievable there does not grow with k).
- `playground/run.py`: the Playground Series (Track A). At the start of each
  window, for each slug in `ML_PLAYGROUND`, it fetches the data, runs a
  5-fold cross-validated ladder (HistGradientBoosting baseline, tuned,
  + frequency encodings), logs every rung to `experiments.jsonl` and submits
  the best when its CV beats the last one sent (one a day). Data in
  `workspace/ml-lab/playground/<slug>`; the owner joins each competition.
  A rung is data, never code: options from a fixed menu (`counts`,
  `ratings_as_category`, `target_encoding` fitted inside each fold,
  `original`: the source dataset added to every training fold) and
  HistGradientBoosting parameters within bounds. More rungs arrive by
  `POST /playground/rung` (checked again, at most 3 waiting) and run in the
  next window; `GET /playground/sources` lists the competition's top-voted
  notebooks and forum, `GET /playground/menu` what a proposal may use.
- `GET /status` and `GET /playground` (brain-internal; `HTTP_ALLOW_INTERNAL`
  lists `ml-runner:8000`).

**Brain** (`plugins/ml-lab`): `squares-report` (daily; written in code; a
note every day, a notification only for an improved puzzle or a sent or
refused submission) and `ml-study` (every 3 days: `ml-student` on gpt-oss
reads the next lesson, runs its exercise in the sandbox, stores
`# ML study — Lnn` under `ml_study`), `squares-study` (daily: one public
method, S01-S12), `playground-report` (daily, from `/playground`) and
`playground-study` (daily: the forum, then the top-voted notebooks; ml-student
stores what each teaches under `playground_study` and ends with one proposed
rung from the menu, or none, which the workflow posts to the runner). The curriculum is the list in
`ml-study.yaml`'s pick step.

Measured 2026-10-23:

| Store | Score |
|---|---|
| Grid | 28.94 |
| + the record page's packings | 24.08 |
| + generated Göbel squares | **23.9235** (the built file, Kaggle's metric, 0 rejected) |

What is left is mostly the 523 puzzles above 324 still at grid (8.07 of
it), between k² and (k+1)². The annealer finds nothing in short runs: from a
grid start, getting below ⌈√n⌉ needs a rotated structure, and every
improvement on a stored packing would be a new record. So B2, constructions
for those n (Göbel strips of the second kind, tilted bands), is the next
lever, and a week of `attempts.jsonl` says whether B3's CPU buys anything.

## 5. Status

- 2026-10-23: competition read; baseline measured (§1); `ml-lab` program
  created under R&D; scipy and scikit-learn added to the Python sandbox.
  The owner decided §3 the same day; runner, store (23.9235), `ml-study`
  and `squares-report` built and running. First lesson (L01) studied, $0.
- 2026-10-24: first submissions. Kaggle refused them (403) until the owner
  verified their identity. The first one through scored 28.94 (the doubles
  lesson, §4); after the fix, Kaggle's score is **23.9234840650**, our
  scorer's to every digit. Rank 350 of 454: the median team is 22.39 and
  the best 22.22, so most teams are ~1.5 past the published records. Finding
  what they share (the competition's discussion forum, public notebooks)
  comes before more search.
- 2026-10-24, B2 from the forum: Chris Deotte's gallery notebook (7th) names
  the families behind strong packings and embeds a per-n reference curve
  (`deotte_payload.json`, total 22.60). Three families are built from it,
  each generated exactly and checked by the metric:
  - `tilted.py` (a tilted b1×b2 core with rows around it): 23.92 → 23.47.
  - `composite.py` (a stored packing in a corner plus a grid): → 23.20.
  - `band.py` (Göbel's strip widened to w rows): → 23.01.
  Composite passes and Deotte's 12 showcase packings bring it to
  **22.9948, Kaggle's score too, rank 332 of 463**. The runner re-runs the
  three families every morning before the build.
- Public notebooks post full solutions at 22.37-22.39 ("Edge LP", "Fault-line
  DP", Deotte's ensemble), which is why the median is 22.39. Measured, not
  used: the best of them per puzzle would beat ours on 414 puzzles (337
  above 324), a blend of **22.3661**. Whether to submit others' packings is
  the owner's call (§6). The owner chose: study the methods first
  (`squares-study`, every 2 days, daily from 10-24; S01-S08), rebuild what we can, blend in
  the rest after.
- 2026-10-24, MIT record registers (Couzo's register, evand, Mishapolk,
  TheSnakeFang, dostashev; `register.py`, the repo kept per packing). Their
  certificates store a rotation as t = tan(θ/2): read as an angle, 329
  packings were rejected until that was fixed. With the families re-run:
  **Kaggle 22.3556317792, rank 64 of 483** (best 22.2248, 10th 22.3048,
  median 22.394). That is better than the blend of public notebooks
  (22.3661), without any of them.
- 2026-10-24 (later): the field moved past us overnight (rank 225 of 642;
  10th 22.2901, median 22.3747), mostly teams submitting the public blend.
  Per puzzle, the downloaded public outputs now beat our store on only 14
  puzzles, 0.005 in all: rebuilding or blending every public method is worth
  at most that. The leaders' further 0.07-0.13 is unpublished.
  The night's annealer search: 569 jobs, nothing improved. The owner chose
  to build the edge LP (S01) now and to run `squares-study` daily.
  `lp.py`: with angles fixed, one edge normal per pair makes non-overlap
  linear in the centres (exact, a fixed axis only narrows the feasible set);
  `slp` also linearises in the angles (at an edge-on kink the half-width
  grows as |Δθ|/2, modelled with an auxiliary, or the LP treats turning as
  free); `squeeze` holds a smaller side and minimises the summed overlap
  (one slack per pair); `polish` hops between them. Known answers pass
  (a 3%-loosened n=10 compacts to its record; n=5 with the centre at 35°
  walks to 45° and 2+1/√2). On our stored records it is near-rigid: 12
  sampled puzzles, nothing in 60 s; the 14 public-gap puzzles, one gain of
  7e-6 (n=131) in 120 s. It replaces the annealer tonight, the public-gap
  puzzles first; `attempts.jsonl` says by 10-26 whether it buys anything.
- 2026-10-24 (evening), owner: rebuild the fault-line method, map the weak
  puzzles, and enter a Playground competition too ("the competitions serve
  multiple purposes"). Above 324 sits 10.15 of the 22.36.
  - `weak.py`: against the smaller intervals' trend the store has 0.009 to
    gain over 50 puzzles. No cluster of weak puzzles; with the public
    measure (14 puzzles, 0.005), we sit at the frontier of what is published.
  - `fault.py`, rebuilt from Deotte's notebook (squares-study S03): a K×K
    grid split by a staircase, the upper part shifted by (+f, +f), gadgets
    (corner crops of dense packings, the least bloat per footprint by
    sorting) along it; a DP over (Σw, Σh, ΣΔ) keeps the least max f, with
    one buffer cell between gadgets. Donors: the store plus the MIT
    registers' best file per n up to 3500 (2426 donors, 3802 gadgets).
    It reproduces the public n=51 gain (7.6904) and stores 134 puzzles:
    22.3556 → **22.3511**, Kaggle 22.3511248784 (to the digit), rank 263
    of 690: the field moved faster. Four public notebooks posted the same
    day (22.3035-22.3079: "Tilt Engine", "Seam Surgery", fault-line v2,
    "ProjUnion") now beat the store on 162 puzzles, 0.050 in all, 0.049 of
    it above 324 and 0.031 on puzzles where we hold a register packing; a
    blend would be ~22.301 (the top 100 is 22.3039). They are S09-S12 of
    `squares-study`, studied first. Blending stays the owner's call.
  - Playground Series S6E10 ("Predicting Airline Satisfaction", AUC,
    closes 10-31): pipeline built and tested end to end on synthetic data.
    The owner joined the same evening. Real data: 699,635 rows, 22 features
    (4 categorical), a boolean target (44% True). Ladder, 5-fold CV AUC on 4
    threads: baseline 0.957652 (26 s), tuned 0.958510 (74 s), + frequency
    encodings 0.958414 (96 s: no gain). Tuned sent: Kaggle **0.95806**
    (CV 0.95851, a gap of 0.0005), rank 2235 of 2922; median 0.95959, top
    25% 0.96165, best 0.96407.
  - `playground-study` built the same evening; first run on the forum (gpt-oss,
    $0): the top thread reports tabular foundation models ahead (TabPFN 3.5
    OOF AUC 0.961145) and a tuned XGBoost at 0.959852, against our 0.958510.
    Neither is on the menu, so it proposed none, as it should; the note's
    Next line did not say what the menu lacks, as asked.
  - Owner: add the model family. A rung now names `model` (hgb, lightgbm,
    xgboost; each takes its own parameters, mapped from shared names;
    LightGBM and XGBoost stop early on 10% held out of the training fold).
    On the full data (4 threads): LightGBM CV 0.958636 (73 s), XGBoost
    0.958594 (114 s), against tuned HistGradientBoosting 0.958510. LightGBM
    sent: Kaggle **0.95843**, rank 1787 of 2926 (from 2235). The family alone
    buys little; the forum's 0.9599 XGBoost carries tuning or features we
    lack, which is the study's next job.
- 2026-10-25, the first night with both tracks: the edge-LP search made its
  first real gains (n=639 -0.0038, n=696 -0.0019, n=457 -0.0008: on the
  fault-line packings, which have slack the imported records lack), but a
  worker died about hourly with no Python error (not memory: a 60 s polish
  peaks at 187 MB) and took the runner down 7 times, so the morning build
  never ran. `optimize.run` now records the jobs of a broken pool as
  crashed and starts a new one (tested by killing a worker), workers carry
  faulthandler, and the morning runs even if the search fails. Morning run
  by hand: the fault-line pass turned the night's finds into 8 more,
  local 22.350811, submitted.
- Playground study, the same day: the web reader returns only the page
  around a Kaggle notebook (title, tags, score), not its code, so the first
  notebook study had nothing to learn from. The runner now pulls a
  notebook's markdown and code with the Kaggle API (`GET
  /playground/notebook?ref=`, 14,000 chars, cached) and the study reads
  that. A second run then failed differently: gpt-oss returned empty
  responses, the fallback model (nemotron) summarised well but stored
  nothing, and the check only required `store_note` when the answer claimed
  "Stored:"; it now requires it always. The third run stored the top
  notebook's lesson: the best public scores (nested CV 0.96217, LB 0.96213)
  are logistic-regression stacks of 124-201 public out-of-fold prediction
  sets plus tabular foundation models.
- Owner: build our own stack and the features. Every scored rung now keeps
  its out-of-fold and test predictions (`oof/<name>.npz`); model `stack` is a
  logistic regression on their logits, on the same folds (rungs scored
  before are re-run once to keep theirs). New options `digits` (each digit
  10^-2..10^5 of the wide numeric columns), `counts_all`,
  `target_encoding_all`, from evgendvorkin's notebook, whose own log has
  them worth 0.95833 → 0.96000 for one LightGBM. Ladder: `lgb-features`
  (those features, its parameters) and `stack`. On a 60k-row copy:
  LightGBM 0.95552, lgb-features 0.95569, stack 0.95589; the backfill path
  reproduces the stack exactly. The full run is the 10-25 night.

## 6. Open

- After `squares-study` (S08 ≈ 2026-10-29, daily since 10-24): rebuild what
  the brain could reproduce, then blend in public packings for the rest
  (owner, 2026-10-24). The 10-24 measure caps that at ~0.005.
- `fault.py` uses a buffer cell between gadgets. Deotte's zero-buffer chains
  (adjacent gadgets sharing an f×f corner where one corner is empty) gained
  him ~0.001; not built.
- Playground menu: LightGBM, XGBoost, our own stack and the strongest
  single-model notebook's features are in (2026-10-25, below). Not built:
  stacking public OOF sets (others' predictions: the owner's call, as
  blending is for Squares); TabPFN (GPU, license first); that notebook's
  later steps (binned keys for target encoding, pair features, original
  target means: +0.0003 together in its log).
- The study's exercise is weak on notebooks (it tabulates the reported
  scores rather than showing the technique).
- Playground: the ladder is three rungs and sits below the median. The
  usual Playground gains, to measure one rung at a time: the original
  dataset the synthetic one was generated from, added to training; the
  1-5 ratings as categoricals; target encoding; other GBM libraries and an
  ensemble of their out-of-fold predictions. Which first comes from
  studying the competition's forum and top notebooks. Next rungs come from the study of
  the competition's top notebooks and forum (a playground-study, as
  squares-study does), and S6E11 opens ~11-01.
- Where the leaders' 0.07-0.13 comes from is unknown. Edge-LP local moves
  do not reach it from our records; the candidates are larger structural
  moves (fault-line gadgets, S02-S03) and proposal searches whose output
  the LP then polishes (S01's GPU stage, S05).

- The second Göbel strip family (n = (a+1)a+3+b, e.g. n = 332, 450, 539,
  796) is not generated yet.
- `parse_svg_packing.py` (and the repository it is from) has no license: it
  is run, never committed. The competition asks a winner to license the
  code that made the submission; the parser only imported published
  records.

## 7. Playbook: what a Kaggle competition workflow should do

Written while working Squares in Squares (2026-10-23/24), so that a
competition workflow can do it next time and get better with each one. Each
step has what it cost and what it bought.

1. **Read the competition**, with `read_page` and reader `anakin` (Kaggle's
   pages need rendering): the task, the metric, deadlines, prizes, rules
   (external data, licenses, daily limit). 10 minutes.
2. **Get the official metric** with the Kaggle API (`kernels/pull` on
   `metric/<slug>`) and vendor it verbatim. Score locally the way Kaggle
   does, through `pd.read_csv` (dev note `ml-lab/kaggle-reads-a-submission-
   through-pandas-check-it-as-doubles`). Our first upload skipped that step and
   lost everything (28.94 on Kaggle against 23.92 locally).
3. **Baseline from published knowledge** before any search. Here: the record
   table (28.94 → 24.08) and formula families (→ 23.92).
4. **Read the forum and the top public notebooks** (`kaggle kernels list
   --competition`, `kernels pull`, `kernels output`) and list what they build
   on. This is where most of the gain was: the families they named (→ 22.99)
   and the MIT record registers they cite (→ 22.66). Measure, per item, what
   each source beats us on before building anything (dev note
   `ml-lab/in-a-kaggle-competition-read-the-forum-and-public-work-befor`).
5. **Check licenses; record every item's source** (dev note
   `ml-lab/import-only-licensed-sources-record-each-one-and-study-publi`).
6. **Submit early and compare** Kaggle's score with the local one. Any gap is
   a bug in the local check.
7. **Study, then search.** The brain studies the public methods
   (`squares-study`); a search runs on the owner's CPU allowance
   (`ml-runner`, 12 cores, 23-7) and logs every attempt, so its value is
   measured.
8. **Report daily** (`squares-report`): score, rank, what changed, what the
   CPU bought.

What stays per competition: the metric file, the data importers, the
constructions or models. What a general workflow can own: steps 1, 2, 4, 5,
6 and 8, plus the decision log in §3.
