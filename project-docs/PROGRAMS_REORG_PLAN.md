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
| Shared services | 7/7 | 667 | 348 | 121 | 0 | On-demand. `fill-knowledge-gap` is 646 of the runs (§3.4). |

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

## 3. Owner decisions (2026-10-23)

1. **R&D areas:** Security Engineering (zero-trust-architecture-research
   moved from NIA › Cybersecurity & Privacy) and Home Lab (no workflows yet;
   collects what rnd-intake and the hand-offs file). Agent engineering stays
   in Brain Ops. `rnd-scout` files under the area a candidate fits.
2. **The `*-rss` drafts:** one run each, 2026-10-23. The outlet feeds put
   one item into a report in three runs (the FCC's HiSilicon device ban, in
   surveillance); everything else came from the searches. The feeds were
   folded into `surveillance-privacy-watch` (optional steps); all three
   drafts deleted.
3. **Switched-off workflows deleted:** brain-exercise, todo-review-chat,
   self-test-dead-tools, news-analysis, weekly-news, hardware-tripwire-rss.
   `relation-extraction` stays off until the Ollama quota question is
   settled.
4. **`fill-knowledge-gap`:** measured on 168 runs (2026-10-14..20, the first
   week its notes link to their runs): 140 notes never read again, 26 read
   once or twice, 2 three or more times. "Never cited" was a false signal:
   nothing extracts claims from gap notes. About 11 of 20 sampled questions
   fit an NIA vertical, 2 R&D, 7 no program. Now capped at 10 a day
   (`GAP_TASKS_DAILY_MAX`) and only for a question the local model places
   under an active program's topic.
5. `critical-tech-dependencies` named a workflow that never existed as a
   file (`weekly-sodium-battery-research`; hardware-tripwire already
   searches sodium-ion): removed.
6. **Paid models per workflow** (routing rule `routing/a-paid-model-runs-only-on-work-the-owner-started`):
   open. After a month of results (~2026-11-23), decide which workflow, if
   any, gets `paid: true`. The candidates are the five standing
   investigations whose synthesis used Sonnet (`deep_reasoning`) until
   2026-10-23.

## 4. Status

- 2026-10-23: audit done; R&D program, the two moves, `rnd-intake` and
  `rnd-scout` built; harness workflows assigned to Brain Ops. §3 decided
  the same day (only the paid question waits).
- First `rnd-intake` run (task fe227756, gpt-oss, $0): filed one todo,
  "R&D: AdFection zero-click spyware delivery, learn" (`6abd28d3`), quoting
  the 2026-10-19 surveillance-privacy-watch report correctly; its "use it"
  suggestion (mesh network agents) is a stretch. Watch the next few weeks for
  whether the picks are things we would actually learn from or use.
