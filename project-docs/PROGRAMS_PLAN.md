# Programs plan

Planned 2026-10-17 from the owner's todo "Create Workflow groups and Agent
Workflow Admin" (`0fcc6502`). Phases 0–4 and 2b built 2026-10-17. Open: turn on `program-reviews`; per-program budget enforcement once costs are measured.

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
- **Off-grid & Decentralized Networking** (department; topic `off-grid-networking`; owner: its own program, not under the NIA):
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

**Built (Phase 0, 2026-10-17):** `lookup` view `workflow_scorecard`
(`plugins/core/views/`; slots `days`, `workflow`). The graph half gives runs,
ok, failed, notes, reads, cited and last run; a DuckDB half joins
`model_calls`, `tokens_in` and `cost_usd` per run from `model_usage.task_id`
(new column, written from the workflow run's owner task). Views gained `sql` +
`join` for this (integer slots only; dev note "A view may add ledger
columns"). An index on `Note.produced_by_task` took it from 45 s to 0.1 s.

Known gaps: cost exists only for runs from 2026-10-17 on (older ledger rows
and chat turns have no task id). Per-program roll-up waits for programs
(Phase 1); agent rows wait for Phase 4. "Read again" is a thin proxy for
value. The owner's rating in the HBI is the better signal and comes later.

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
| 0. Scorecard (**done** 2026-10-17) | `workflow_scorecard` lookup view; task id on `model_usage` | The §1 table, with cost, comes from one view |
| 1. Programs (**done** 2026-10-17) | Seed kind, validator, `manage_program`, REST, graph nodes, the §4 tree seeded, `program` on todos | Every enabled workflow is in a program or shared; a bad save is refused with what fits |
| 2. HBI (**done** 2026-10-17) | Programs panel: tree, editor, scorecard | The owner edits a program and sees its scorecard |
| 2b. Assist + schema (owner, 2026-10-17; **done** 2026-10-17) | The New Program drawer gets the `AgentAssist` widget like the workflow form (an agent drafts the program from a prompt, read-only, the draft fills the form); `schemas/program.json` is the program's JSON Schema, checked before any other check on every save (tool, REST, seed), as `schemas/workflow.json` is for workflows | An agent's draft fills the form and saves; a body that breaks the schema is refused with the schema's own error |
| 3. Manager (**built** 2026-10-17; the daily dispatcher ships off) | `program-manager` profile, `program-review` workflow, proposals as todos | One week of NIA reviews read by the owner |
| 4. Agents (**done** 2026-10-17) | Agent rows in the scorecard; agent-tweak proposals | The manager proposes a profile change with evidence |

## 8. Decisions (owner, 2026-10-17)

1. **Tree:** as §4, except **Off-grid & decentralized networking is its own
   top-level program**, and **Accountability (the Trump-benefit monitor) stays
   under the NIA**.
2. **Pausing a program pauses its workflows**, generally speaking (a workflow
   may opt out; to settle in Phase 1).
3. **Programs get their own budgets**, like departments in a company: a
   monthly cap per program, inside the provider caps, enforced by the routing
   policy for the program's work. Cost goes on the scorecard first (Phase 0),
   so the caps are set from measured spend.
4. **An investigation may close itself** when its question is answered, but
   **closed is not deleted**: closed investigations are cold case files, kept
   whole and **re-openable** (by the owner, or a manager proposal the owner
   approves). Status: `active | paused | closed`, and a closed program keeps
   its history, notes and workflows (disabled).

## 9. Phase 1 as built (2026-10-17)

`services/programs.rs` (work crate), `manage_program` (`skills/program.rs`),
`/api/programs` (`clients/programs.rs`), seed kind `programs/`. Dev note:
"Programs own work; their status reaches their workflows".

- **The tree is seeded** as §4 with §8's changes: in the core bundle
  `brain-ops`, `ai-ml-research` and `shared-services` (kind `services`); in the
  private bundle `nia` with `critical-tech-dependencies`, `national-security`,
  `cybersecurity-privacy` and `accountability` under it, `jobsearch`,
  `off-grid-networking` (top level) and `briefings` (Briefings & Intake, which
  also holds `api-scout`). Every enabled workflow is in a program; the
  charters and success lines are first drafts for the owner to edit.
- **Membership lives on the program** (`workflows:`), as §2 planned: a personal
  program owns core workflows (the NIA owns the core claims monitor), which a
  workflow-side link could not do without core files naming personal programs.
  A seed file's names this brain lacks are dropped with a warning.
- **Pausing (§8.2):** a program that is not `active` disables its workflows and
  those of every program under it, with `paused_reason` `PROGRAM_PAUSE: program
  'x' is paused`; `keep_running` is the per-workflow opt-out. Active again
  resumes exactly those; a workflow paused for any other reason is never
  touched. Enabling such a workflow directly is refused, with the way out.
- **Closing (§8.4):** closing an investigation or case needs a `finding`. The
  brain on its own (a job, an agent run) may close an investigation or case,
  never standing work, and never reopens: it proposes that as a todo. A
  person (HBI, chat, an MCP client) decides any of it.
- **Budgets (§8.3):** `budget: {monthly_usd}` is stored and shown on
  `program_scorecard` next to the cost; the routing policy does not enforce
  it yet. Enforcement waits for a few weeks of measured cost per program.
- **Status is graph-owned**, like a workflow's `enabled`: a status change
  keeps a seeded program yaml-owned; any other edit makes it runtime-owned,
  and `reset` puts the file back.
- **Todos** carry `program`, checked to exist, and an item a workflow run makes
  is stamped with that workflow's program.
- **`program_scorecard`** (lookup view) rolls the workflow scorecard up each
  program's subtree, with its budget.

## 10. Phase 2 as built (2026-10-17)

- **Work › Programs** (`#/work?view=programs`, `ProgramsLane.tsx`): the tree
  as a table, each row with its scorecard over 7, 30 or 90 days (workflows
  on/owned, runs and failures, notes, reads, cited, cost, budget); a parent's
  row counts the programs under it. Enabled workflows in no program are
  listed under the table with links. "+ New program" opens the form.
- **The program drawer** (`?program=<name>`, `ProgramView.tsx`): charter,
  success, finding, who owns the definition (file or edited here), the
  30-day scorecard, the programs under it, its workflows with their state and
  pause reasons (each opens the workflow), topics, agents, APIs and open
  todos. Actions: Edit, Pause/Resume, Close… (asks for the finding; required
  for an investigation or case), Reopen, Reset to file, Delete (disabled while
  programs sit under it).
- **The form** (`ProgramForm.tsx`) offers what exists: workflows (one taken
  by another program is shown and disabled), topics, agents, the parent
  (never itself or anything below it). The server's check still decides, and
  its refusal is shown as given.
- **One set of numbers:** `GET /api/programs/scorecard` runs the
  `program_scorecard` lookup view through the tool registry
  (`ToolRegistry::call`), so the HBI shows what the brain reads.

## 11. Phase 3 as built (2026-10-17)

- **`program-manager`** (`plugins/core/contexts/program-manager.yaml`):
  spawnable, read-only tools (`lookup`, `search_notes`, `recall_recent`), on
  Haiku 5.5 with free `ministral-14b-latest` as the fallback. It judges a
  program against its charter and success line and returns one JSON object:
  `verdict` (healthy · attention · failing), `summary`, `findings`, and at
  most 3 `proposals` {title, why, ask, target}. It changes nothing. In chat
  the front door sends "how is the NIA doing?" to it; it reads the
  `programs`, `program_scorecard` and `workflow_scorecard` views.
- **`program-review`** (input `program`) is a fixed chain; only the judgement
  is a model call: `manage_program review_context` (definition, workflows'
  state, the programs under it with their latest verdicts, its previous
  review, open todos, issues) → `lookup workflow_scorecard program=…` (its
  subtree's workflows, 30 days) → `run_agent program-manager` (read-only) →
  `manage_program record_review` (each proposal a todo for the owner, tagged
  with the program, an open one with the same title not filed again; the
  program stamped with verdict and summary) → `store_note` (a `reflection`,
  `source_context: program-review:<name>`).
- **`program-reviews`** (daily, **seeded off**): `manage_program
  start_due_reviews` starts a review for each active program whose
  `review.every` has passed (not `services`), deepest first, so a parent
  reads its children's fresh verdicts. Turning it on costs about a tenth of
  a cent per review (10 programs weekly).
- **Testing does not spend** (owner, 2026-10-17): the live test ran a
  runtime copy pinned to `ministral-14b-latest` via `run_agent`'s `model`,
  then deleted it. National Security came back `attention` (69 notes, 2
  reads) with two proposals; both todos were closed after the test.
- Found by the test: `lookup` refused the queue's `__owner_task_id` as an
  unknown slot (no workflow had used `lookup` as a step before); the queue's
  `__` stamps are now dropped before the slot check.

## 12. Phase 2b as built (2026-10-17)

- **`schemas/program.json`** (core bundle): the program's JSON Schema
  (`additionalProperties: false`, slug name, kind and status enums, text
  limits, unique name lists, `review.every` pattern, `budget.monthly_usd >= 0`).
  It is the first check everywhere: `programs::save` checks the merged
  candidate before parsing it, and `load_file` checks a seed or reset file
  the same way. `init-db` now loads the bundles' schemas too
  (`validation::load_bundle_schemas`), so the seed it runs is checked as a
  save is. What a schema cannot say (the parent exists and does not loop,
  the workflows exist and are free, the topics, agents and APIs exist, the
  status rules) is `programs::check`, next.
- **`POST /api/programs/check`** `{program, create}`: every check a save runs,
  nothing written (`programs::check_save`); **`GET /api/programs/schema`**
  serves the schema.
- **Agent assist in the program form** (new and edit), as the workflow form
  has it: `program-manager` by default (read-only; the model picker can pick
  a free one). The task (`lib/programDraft.ts`) carries the request, the form
  as it stands, the schema, the existing programs, the workflows in no
  program, the topics and the agents. The answer's JSON object goes through
  the check; a refusal goes back to the agent once with the errors; then it
  fills the form, saying whether Save will take it. Nothing is saved until
  the person presses Save.
- Measured on free `ministral-14b-latest`: asked for "an investigation under
  the NIA into who supplies HBM to Chinese AI labs, reviewed weekly, $2 a
  month", it drafted a program that passed the check on the first try.

## 13. Phase 4 as built (2026-10-17)

- Telemetry knows the agent: `ctx::CURRENT_AGENT` is scoped around a sub-agent
  run; `arg_checks.agent` records it, and `agent_runs` (DuckDB) records each
  run (agent, caller, model, ok, tool calls and failures, duration, task id).
  Both from 2026-10-17; `model_usage` (`subagent:<agent>`) covers 30 days.
- `agent_scorecard` view (slot `program`: agents it relies on or its
  workflows call via `run_agent`); `GET /api/programs/agents`; an "Agents its
  work uses" section in the program drawer.
- `program-review` gains an agents step; review_context adds each workflow's
  schedule and description and each agent's setup (model, fallbacks, tools),
  so a proposed profile change names the field and the new value.
- Program names `new`, `schema`, `check`, `scorecard`, `agents` are reserved.
- Free-model tests (Ministral): Brain Ops reviewed end to end; a temporary
  program relying on scribe and code-analyst got a profile-change proposal
  with numbers (done-when met), but it compared counts not rates and named
  no concrete change. The prompt now requires rates and a named change and
  the setups are in the context. Re-run: scribe first (7 of 11 calls
  failed), "scribe: add fallback model", with the rate.
- The re-run found a real bug: proposals were keyed by program and date, so
  a second review the same day had its proposals swallowed as "filed
  already" by the first's (the message showed the old title). Now keyed by
  the review run's task id.
