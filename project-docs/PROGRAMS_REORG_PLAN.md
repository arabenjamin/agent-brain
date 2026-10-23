# Programs reorg plan: an R&D program, and the NIA hands it what it finds

Owner's request 2026-10-23: audit the programs and their workflows; out of it,
a Research & Development program with many areas, NIA discoveries passed to
R&D "to see if we can learn from it or use it", and Off-grid & Decentralized
Networking inside R&D. Builds on PROGRAMS_PLAN.md (membership lives on the
program, `parent` nests programs, a workflow run's todos carry its program).

## 1. The audit (30 days to 2026-10-23)

11 programs, 56 workflows. Numbers are the `workflow_scorecard` view: runs,
notes the runs wrote, how often those notes were read again and cited by
claims.

| Program | Workflows (on/all) | Runs | Notes | Read | Cited | Finding |
|---|---|---|---|---|---|---|
| AI & ML Research | 4/4 | 17 | 21 | 0 | 1 | Already research: model tier, fine-tuning data, deep-dive questions, the HN trial. Nothing it writes has been read again. |
| Brain Ops | 16/19 | 829 | 126 | 6 | 0 | Healthy plumbing. `harness-candidates` and `harness-improve` (built 10-23) belong here but are in no program. |
| Briefings & Intake | 6/8 | 850 | 2,084 | 702 | 334 | The brain's main input: video-learning and article-learning are most of what gets read and cited. |
| Jobsearch | 2/2 | 6 | 1 | 0 | 0 | Small and working; the job search sends notifications, not notes. |
| NIA (+4 verticals) | 8/8 | 103 | 230 | 32 | 25 | The best-cited program after intake. Its reports name technologies, tools and methods (Reticulum, zero trust, chip packaging, CISA KEV), and none of that reaches the work that could use it. |
| Off-grid & Decentralized Networking | 1/1 | 4 | 26 | 12 | 2 | A research interest (mesh, Reticulum, LoRa), not intelligence; on its own since 10-17. |
| Shared services | 7/7 | 667 | 348 | 121 | 0 | On-demand. `fill-knowledge-gap` is 646 of the runs; its notes are read (115) but never cited. |

Workflows outside every program: `harness-candidates`, `harness-improve`
(enabled) and three `*-rss` drafts the API match digest made on 2026-10-19
(`vulnerability-watch-rss`, `surveillance-privacy-watch-rss`,
`zero-trust-architecture-research-rss`). The drafts are enabled, have no
schedule, run only by hand, and have never run.

Off, kept for now: `brain-exercise`, `relation-extraction`, `todo-review-chat`
(Brain Ops), `news-analysis`, `weekly-news` (Briefings), `hardware-tripwire-rss`,
`self-test-dead-tools`.

What was missing: **no program hands work to another.** A program review
files todos for its own program; the chat hand-off's `intake` agent places one
link at a time. Nothing reads what the NIA found and asks whether the brain
should learn from it or use it.

## 2. The shape

```
Research & Development  (research-development, core)   NEW
├── AI & ML Research        (ai-ml-research, core)       moved under R&D
├── Off-grid & Decentralized Networking (personal)       moved under R&D
└── workflow rnd-intake (weekly)                          NEW: NIA → R&D
NIA (unchanged) ── its verticals' reports feed rnd-intake
Brain Ops ── + harness-candidates, harness-improve
```

- **R&D's charter:** find what the brain could learn from or use (techniques,
  tools, models, datasets, protocols) and turn the best into a deep dive or
  something built. Its areas are child programs; each keeps its own topics,
  workflows and 30-day review. The review of R&D reads its areas' reviews.
- **`rnd-intake`** (weekly, free models only): reads the last 7 days of
  reports from every workflow the NIA subtree owns, plus R&D's own open todos,
  and has `rnd-scout` (gpt-oss) pick at most two things worth learning from or
  using. Each becomes a todo under R&D whose `ask` is the owner's decision:
  learn (a deep dive), use (try it in our own work), or drop. A week with
  nothing worth it files nothing. A repeated title merges into the open todo.
- **Membership only:** moving a program under R&D changes its `parent`, not
  its workflows, topics or schedules. Pausing R&D pauses its areas.

## 3. Proposals for the owner (not done)

1. **New areas when they have work:** Security engineering (zero trust and
   the vault applied to our own systems; `zero-trust-architecture-research`
   could move from NIA › Cybersecurity, since it reads as "what should we
   adopt" more than "who gains or loses"), Home lab (the tailnet discovery
   todo `8b29f766`), Agent engineering (the Harness² loop, if it outgrows
   Brain Ops).
2. **The three `*-rss` drafts:** run each once and compare with its original,
   as their descriptions ask, then fold the feeds in or delete the draft.
3. **Retire or restart what is off:** `news-analysis` and `weekly-news` were
   read (16 and 0 reads); `brain-exercise` was replaced by `deep-dive-pick`;
   `relation-extraction` succeeded in 52 of 69 runs. Each is a delete or a fix.
4. **`fill-knowledge-gap`:** 646 runs and no citations. Worth a look at what
   triggers it and whether its notes answer anything.
5. **`critical-tech-dependencies` names a workflow that no longer exists**
   (`weekly-sodium-battery-research`): every startup drops it with a WARN.
   Remove it from the program file or bring the workflow back.
6. **Paid models per workflow** (routing rule `routing/a-paid-model-runs-only-on-work-the-owner-started`):
   after a month of results (~2026-11-23), decide which workflow, if any,
   gets `paid: true`. The candidates are the five standing investigations
   whose synthesis used Sonnet (`deep_reasoning`) until 2026-10-23.

## 4. Status

- 2026-10-23: audit done; R&D program, the two moves, `rnd-intake` and
  `rnd-scout` built; harness workflows assigned to Brain Ops. §3 waits for
  the owner.
- First `rnd-intake` run (task fe227756, gpt-oss, $0): filed one todo,
  "R&D: AdFection zero-click spyware delivery, learn" (`6abd28d3`), quoting
  the 2026-10-19 surveillance-privacy-watch report correctly; its "use it"
  suggestion (mesh network agents) is a stretch. Watch the next few weeks for
  whether the picks are things we would actually learn from or use.
