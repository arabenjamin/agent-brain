# Programs plan

Planned 2026-10-17 from the owner's todo "Create Workflow groups and Agent
Workflow Admin" (`0fcc6502`). Nothing built yet.

## 1. Why

The brain runs 40 workflows. They carry a `kind` (brief, learning,
maintenance, monitor) and 13 carry a `topic`, but nothing says what a group of
them is *for*, who looks after it, or whether it is worth running. The last 30
days, from the run history:

| Workflow | Runs | Notes made | Times those notes were read |
|---|---|---|---|
| intelligence-agency-claims-monitor | 30 | 69 | 2 |
| off-grid-networking-monitor | 4 | 11 | 11 |
| video-learning | 486 | 648 | 209 |
| fill-knowledge-gap | 754 | 151 | 68 |

Some work is read, some is not, and nobody is asked. The owner's ask: group
work into departments, projects, investigations and cases; give each a
manager that keeps its workflows and agents useful; and edit all of it like
everything else.

## 2. The model

A **Program** organizes work. It is not a topic: a topic labels *notes* by
subject; a program owns *work* (workflows, agents, APIs, todos) and names the
topics it covers.

```yaml
# plugins/<bundle>/programs/<name>.yaml
name: nia                       # slug, ^[a-z0-9][a-z0-9-]{0,63}$
label: Niche Intelligence Agency
kind: department                # department | project | investigation | case
parent: null                    # another program's name: programs nest
status: active                  # active | paused | closed
charter: >-                     # what it is for, in a few sentences
  ...
success: >-                     # what good output looks like; the manager judges against it
  ...
topics: [nia]
workflows: [trump-benefit-daily-monitor]
agents: []                      # profiles its work relies on
apis: []                        # ApiContext / McpServer names
review: { every: 1w }           # the manager's review cadence
```

- **Kinds.** A *department* is standing work. A *project* has a deliverable
  and ends. An *investigation* or *case* has a question and closes with a
  finding.
- **Nesting.** `parent` builds a tree (NIA over its verticals). Scorecards
  roll up the tree, and a parent's manager sees its children's.
- **One owner per workflow.** A workflow belongs to at most one program, so
  someone is accountable for it. On-demand chains used by everyone
  (fill-knowledge-gap, deep-dive, deep-research, default, diagnose, codebase)
  are **shared services**, listed as such.
- **Todos** gain an optional `program` field, so a program's open work is a
  list.

## 3. Editing (like everything else)

- **Files:** a new seed kind `programs/` in plugin bundles. Core programs in
  `plugins/core/programs/`, personal ones (NIA, Jobsearch) in the private
  bundle.
- **Tool:** `manage_program` (`list | get | upsert | delete | audit`), the
  same shape as `manage_workflow`.
- **REST and HBI:** `/api/programs` and a Programs panel with a tree, an
  editor and the scorecard.
- **Checked like a form on every save** (seed, tool, REST, HBI), as profiles
  are. Hard (refused): unknown keys, a bad name, an unknown parent, a cycle,
  a workflow, topic, agent or API that does not exist, or a workflow claimed
  by two programs. Soft (shown): a program with no workflows, an enabled
  workflow in no program and not shared, a closed program with enabled
  workflows.
- **Ownership** works as for workflows and profiles today (`managed_by:
  yaml | runtime`, reset to the file), until the YAML hot-sync todo
  (`f3ddabb9`) replaces it. Programs are the first new kind that sync must
  cover.

## 4. The starting tree (proposed; the owner edits it)

- **Niche Intelligence Agency** (`nia`, department; topic `nia`)
  - **Critical Tech Dependencies** (department; the NIA's first vertical;
    topics `semiconductors`, `batteries-materials`): hardware-tripwire,
    tech-dependency-synthesis, weekly-sodium-battery-research (disabled)
  - **National Security & Intelligence** (department; topic
    `national-security`): intelligence-agency-claims-monitor
  - **Cybersecurity & Privacy** (department; topic `cybersecurity`):
    zero-trust-architecture-research
  - **Accountability** (department; topics `us-politics`, `nia`):
    trump-benefit-daily-monitor
- **Jobsearch** (department; topic `my-job-search`): ai-dev-board-job-search,
  tech-hiring-trends-weekly; APIs aidevboard, worklittle, jobs-board-remoteok.
  The Jobs Board todo's follow-up research workflows land here.
- **AI & ML Research** (department; topic `ai-ml`): slm-benchmark-watch,
  rust-training-data-scout, deep-dive-pick
- **Off-grid Networking** (project; topic `off-grid-networking`):
  off-grid-networking-monitor
- **Briefings & Intake** (department): daily-news, weekly-news,
  news-analysis, media-watch, discover-media-sources, video-learning,
  article-learning
- **Brain Ops** (department; topic `agent-brain`): health-monitor,
  capability-mining, brain-api-sync, behavior-candidates, api-scout,
  claim-verification, entity-linking, relation-extraction, consolidate,
  item-dispatch, todo-snapshot, overdue-todo-review, todo-review-chat,
  self-test-dead-tools

## 5. The scorecard (measure before managing)

Per workflow, over a window (default 30 days), rolled up per program:
runs and success rate; notes made; notes read again (`access_count`); notes
cited later (claim evidence, briefs); cost; last run. Per agent: delegations,
rounds used, tool calls that failed their schema (`arg_checks`), cost.

Known gaps: `model_usage` has no task id, so cost per workflow run cannot be
attributed yet (Phase 0 adds it). "Read again" is a thin proxy for value. The
owner's rating in the HBI is the better signal and comes later.

## 6. The manager

One generic **`program-manager`** agent (Haiku 5.5, `orchestration`), not one
per department: a department's specifics are its charter and success
criteria, which are data. A `program-review` workflow runs it per program at
its `review.every`. It reads the scorecard, the charter, and its children's
reviews, and returns **proposals as todos tagged with the program**: pause a
workflow nobody reads, fix one that keeps failing, draft a new one (through
`schedule-author`), tweak an agent. It never changes anything itself in the
first version; the owner approves. This is the pattern of `API_USE_PLAN.md`.

The front door delegates "how is the NIA doing?" to `program-manager`. The
original todo named the front door as every department's manager; keeping the
manager separate keeps chat a conversation and the review a scheduled job.

## 7. Phases

| Phase | Scope | Done when |
|---|---|---|
| 0. Scorecard | `workflow_scorecard` lookup view; task id on `model_usage` | The §1 table, with cost, comes from one view |
| 1. Programs | Seed kind, validator, `manage_program`, REST, graph nodes, the §4 tree seeded, `program` on todos | Every enabled workflow is in a program or shared; a bad save is refused with what fits |
| 2. HBI | Programs panel: tree, editor, scorecard | The owner edits a program and sees its scorecard |
| 3. Manager | `program-manager` profile, `program-review` workflow, proposals as todos | One week of NIA reviews read by the owner |
| 4. Agents | Agent rows in the scorecard; agent-tweak proposals | The manager proposes a profile change with evidence |

## 8. Decisions for the owner

1. The §4 tree: is it right? In particular: does Off-grid belong under the
   NIA, and does Accountability (the Trump-benefit monitor) belong there or in
   its own program?
2. Does pausing a program pause its workflows (recommended: yes, without
   editing each workflow)?
3. Program budgets: a monthly cap per program inside the provider caps, or
   not yet (recommended: not yet; cost goes on the scorecard first)?
4. Should an *investigation* close itself when its question is answered, or
   only on the owner's say?
