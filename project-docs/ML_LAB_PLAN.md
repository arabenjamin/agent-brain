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

## 3. Owner decisions

1. **Kaggle account:** enter the competition under the owner's account (accept
   the rules; entry closes 2027-07-11). Submit by hand, or add a Kaggle API
   token so the brain submits?
2. **B3 compute:** a long-running optimizer on the host CPUs needs a runner
   with more time than the sandbox gives, either a third sandbox with long
   limits or a workspace job. How many cores, and when (overnight only)?
3. **Curriculum:** the order in §2 Track A, or another (fast.ai, d2l.ai)?
4. **Track C:** which first model after the curriculum, and whether the
   Super Bowl is in. Sports are otherwise out of every brain workflow.

## 4. Status

- 2026-10-23: competition read; baseline measured (§1); `ml-lab` program
  created under R&D; scipy and scikit-learn added to the Python sandbox; todo
  `83199798` moved to `ml-lab` with this plan as its next step. Nothing
  submitted.
