# HBI Redesign — from tabs to a dashboard

Status: **Phases 0–7 done, 2026-10-08. Second round (§9): Phase 8 done 2026-10-08; Phase 9 done 2026-10-09 (4,510 claims rewritten or checked; eval 0.880 / 0.780); Phase 10 done 2026-10-09 (a model per step, per `run_agent`, per chat turn; one picker, one assist widget); Phase 11 done 2026-10-09 (declared topic/kind, a traced 30-day topic mix, grouped lists; starting values await the user's review); Phases 12–13 planned, all §9.9 decisions taken.** All §7 decisions taken 2026-10-08 (the recommendations, plus proposals-only entity merges). Supersedes the layout in `HBI_PLAN.md` (Feb 2026), whose transport and SSE notes still hold.

## 1. Why

The HBI grew one tab per feature. Today it has **9 sidebar tabs**, plus a Settings modal, an Agents modal hidden in Chat, and a `ProposalsPanel` that nothing routes to. Panels that serve one job sit far apart, and several fetch the same data:

| Symptom | Evidence |
|---|---|
| One job, three places | Tasks, Todos and Scheduled are separate tabs. Todo → Task → Job lineage is not visible anywhere. |
| Knowledge split in two | Knowledge (list + reader) and Graph (force view) don't share a selection. Clicking a graph node can't open the reader. |
| Keyword-only search | `/api/notes?q=` is a `toLower(content) CONTAINS` scan over 23k notes. The hybrid search (`search_notes`) isn't reachable from the UI, and neither is lookup by ID. |
| Graph shows "recent", not "what we know" | `/api/graph` returns the 200 most recently *accessed* notes, plus entities and tasks. There is no domain layer, so no mind map is possible today. |
| Duplicate polling | `/api/queue/status` is fetched by Tasks, Logs **and** Architecture. `/api/scheduler/status` by Logs and Architecture. 5 files each define their own `apiFetch`/auth headers. |
| Logs mixes system and activity | LogsPanel shows recent tasks + queue + scheduler next to the tracing stream. |
| Architecture is hand-written and stale | `ArchitecturePanel.tsx` (1201 lines) hardcodes a `DETAILS` map (still describes OpenWebUI and stdio Claude Code as the primary clients) and its own colour palette. |
| Settings are scattered | Brain URL/key in the modal. Agents/profiles in a Chat modal, scheduler config in the Scheduled panel, scheduler start/stop in Architecture, model choice in the chat bar. |
| Style drift | 149 inline `style={{}}` in ScheduledTasksPanel and 75 in Architecture. Architecture has its own palette `C`. |
| Giant files | ScheduledTasksPanel 1910, ChatPanel 1242, ArchitecturePanel 1201 lines. |

## 2. Target information architecture

Five workspaces replace nine tabs. Each one is grouped by **the job it serves**. Inspection happens in a shared drawer, so the user never leaves what they are working on.

```
┌────┬─────────────────────────────────────────────────────────┬──────────────┐
│ ⌂  │                                                         │  DRAWER      │
│ 💬 │   WORKSPACE (panel)                                     │  (any entity │
│ ⚙︎ │   Home · Chat · Work · Knowledge · System               │  note / task │
│ 🧠 │                                                         │  job / todo /│
│ 🖥 │                                                         │  schedule /  │
│    │                                                         │  entity /    │
│ ── │                                                         │  topic)      │
│ ⚙  │                                                         │              │
└────┴─────────────────────────────────────────────────────────┴──────────────┘
  Ctrl-K: command palette (jump to a note by id, a task, a setting, a workspace)
```

| Workspace | Absorbs | Job it serves |
|---|---|---|
| **Home** (new, landing) | notification banner, the status cards from Architecture/Logs | "What is the brain doing, and does anything need me?" |
| **Chat** | Chat, plus a context dock | Talk to the brain and see what it used. |
| **Work** | Tasks, Todos, Scheduled, Jobs, Proposals | Everything the brain or the user intends to do: one-off, delayed and recurring. |
| **Knowledge** | Knowledge, Graph | Find, read and explore what the brain knows, by domain. |
| **System** | Logs (system part), Architecture, Tools | How the brain itself is built and how healthy it is. |
| **Settings** (full page, sectioned) | Settings modal, Agents modal, scheduler config, model/routing view | Configure. |

**Panel vs modal, decided.** Workspaces are panels. Looking at a thing opens the **right-hand drawer** (it can expand to full width for long editors such as the schedule step editor). Modals are only for confirmations. Every drawer state is a URL (`#/work?task=<id>`), so a notification, a chat citation or a log line can link straight to the thing.

### 2.1 Home — the dashboard

```
┌ Brain ───────────────┐┌ Needs you ────────────┐┌ Next up ─────────────────┐
│ ● scheduler running  ││ 3 agent messages      ││ 14:00 Daily news brief   │
│ queue 4 run · 12 wait││ 2 todos overdue       ││ 18:00 Claim verification │
│ 0 errors / 24h       ││ 1 approval pending    ││ Mon   Hardware tripwire  │
└──────────────────────┘└───────────────────────┘└──────────────────────────┘
┌ Domains: last 24h ────────────────────────────┐┌ Learned today ───────────┐
│ ▇▇▇▇ national-security   +41 notes  3 claims  ││ 212 notes · 38 claims    │
│ ▇▇   small models        +12        1 task    ││ 9 tasks done · 1 failed  │
│ ▇    off-grid networking +4                   ││ 4 videos watched         │
└───────────────────────────────────────────────┘└──────────────────────────┘
┌ LLM budget / providers ───┐┌ Recent failures ──────────────────────────────┐
```

Rule: **every number is a link to its rows** (count-then-drill). A tile with nothing to show says what it checked ("0 failures across 840 jobs in 24h"), never just blank.

### 2.2 Work — Tasks + Todos + Scheduled as one model

The brain already has a lineage. The UI should show it:

```
Todo (human intent, may carry a delayed prompt) ──▶ Task (goal) ──▶ Job chain ──▶ Notes produced
ScheduledTask (recurring) ─────────────────────────────────────────▶ Job chain ──▶ Notes produced
Proposal (brain suggests) ──accept──▶ Task / Todo / ScheduledTask
```

Layout: a list with lanes on the left and a detail drawer on the right.

- **Lanes:** `Inbox` (proposals, approvals, overdue todos) · `Todos` · `Active` (tasks in progress, running and parked jobs) · `Scheduled` · `History`.
- **Scheduled** gets a **timeline view** (next 24 h / 7 d) beside the list, so the user sees when things run and where they collide.
- **Detail drawer:** the item's lineage as a vertical chain (todo → task → each job step with status, attempts and output preview → notes it stored). Actions live here: retry/cancel job, edit/run-now schedule, convert todo → task.
- The 1910-line schedule editor moves into the drawer and is split (`StepEditor`, `StepList`, `ScheduleMeta`, `AiAssist`). Its scheduler-config block moves to Settings.
- Fits `TODO_DELAYED_PROMPT_PLAN.md`: a delayed-prompt todo is simply a todo whose lineage has a future task.

### 2.3 Knowledge — search, read and the mind map in one place

One search bar with a mode switch, and four views over the same result set and selection:

```
[ search…                         ] (Auto | Semantic | Keyword | ID)  [type ▾] [domain ▾]
 View:  List · Mind map · Neighbourhood · Timeline
```

- **Auto mode:** a UUID goes to exact lookup; a quoted phrase goes to keyword/BM25; anything else goes to hybrid semantic. Each result shows *why* it matched (`semantic 0.82`, `bm25`, `id`).
- Searches note content, entity names and topic names. A note opens in the drawer with related notes, the entities it mentions, its claims and corroboration, its provenance (`source_context`, source record) and the task that produced it.
- **Neighbourhood** is today's GraphPanel. It is seeded from the selection or the result set, not from "last 200 accessed".
- **Timeline:** notes over time for the current query or domain. It shows growth, bursts, and when a domain went quiet.
- **Mind map:** see §3.

### 2.4 System — the brain itself

Activity (tasks, jobs) leaves this workspace for Work. System is about the machine:

- **Health:** one aggregate call covering Neo4j, DuckDB, host Ollama (embeddings), the llama.cpp sidecar, sandboxes, Whisper and SearXNG reachability. Also the GPU lease, provider budgets and cooldowns (`/api/llm/budget`), provider gates and queue semaphores, and the scheduler state (running/sleeping, error budget).
- **Logs:** the tracing stream with level, **module/target** and text filters, paused/live toggle, and click-through to the job/task id in a line.
- **Architecture:** live and data-driven (§4).
- **Registry:** skills → tools (today's Tools panel), agents/profiles, chains, MCP servers, providers and models. Read-only here; edits go through Settings.

### 2.5 Chat, reconsidered

Chat stays the primary interaction. It gains the same drawer and links:

- A collapsible **context dock** per assistant turn: notes cited (open in drawer), tasks or schedules it created (open in Work), and the **tool trace receipt** of what actually ran (the anti-fabrication receipt already in the chat loop, made visible).
- The header shrinks to agent · model · tokens. The Agents editor moves to Settings → Agents, and the chat header keeps a picker.
- Notifications get an inbox on Home. The chat badge stays, and both read the one list in `api/notifications.ts`, as now.
- The session sidebar becomes collapsible, with search over session titles.

### 2.6 Settings, navigable

A full page with a left nav and a filter box, also reachable through Ctrl-K:

`Connection` (URL, key) · `Agents & profiles` (moved from Chat) · `Models & routing` (read-only registry, budget, which providers admit private work) · `Scheduler` (interval, sleep, idle ticks; start/stop, moved out of Architecture) · `Appearance` (theme, density) · `About` (brain version, build).

## 3. Domains and the mind map

**The user's view:** a mind map of every domain the brain is working on or keeping track of.

**What exists:** nothing that names a domain. Measured 2026-10-08:

- 23,078 notes, 8,513 entities, 90,514 `RELATES_TO` edges. Rendering raw nodes is not an option, so the map must be aggregated server-side and expanded progressively.
- `source_context` has **378 distinct values**, some of them free-text goals ("Investigate queue backlog: 102 jobs…"). It says which *process* wrote a note, not what the note is *about*, so it can't be the domain key.
- Entity types are inconsistent (`concept`/`Concept`, `organisation`/`organization`). They are fine as subtopic labels once normalised, but they don't work as domains.
- "Domain" already means a **web hostname** in the codebase (`SourceList.domains`, `corroborating_domains`). To avoid the collision the node is **`:Topic`**. The UI can still say "Domains".
- 16,236 / 23,078 notes carry an embedding on the note itself. Long notes embed through chunks, and the rest must be checked before assignment relies on embeddings. An unassignable note goes in a visible `unassigned` bucket. It is never dropped.

**Proposal: declared topics, assigned by embedding.** *(Changed in Phase 4b, measured: the local model assigns them. Embedding centroids reached F1 0.61 against hand labels, against 0.83 held-out for the classifier. See the Phase 4b outcome.)*

- `topics/*.yaml` seed dir (`TOPICS_DIR` in compose). Each file names a topic and gives a description, seed phrases, and optional links to the ScheduledTasks and SourceLists that watch it. Starting set from what the schedules already watch: national security / intelligence, semiconductors & AI hardware, batteries & materials, small/local models & ML research, off-grid / mesh networking, Michigan civic news, US politics, the brain itself (self-knowledge), the user's career. The user reviews this list. The `api-scout` profile's hardcoded "OUR INTERESTS" list could later read from it.
- `(:Note)-[:IN_TOPIC {score, method}]->(:Topic)`. Assignment compares the note embedding with the topic centroid (embedded seed phrases), above a threshold, and may give several topics. It runs at note write time (alongside entity extraction, async), plus a one-off backfill. Assignments are recomputed when a topic's YAML changes.
- A topic's **subtopics** are its most-mentioned normalised entities. **Bridges** are entities shared by two topics.
- **Suggested topics:** dense clusters in `unassigned` are offered to the user as candidate topics. They are never auto-created.

**The map: the force graph, organised by topic.**

It keeps today's force-directed graph (`react-force-graph`), including dragging nodes, and adds a topic layer plus a choice of what node size means. It is not a radial tree: a fixed hierarchy can't be dragged around, and it hides the cross-links that make the graph worth looking at.

> Today's graph is the **2D** build (`react-force-graph-2d`). `react-force-graph-3d` is the same author's sibling with the same props, node dragging included. Going 3D is a one-component swap plus a 2D/3D toggle, so it is an option rather than a rewrite. Text labels are harder to read in 3D, which is why 2D stays the default.

- **Topic hubs.** Each `:Topic` is a large anchor node. Notes link to their topics (`IN_TOPIC`), and entities are pulled toward the topics whose notes mention them, so the graph settles into one cluster per domain. An entity shared by two topics sits between them, and that bridge is the interesting part. Topic hubs can be pinned in place by dragging, and they stay where you leave them.
- **Size by** (a selector; one meaning at a time, always stated in the legend):
  - **Connections:** the node's degree (MENTIONS for entities; RELATES_TO + MENTIONS for notes). This shows what the brain's ideas hang off.
  - **Certainty:** a claim's status/corroboration tier (refuted < unverified < unclassified-only < established < primary). An entity's certainty is the corroborated share of the claims that mention it, shown with its *n*. Notes that aren't claims take a fixed size by note type (`semantic` > `inference` > `unsourced_synthesis`). Measured 2026-10-08: **3,816 of 4,446 claims (86%) are unverified**, so this view will mostly show small nodes. That is the honest picture, and it is exactly where the claim-verification schedule is behind.
  - **Activity:** notes added or accessed in the last 7 days.
- **Colour** is a separate channel from size: node kind (topic / entity / note / task), with the claim status ring on claims. Contradictions (`CONTRADICTED_BY`) draw as red edges.
- **Scale:** the graph never loads 23k notes. Opening the map loads topics plus their top entities by degree (≈ 300 nodes). Double-click expands a node's neighbourhood on demand, as Focus mode already does.
- **Noise out by default:** `url` entities (2,104 of 8,513) and `date` entities (500) are hidden from the map. URLs show as source links in the drawer instead.
- **Click** a topic → Knowledge filtered to that topic. **Click** an entity → its notes in the drawer.

### 3.1 Entity resolution: one thing, one node

The map is only as good as its entities, and today the same thing appears under several names. Measured 2026-10-08:

| Real entity | Nodes in the graph (mentions) |
|---|---|
| United States | `us` (137) · `united states` (60) · `u.s.` (52) · `usa` (13) · `america` (8): **5 nodes, 270 mentions** |
| Donald Trump | `trump` (207) · `donald trump` (149) · `president trump` (23) · `president donald trump` (2): **4 nodes, 381 mentions** |
| spelling only | `openai`/`open ai`, `j.d. vance`/`jd vance`, `us senate`/`u.s. senate`, `rfk jr.`/`rfk jr`, `deepfakes`/`deep fakes`, … |

There are also 86 single-word person names with a longer full-name node (`vance` / `jd vance`).

**Why it happens** (`services/knowledge/entities.rs`):

- The `MERGE (e:Entity {name})` key is the exact lowercased string, so any spelling difference creates a new node.
- The prompt asks the model to "merge obvious aliases". A per-note LLM call can't know which spelling earlier notes used, so it never could.
- The extractor lowercases before storing and discards the surface form, so `US` (the country) and `us` (the pronoun) become indistinguishable afterward. 122 of the 137 notes behind `us` contain a capital `US`/`U.S.`; the other ~15 are likely the pronoun.
- `entity_type` is set `ON CREATE` only, so the first note to mention a name fixes its type forever.
- The extractor parses its JSON first-`[`-to-last-`]`, which the structured-output rule forbids. It should go through `generate_json`.

**Design: canonical entities with aliases, resolved at write time.**

1. **Model.** A canonical `(:Entity {name, entity_type})` gains `aliases: [..]` (indexed). A merged-away name keeps an `(:EntityAlias {name})-[:ALIAS_OF]->(:Entity)` node, and each repointed `MENTIONS` keeps `surface: "us"`. A merge can therefore be undone by surface form, and nothing is lost.
2. **Normalise before lookup** (deterministic, no LLM): strip periods and spacing variants (`u.s.` → `us`, `open ai` → `openai`), and strip leading titles held in data (`president`, `sen.`, `dr.`, …).
3. **Resolve against the alias index.** A hit writes to the canonical entity, and a miss creates a new one. This replaces the bare `MERGE {name}`.
4. **Keep the surface form.** The extractor returns the text as written, and the code lowercases *after* resolution. That way an uppercase `US`/`U.S.` resolves to United States and a lowercase `us` doesn't.
5. **Curated seed:** an `entities/aliases.yaml` seed dir (`ENTITY_ALIASES_DIR` in compose), not Rust literals. Example: `united states: [us, u.s., usa, u.s.a., america]`, `donald trump: [trump, president trump, president donald trump]`, plus the title list and type normalisation (`organization` → `organisation`, `Concept` → `concept`).
6. **Candidate discovery for the long tail.** These are proposals, never auto-merges: punctuation/space collisions, a single name that is the last token of a full person name, acronyms, and entity-name embedding similarity confirmed by co-mention overlap. Candidates go to the Work **Inbox** as "merge `vance` into `jd vance`? (31 shared notes)", with accept / reject / "different thing". A rejection is remembered so it isn't asked again.
7. **Related ≠ same.** `trump administration` is an organisation linked to Donald Trump, not merged into him, and the same goes for `donald trump jr.`. The merge UI offers "related to" as a third answer, which writes `RELATED_ENTITY`.
8. **Backfill** applies the curated seed to existing nodes. For the ambiguous case (`us`), it repoints a mention only when the note's text has the uppercase form, and leaves the rest as surface `us` for review. Run it after a snapshot.
9. **Measure it.** Entity expansion feeds retrieval (the MENTIONS bridge). Run `eval-retrieval` before and after the backfill, and record the numbers in `engineering-history.md`. `SnapshotService` restore (which MERGEs entities by name) must carry `aliases` and `EntityAlias` too.

## 4. A living architecture view

**The ask:** the brain updates its own architecture diagram as part of an existing scheduled task, perhaps by editing the React components.

**Recommendation: render architecture from data the brain keeps current, not from code the brain writes.** It gets the same result (the diagram tracks the code) without breaking three standing rules:

1. `write_codebase_doc` refuses anything that isn't `.md`. That is deliberate: no brain-written source code.
2. Model-written code goes only on `feature/*` worktrees and is human-signed (`FINE_TUNING_PLAN.md`).
3. `doc-update` was rebuilt with no LLM (2026-06-12) after the local model couldn't produce grounded doc content. A model hand-editing TSX layout is the same failure in a harder medium.

**Design:**

- `GET /api/architecture` is built **live and deterministically** from what the brain already knows about itself: the skill registry (skills → tools), `providers:` + `:ModelDef`, `:ContextProfile` agents, `:ScheduledTask`, `:SchedulerChain`, `:McpServer`, the `:ApiEndpoint` graph that `brain-api-sync` regenerates weekly from the Axum routes, and the seed dirs. Because it is computed, it cannot drift.
- `architecture/layout.yaml` (human-curated) holds only what can't be derived: groups, the container/network topology from compose, and one-line blurbs. Anything the live data has and the layout doesn't is drawn in an "unplaced" group, which keeps the gap visible.
- **The brain's part**, as a new step on the existing weekly `brain-api-sync` schedule: `snapshot_architecture` stores an `:ArchSnapshot` (counts + member lists) and diffs it against the last one. The diff is deterministic: tools added/removed, routes, schedules, agents. Optionally a one-shot local summary of *that diff* is written. One-shot synthesis is within E4B's range, and the summary is stored with provenance and labelled model-written. The frontend shows "what changed this week" from it.
- Later, if wanted, the brain could propose `layout.yaml` placements for unplaced components as a **proposal** in the Work inbox, never a direct write.

**Doc-update, measured:** it ran today (13:34 UTC, next 01:34 UTC), and `STATUS.md`'s auto section is current to the last commit before that run. But what it does is narrower than "reads its own codebase and updates documentation". It renders the last 15 commit subjects into one section, with no LLM. A real performance review of the brain's self-documentation (which docs drift, and whether the `codebase_self_analysis` notes are any good) is a separate item, listed in §7.

## 5. Backend work the frontend needs

| # | Endpoint / change | Notes |
|---|---|---|
| B0 | `GET /api/activity?hours=`; `since` on `/api/notes`, `/api/tasks`, `/api/jobs`; `verified_since` on notes; comma `status` lists; `version` in `/api/health` | **Done in Phase 1.** Home's counts and the rows behind them. |
| B1 | **Done in Phase 3** (with `/api/lookup/{id}`, `/api/entities/{id}`, `/api/graph?seeds=`, and the note detail fields). Planned as: `GET /api/search?q=&mode=auto\|semantic\|keyword\|id&kind=note\|entity\|topic&topic=&limit=` | Semantic goes through the **non-perturbing** `search_notes_inner(track_access=false)` path: a human browsing must not skew freshness/access-count retrieval signals. Keyword uses the BM25 fulltext index, not `CONTAINS`. Each hit returns `match_reason` + score. A truncated result says so. |
| B2 | `:Topic` + `topics/*.yaml` + `TOPICS_DIR` compose env; `IN_TOPIC` assignment + backfill; `GET /api/topics`, `GET /api/topics/{id}/map` | §3. Schema in `init_schema`, documented in `schema.md` (`IN_TOPIC.assigned_at` is a native `DATETIME`). |
| B2b | Entity resolution (§3.1): normalise + alias index at write time; `entities/` seed dir + `ENTITY_ALIASES_DIR`; extractor keeps surface form and uses `generate_json`; merge-candidate proposals; reversible merge/backfill; `POST /api/entities/merge` | Before the map, not after. Retrieval eval before and after. |
| B3 | **Done in Phase 2** as `GET /api/work/{tasks,jobs,schedules,todos}/{id}`, `GET /api/work/schedules`, `POST /api/work/schedules/{id}/run`, `POST /api/work/todos/{id}/task`. Planned as: `GET /api/work/{kind}/{id}/lineage`; `next_run_at` + last outcome in the schedule list; a todo ↔ task link | Lineage = todo/schedule → tasks → jobs (steps, attempts) → notes (`source_context`/job id). |
| B4 | `GET /api/system/health` (one aggregate); `/api/logs` gains `target` and `q` filters, optionally SSE | Replaces three panels polling the same status endpoints. |
| B5 | `GET /api/architecture`; `architecture/layout.yaml`; `snapshot_architecture` tool + step on `brain-api-sync` | §4. The tool is deterministic. The optional summary step is `provider_hint: ollama` at the step's top level. |

## 6. Phases

Each phase ships on its own and leaves the HBI working. Old panels are **mounted inside the new workspaces** until replaced (strangler pattern), so nothing becomes unreachable mid-migration.

| Phase | Scope | Done when |
|---|---|---|
| **0. Foundations** | One typed `api/` client (single `apiFetch`, auth, errors); a shared polling cache, so a status endpoint is fetched once however many views show it (TanStack Query recommended); hash router + deep links; CSS design tokens (retire `C` and most inline styles in touched files); a small kit (`Card`, `Stat`, `Pill`, `Tabs`, `Drawer`, `EmptyState` that names what it checked); the global entity drawer. Fold `ProposalsPanel` into the plan for Work, or delete it. | `npm run build` + lint clean; every existing panel still works; a `#/…` URL opens a note in the drawer. |
| **1. Shell + Home + Settings** | 5-workspace nav; Home dashboard (on existing endpoints first); Settings page with sections; Ctrl-K palette. Old tabs are reachable as sub-tabs. | Home tiles all drill to rows; every setting is reachable from Settings or Ctrl-K. |
| **2. Work** | B3; lanes; lineage drawer; schedule timeline; split the schedule editor. | One place answers "what's running, what's next, what came of it". Tasks/Todos/Scheduled tabs removed. |
| **3. Knowledge search** | B1; unified search bar; List + Neighbourhood + Timeline views sharing one selection. | A semantic query, a keyword phrase and a pasted note id all find the right note. Knowledge/Graph tabs removed. |
| **4a. Entity resolution** | B2b; curated alias seed reviewed by the user; backfill after a snapshot; merge candidates in the Work Inbox. | `us`/`united states`/`u.s.`/… and `trump`/`donald trump`/… are one node each; `eval-retrieval` is no worse; every merge can be undone. |
| **4b. Topics + map** | B2; user reviews the starting `topics/*.yaml`; backfill; force graph with topic hubs, size-by selector (connections / certainty / activity), url/date entities hidden; optional 3D toggle; Home's domain tile goes live. | Every note is in a topic or in a visible `unassigned` count; the map loads in < 2 s and stays draggable. |
| **5. System** | B4; Health, Logs (system-only), Registry. | Logs shows no task/queue activity; one call drives Health. |
| **6. Living architecture** | B5; data-driven diagram replaces `ArchitecturePanel`'s hardcoded map; weekly diff on Home/System. | Adding a tool or schedule shows up without a frontend change. |
| **7. Chat** | Context dock (citations, created work, tool-trace receipt); header slimmed; Agents moved to Settings. | Every citation and created task in a turn opens in one click. |

### Phase 0 outcome (2026-10-08)

Built: `api/client.ts` (all ~45 hand-rolled fetches and 5 duplicate helpers now go through it), `api/queries.ts` (TanStack Query; queue and scheduler status shared by Tasks, Logs and Architecture, with one app-level job-push subscription replacing one per card), `router.ts` (hash routing; tabs, Back/Forward and deep links), `components/ui` (Pill, Card, Stat, Tabs, EmptyState, Drawer) + `styles/ui.css`, and the entity drawer for notes (content, related notes, copy link). The Graph's note detail gained **Open**, and Knowledge search treats a pasted note id as a lookup. 33 vitest tests, lint and build clean; checked in headless Chrome against the live brain (deep link, related, not-found, Esc, id search, unknown route, Back, every panel loads with no console errors).

Found on the way:
- Four CSS variables were used but never defined, so queued/created status badges and code blocks rendered with no colour. They are aliases now.
- `src/index.css` was never imported (a scaffold leftover); deleted.
- `GET /api/notes/{id}` returns only id/content/type/access count/created. Provenance, `source_context` and claim fields reach the drawer with B1 (Phase 3).
- `/api/notes/{id}/related` silently caps at 10 outgoing edges; the drawer now says so.
- The schedule editor posts raw JSON-RPC to `/mcp`, bypassing the SDK client. Its behaviour is kept for now and revisited when the editor is split (Phase 2).
- Not changed: ArchitecturePanel's palette (replaced in Phase 6), and `ProposalsPanel` (unrouted but working; it becomes the Work Inbox in Phase 2). Architecture's scheduler card shows `paused` whenever `is_running` is false, including while sleeping (a Phase 5 check).

### Phase 1 outcome (2026-10-08)

Built:
- **Shell:** five workspaces plus Settings (`src/workspaces.ts`). Each workspace shows its views as tabs, and the old panels are mounted inside them. Old tab URLs redirect to their new home, keeping their params.
- **Home:** five tiles (Brain, Needs you, Next up, Last 24 hours, LLM). Every number links to its rows through URL drill-downs, and a drilled view shows a banner with Show all.
- **Settings:** a full page (`#/settings?section=`) with five sections:
  - Connection, with a live check.
  - Agents: the Chat editor rendered inline, sharing one profile query with Chat.
  - Models & routing: read-only, from `/api/llm/budget`, including per-model cooldowns and spend against caps.
  - Scheduler: start/pause, run a tick, and the five config fields, saving only the ones changed.
  - About: the running commit and the bundle's build time.
- **Ctrl-K palette.**
- **Backend:** B0 above. Checked live against the rebuilt brain; every drill returned exactly its count.
- **Tests:** 51 vitest tests and 7 new Rust tests (783 lib tests pass).

Fixed on the way:
- **Cypher injection:** `/api/tasks` and `/api/jobs` formatted `?status=` into Cypher. Now bound.
- **Dead jobs hidden:** the "Failed / Dead" job filter never matched `dead`/`dead_letter` (showed 0 of 39), and `/api/health`'s dead-job alert ignored `dead_letter`.
- **Active filter:** "Active" filtered a latest-50 page in the browser, hiding older parked jobs.
- **Scheduler card:** the Architecture card read `is_running` as on/off, so it showed a sleeping scheduler as paused with a Start button.
- **Silent model overwrite:** the schedule editor's settings block sent `local_model` (the stale `OLLAMA_LOCAL_MODEL` default) on every save, which writes the live background model. The block is gone, and Settings doesn't offer the field.
- **Knowledge type filter:** it offered 4 of 13 note types.

Deliberately not built: an **Appearance** section (there is only one theme; nothing to configure yet). The **domains** tile waits for Phase 4.

### Phase 2 outcome (2026-10-08)

Built:
- **Work lanes:** Inbox · Active · Scheduled (list, a 24 h / 7 d timeline with a load strip, routing chains) · Todos · History (tasks or the job log).
- **Drawer views** for tasks, jobs, schedules, todos and proposals:
  - Task lineage: where it came from → each chain step (status, attempts, time, output) → the notes and tasks it produced.
  - Job: full arguments and output, its owning task and chain, cancel/retry.
  - Schedule: cadence, runs, steps; run now, turn on/off, edit, delete.
  - Todo: mark done, edit, hand to the brain, delete.
  - Proposal: read, dismiss with a reason.
- **Lineage backend:** B3 above, with the chain reconstructed from what is already recorded (`clients/work.rs`), plus `run_scheduled_task_now`.
- **Editors:** the schedule and todo editors are split into modules and render embedded in the drawer.
- **Tests:** 62 vitest tests and 9 new Rust tests (791 lib tests).
- **Checked live:** a news-analysis run shows its 6 steps, the `semantic` note its `store_note` produced, and the task its `create_task` made. Run now on the SELF-TEST schedule dispatched a run that failed as designed: the first job dead-lettered and the rest were cancelled.

Fixed on the way:
- **Tool errors read as success.** Action buttons ignored a tool's `isError`, so a refusal looked like success. `callToolStrict` now throws. This also covers Settings → Scheduler.
- **Dead-letter retry was impossible.** Retrying a `dead_letter` job through `manage_job` always failed; it now goes through `dead_letter`.
- **The step editor's Model field did nothing.** It wrote a per-step `model` that the runner drops, and its provider list lacked `ollama-cloud`.
- **Overdue disagreed by timezone.** It is now one date-based rule.
- **Old-format steps were hidden.** The list showed tool names for steps the runner reads as empty; they are now flagged as broken.

Found, not fixed (needs a decision):
- **Garbage research tasks.** `schedules/news-analysis.yaml` step 5 is `create_task goal: "{{_prev}}"`. When the preceding `reason` finds no research topic, its answer becomes the goal. 10 tasks so far have the goal "No section titled "Priority Research Topic" was found in the provided context."
- **Summary JSON failures.** Most dead-lettered jobs this week are `ingest_article`/`ingest_media` summaries where the model "failed to produce valid JSON after 3 attempts".
- **Daily schedules collide.** The timeline shows six daily schedules firing together around 20:22 local.

### Phase 3 outcome (2026-10-08)

Built:
- **Search:** `/api/search` in auto, semantic, keyword and id modes, each hit carrying a match trace, plus id lookup, entity detail, a seeded graph, and richer note detail.
- **Knowledge workspace:** Results, Neighbourhood and Timeline, sharing one query and one selection.
- **Drawer:** the note view gains provenance, claims, chunk parent, mentions, edit/delete and create; a new entity view.
- **Ctrl-K:** resolves a pasted id's kind before opening it.
- **Tests:** 63 vitest tests and 6 new Rust tests (797 lib tests).
- **Done criterion met, checked live:**
  - A semantic query found a newly written note at #1.
  - A `"strait of hormuz"` phrase search ran as keyword, and its entity chip opened the entity (51 mentions, co-mentioned with iran, trump, …).
  - A pasted task id opened the task from both the search bar and Ctrl-K.
- **Retrieval unchanged:** `eval-retrieval` identical before and after (0.920 / 0.757, all 25 cases).

Fixed on the way:
- **Notes written in the HBI were never embedded, linked or entity-scanned.** `POST /api/notes` was a raw `CREATE`, so semantic search could not find them. The old comment said maintenance would embed them later; no such backfill covers them.
- **An edit left the old embedding, links and chunks in place.** `PUT /api/notes/{id}` was a bare `SET content`, so everything derived from the text still described the old version.
- **Keyword search matched any word, not all of them.** Lucene's default OR meant "sodium batteries" matched every note saying "batteries". Bare words are now all required.
- **The type filter only searched what was already loaded.** It offered 4 types and filtered a 50-note page in the browser; the type now goes to the server.

Found, not fixed: the trace makes a retrieval quirk visible. Freshness plus BM25 lifts Todo Snapshot notes above the best semantic match for topic queries (see subsystem-rules). Whether snapshots should be down-weighted or excluded from topic retrieval is a retrieval-tuning decision, and it should be measured with `eval-retrieval`.

### Phase 4a outcome (2026-10-08)

Built:
- **Resolution at write time** (`services/entity_resolution.rs`). Each extracted name resolves through alias → exact name → normalised key before anything is created. The extractor now asks for names as written and goes through `generate_json`. Mentions keep `surface`, `via` and `type`. The entity type is the majority of its mentions. `location` joins the type list.
- **The curated seed** is `entities/aliases.yaml` (`ENTITY_ALIASES_DIR`). It holds 14 canonicals (United States, Donald Trump, UK, EU, UN, DoJ, DoD with Department of War, DHS, ICE, AI, LLM, RAG, MCP, GOP), the title and article lists, type spellings, and `keep_distinct`. `us`, `un` and `ice` are case-sensitive.
- **Sync on every start** (`init-db`): seed, keys, then the automatic merges after a `pre_entity_merge` snapshot. `agent-brain resolve-entities [--dry-run] [--unmerge NAME]` runs it by hand.
- **Proposals** for everything else, ranked by shared notes: last name, acronym, title + surname, plural. They appear in the **Work Inbox** ("Same thing? Entity merges") and in the entity drawer ("Maybe the same as"), with Merge, ⇄, Different (`NOT_SAME`) and Related (`RELATED_ENTITY`). The drawer also offers "Merge into…" for any pair, and lists the aliases with **Undo**.
- **Endpoints:** `GET /api/entities/candidates`, `POST /api/entities/merge`, `POST /api/entities/decide`, `POST /api/entities/aliases/{name}/unmerge`. The entity detail endpoint adds aliases, and an id that was merged away answers with where it went. Search and `search_by_entity` match aliases.
- **Snapshots** carry `key`, `type_source`, `distinct` and the aliases. Prune keeps canonicals.
- **Tests:** 16 Rust unit tests (813 lib tests) and 65 vitest tests. Merge, case-sensitive partial merge, nested undo, the write-path Cypher and every HTTP endpoint ran against a throwaway Neo4j, and the HBI was driven through merge → alias → undo in headless Chrome against it.

**Live dry-run (read-only, 2026-10-08):** 91 merges, 24 from the seed and 67 same-spelling. The first dry-run planned 108, and reading them is what changed the title rule. `senator johnson` and `johnson` were heading into `speaker johnson` (different people), `bush` into `president bush`, `president bio` into `bio`, and `911` into `9/11`. Titles now fold only before a full name, all-digit keys never fold, and title + surname is a proposal. Retrieval baseline just before: recall@10 0.920, MRR 0.757.

**Applied (commit 762f246):** the `pre_entity_merge` snapshot was taken, then all 91 merges went in with 0 failures. Entities went from 8,575 to 8,487, with 100 aliases. `donald trump` has 368 mentions and `united states` 267. **Done criterion met:** each is one node, `eval-retrieval` is unchanged (0.920 / 0.757, all 25 cases identical), and every merge can be undone from its alias. 188 merge proposals are open in the Inbox.

### Phase 4b outcome (2026-10-08)

Built:
- **Topics:** `topics/*.yaml` (`TOPICS_DIR`), 11 starting topics for the user to review: national security, geopolitics, US politics, AI/ML, semiconductors, batteries & materials, off-grid networking, Michigan, cybersecurity, the brain itself, career. `topics/_scope.yaml` skips activity records. `init-db` syncs `(:Topic)`.
- **Assignment by the local model, not embeddings** (`services/topics.rs`). Measured first, on the live graph and hand labels:
  - *Embedding centroids:* best rule F1 0.61, precision 0.53. Z-scores and re-centring did no better.
  - *The classifier:* F1 0.83 on a held-out set (precision 0.77, recall 0.91).
  - *Unexpected gain:* the 6,878 unembedded notes get topics too.
  - *How it writes:* main topic first, a second only if as central, as `IN_TOPIC {rank}`.
  - *When it runs:* an idle-time assigner in `serve` (no job queued or running, no GPU lease), about 0.3 s per note.
- **Coverage:** every top-level note is in a topic, fits none, waiting, failed or skipped, and `GET /api/topics` returns the five counts, which sum to the total.
- **Endpoints:** `GET /api/topics` (counts, coverage, what the no-topic notes mention), `GET /api/topics/map` (hubs, the top entities per topic, size metrics) and `GET /api/topics/{name}`. `/api/search?topic=` narrows a search, or with no query lists the topic's newest notes. Note detail and search results carry their topics.
- **HBI:**
  - *Map:* Knowledge **Map** view, the force graph with topic hubs. Entities take the colour of their strongest topic, and bridges get a ring. Size by connections / certainty / activity, stated in the legend. 30 labels at rest, the rest on zoom. Drag to pin. Click to open, double-click an entity for its neighbourhood. Fits once on load. urls and dates hidden.
  - *Topic drawer:* definition, counts, schedules watching it, top entities, newest notes, "Search within".
  - *Filter and chips:* a topic picker (`within=`), and topic chips on results and in the note drawer.
  - *Home:* the **Domains** tile went live.
- **Tests:** 4 Rust unit tests (817 lib tests) and 4 vitest tests (69). End to end on a throwaway Neo4j loaded with a 192-note slice of the live graph, with the assigner calling the real llama.cpp sidecar: full coverage, the map in 0.16 s, and every view driven in headless Chrome.

Fixed on the way:
- **Broken embeddings shown as embedded.** The note drawer called 223 notes with an *empty* embedding "embedded" because it tested `IS NOT NULL`.
- **A Cypher null trap in the first skip filter.** Notes without a `source_context` fell out of every coverage bucket. The sum check exposed it.

Applied live (the backfill finished 2026-10-08 19:38, no failures): 9,892 notes in a topic, 3,703 fitting none, 3,750 skipped by scope, 0 waiting, summing to all 17,345. New notes are classified as they arrive. The live map loads in 0.07 s.

**Done criterion met:** every note is in a topic or in a visible count, and the map loads well under 2 s and stays draggable.

Not built (planned as optional): suggested topics from clusters of no-topic notes (for now the tile and endpoint list what those notes mention most), and the 3D toggle.

### Phase 5 outcome (2026-10-08)

Built:
- **B4:**
  - `GET /api/system/health` is one call. It probes all 8 services in `architecture/components.yaml` (Neo4j, DuckDB, host Ollama, llama.cpp, both sandboxes, Whisper, SearXNG) in parallel, adds the scheduler, queue, GPU lease, provider budgets and version, and returns `issues` worst first. Measured at 0.17 s for everything.
  - `GET /api/system/registry` lists skills and tools, dynamic tools, agents, chains, MCP servers (whether each secret is set), providers and components.
  - `/api/logs` gains `target` (prefixes, `-prefix` drops), `quiet`, `q` and `after`, plus `matched`/`buffered`/`oldest`. The buffer grew from 500 to 2,000 lines.
- **System workspace:**
  - *Health (new default):* a verdict banner, issues, services with status, latency and purpose, then the scheduler, the queue with per-provider slots and the GPU, providers, and the running version.
  - *Logs (rewritten):* level, module and text filters, "hide routine activity" on by default (warnings and errors always show), live or paused, a click on a module filters to it, and ids in a line open in the drawer. The old panel's job-queue and recent-tasks sections are gone: that is Work.
  - *Registry:* replaces Tools. Counts first, then tabs for Tools (the existing explorer), Agents, Chains, MCP servers, Providers and Services. `view=tools` redirects.
- **Tests:** 8 Rust unit tests (825 lib tests) and 1 vitest test (70). Checked on a scratch server against the real sidecars, and driven in headless Chrome.

**Done criterion met:** one call drives Health, and Logs show no task or queue activity by default (only their warnings and errors).

### Phase 6 outcome (2026-10-08)

Built:
- **B5, computed rather than written** (`services/architecture.rs`):
  - *Inventory:* skills and their tools from `(:ToolDef)` (re-synced from the registry at every start), agents, chains, schedules, MCP servers, dynamic tools, providers and the `components.yaml` services. HTTP routes are **parsed from the router source** (`mcp/http_transport.rs`, compiled in with `include_str!`), each with its handler module, so a new route can't be missed. The weekly `ingest_api` endpoint graph is LLM-extracted and had only part of them.
  - *`architecture/layout.yaml`:* the only hand-written part. Groups (selectors such as `skill:Query`, `routes:api/*`, `provider:*`; an exact selector beats a wildcard), the edges between groups, and the container and network topology. A live item no group claims is returned as **unplaced**, and a selector matching nothing as **stale**. A unit test fails when the topology disagrees with `docker-compose.yml` (containers, networks, published ports), and another when a route area is unplaced.
  - *`GET /api/architecture`:* the groups filled with live nodes, unplaced, stale, edges, topology, counts, **what changed since the newest snapshot** (the live inventory against the stored one, so it is never stale) and the last 12 snapshots.
  - *`snapshot_architecture` tool* (Query skill): stores an `(:ArchSnapshot)` with its diff against the previous one; `dry_run` reports without storing. Set arithmetic, no model. It is the first step of the weekly `brain-api-sync`, followed by a note of the diff (`source_context: architecture_snapshot`), so "what changed this week" is answerable from memory too. The brain takes a baseline snapshot at its first start, so the view has something to compare with from day one.
- **System → Architecture** (`ArchitectureView`) replaces the 1,131-line `ArchitecturePanel` and its hardcoded map ("47 tools · 15 skills" against a real 98 and 23):
  - *Diagram:* columns of groups, edges measured from the rendered cards, live status dots on services and providers. Click any box for the rows behind it: a skill's tools, an area's routes (the Routes count opens all 75), a collection's names, a service's live status. Long groups fold to 10 with "+N more".
  - *Containers:* a container × network grid with published ports and live status, showing at a glance that the sandboxes sit only on the internal network.
  - *Snapshots:* each with its diff, plus the live change since the newest one.
  - A banner says what changed since the last snapshot, and lists unplaced items and stale selectors.
- **Home:** the Brain tile says what changed since the last snapshot. **Health:** the Queue card gains **Drain queue**, the one control the old panel had that nothing else did.
- **Tests:** 9 Rust unit tests (834 lib tests) and 2 vitest tests (72). On a throwaway Neo4j with the real seed dirs: the baseline at first start; a new skill, a new tool and a deleted schedule all showed in the live diff and the tool's diff, and the new skill as unplaced; then every view driven in headless Chrome.

Dropped rather than moved: the old panel's **integrity card** called `verify_knowledge_integrity`, which no longer exists, so it could only ever fail.

Not built: the optional model-written summary of the weekly diff. The deterministic one-line summary reads well enough, and a model adds a way to be wrong. The brain proposing `layout.yaml` placements for unplaced items is also left for later.

**Done criterion met:** a new tool or schedule shows up in the diagram, the counts and the diff with no frontend change, and a new skill shows up as unplaced until `layout.yaml` places it.

### Phase 7 outcome (2026-10-08)

Built:
- **Context dock under every answer** (`components/chat/ContextDock.tsx`, derived in `lib/chatDock.ts` from the turn's own SSE events, no model involved). A one-line summary ("3 tools ran · wrote 1 note · cites 1 item · 1 link · 1 not from a tool") opens to:
  - *Ran, in order:* each tool with ✓/✗ and its arguments, including a sub-agent's tools (`knowledge-worker:list_capabilities`). Flagged where the model was given less of a result than exists ("model read 2,253 of 8,400 chars"). This is the chat loop's tool-trace receipt, made visible: anything the answer claims that is not listed did not happen.
  - *Wrote:* the ids in the results of tools that write (`store_*`, `create_*`, `manage_*` with a writing action, …).
  - *Cited in the answer:* full ids and the short `note:7d75fff5` form. An id the answer names that resolves to nothing is shown as such.
  - *Read:* ids from every other tool, resolved when opened, with the cited ones marked.
  - *Links:* every URL in the answer, marked by which tool returned it this turn, or **"not in any tool result"**: a link from the model's memory is visible as one.
  - Every item opens in the drawer in one click. A turn restored from history keeps only its text, so its dock shows what the answer names, marked unchecked.
- **`POST /api/lookup`** resolves up to 60 ids or 8+ character prefixes in one call (a prefix only when exactly one node has it), returning `found` and `missing`.
- **Header slimmed to agent · model · tokens:** an agent picker, the model (the profile's pinned model when it has one), 24 h tokens with the per-model breakdown in the tooltip and unmetered models named, then "edit agents →" and Export. The agent editor modal is gone from Chat; it lives in Settings → Agents (`components/settings/AgentsEditor.tsx`). The last choice is remembered per browser.
- **Session sidebar:** collapsible (remembered), with search over titles, session ids and task goals. A search that matches nothing says so.
- Notifications already had their Home inbox (the Needs you tile, since Phase 1), reading the one list in `api/notifications.ts`.
- **Tests:** 1 Rust unit test (835 lib tests), 6 vitest tests (78). End to end on a throwaway Neo4j with a real cloud model: a `list_capabilities` → `store_note` turn produced the dock above, the note opened in the drawer, and a turn naming no agent ran under the front door.

Fixed on the way:
- **HBI chats bypassed the front door.** The picker defaulted to a hardcoded `"general"` and always sent it, so `CHAT_DEFAULT_PROFILE` (`GeneralDefault-Orchrestator`, which routes through `delegate`) never applied to an HBI chat unless picked by hand. The picker now defaults to sending no profile. `/api/contexts` returns `default_profile`: the front door, or `null` when none is set and the server picks per message, labelled "auto (chosen per message)".
- 160 lines of CSS for the old settings bar, now unused, deleted.

Not built: keeping a turn's tool events with the session, so a restored turn's dock could be checked too. Working memory stores text only; that would be a schema change, not a UI one.

**Done criterion met:** every citation and created item in a turn opens in one click.

Order rationale: 0–1 make everything after them cheap. Work (2) is pure frontend plus a small endpoint. Search (3) unblocks the mind map's drill-down. Entity resolution (4a) comes before topics (4b) because the map's clusters and bridges are made of entities: with duplicates, one person shows up as four weak nodes instead of one strong one. Chat (7) depends on the drawer, Work and Knowledge being in place.

## 7. Decisions (taken 2026-10-08: every recommendation accepted)

1. **Topics: declared or emergent?** *Recommend declared YAML + suggested clusters.* Emergent-only clustering gives unstable, unnamed domains that shift on every re-run.
2. **Work layout: list + drawer or kanban?** *Recommend list + drawer with lanes.* Kanban fits tasks but not schedules or job chains.
3. **Add TanStack Query?** *Recommend yes.* It is small and removes the duplicate-polling class of bug. (`d3-hierarchy` dropped 2026-10-08: the map stays a force graph.)
4. **Architecture: data-driven (§4) or brain-authored TSX on a `feature/*` branch for review?** *Recommend data-driven.*
5. **Separate follow-up:** a performance review of the brain's self-documentation (doc-update scope, `codebase_self_analysis` note quality). It is out of scope here.
6. **Entity merges: proposals only, or auto-merge above a confidence threshold?** *Recommend proposals only* for anything outside the curated seed. A wrong merge silently corrupts claims and retrieval, and undoing it later is harder than asking now.

## 8. Testing

There are no frontend tests today. Phase 0 adds `vitest` for the `api/` layer and pure helpers (search-mode detection, lineage assembly). Each phase is checked in the running HBI (port 3002, rebuilt via compose) against the live brain, not just by `npm run build`. Backend endpoints get unit tests and are documented in `README` / `subsystem-rules.md` when they land.

## 9. Second round: Phases 8–13 (planned 2026-10-08)

Source: the user's HBI todos of 2026-10-08 (group schedules by topic, collapse the sidebar, ambiguous claim notes, merge Inbox/Todos/chats, model selection for tasks and assist widgets) plus the older "Update Todo List Feature" todo. The review found that most of them are bugs or half-built features, not only missing UI.

Decisions taken in the discussion (2026-10-08):
1. **Items:** go straight to one item record with an assignee. No view-only merge first.
2. **A todo is one-shot work, probably multi-step.** It is not a routine. "Make it a schedule" is an explicit switch on the item that produces a ScheduledTask draft. The todo itself never recurs.
3. **"Widget"** means anywhere the user asks an agent to assist with something. The schedule AI-assist is the model case.
4. **Rewrite every existing claim**, not only new ones. Garbage in, garbage out.
5. These phases are appended here, not in a new plan doc.

### 9.1 What the review found

| Todo | Finding | Evidence |
|---|---|---|
| Model selection | **The step Provider dropdown does not do what it says.** Only `ollama` changes routing (`USE_LOCAL_LLM`). `anthropic` and `gemini` only pick which concurrency semaphore the job waits on, then run on the active chat config. Steps have **no `model` field**, but the editor's help text lists one. The live "Zero Trust Architecture Research" schedule sets `model: gemma4:latest`, a local model, on an `ollama-cloud` step, and it is silently ignored. | `services/queue.rs` (semaphore match, `use_local`), `ChainStep` has no `model`, `ScheduleForm.tsx` help text, `shared_llm.rs` routing precedence |
| Model selection | The one assist widget (schedule AI-assist) always runs `schedule-author` on its pinned model. It re-states the agent's own lint rules in a frontend prompt string. | `ScheduleForm.tsx` `handleAiGenerate` |
| Ambiguous claims | **The source exists and nothing shows it.** 4,485 of 4,510 claims have `ASSERTED_IN` to a source note, and almost every one of those sources carries its URL. The note drawer doesn't render the edge. The extraction prompt already asks for "plainly and self-contained" claims, and the model ignores it ("the software"). Every claim is stored as `provenance: 'user_input'`, hardcoded in `store_claim`, and recall shows that label to the model. The Paragon claim (889d47f8) is filed under the `agent-brain` topic. | `services/claims.rs` `extract_claims` / `store_claim`, `NoteView.tsx` |
| Group schedules | 28 schedules, with no topic or kind field. Where their notes actually land (30 days, by `source_context`): monitors map cleanly (`off_grid_networking_watch` → 31/31 off-grid-networking). Pipelines span every topic (`video_learning`, news, claim evidence). Those are services, not topics. | live graph, 2026-10-08 |
| Collapse sidebar | The chat session list collapses since Phase 7. The 200 px nav sidebar does not. | `main.css .sidebar` |
| Inbox / Todos / chats | "Something wants you" lives in four places: Work → Inbox (proposals, 188 entity merges, overdue todos, failures), Todos, the Chat notification banner, and Home's Needs-you tile. All 100 recent notifications open a chat session: 45 Todo Review, 30 Daily Brief, 13 Daily News Brief. `TODO_DELAYED_PROMPT_PLAN.md` (2026-09-11) designed assignees and dispatch, and **none of it is built**. There is still no `manage_todo` tool. | `/api/notifications`, `InboxLane.tsx`, `skills/` |

### 9.2 Items: one record for work, with an assignee

Builds on `TODO_DELAYED_PROMPT_PLAN.md`. That plan's design and every one of its guardrails are kept: the capped `dispatch_count`, bounded fan-out, roster validation at assign time, idempotent dispatch, no recurrence on a todo, and `side_effects` derived from the allowlist and failing safe. This section supersedes only its sequencing.

**The record is `:Todo`**, extended rather than replaced. REST, the `SPAWNED` lineage, three schedules and the front door's `pre_load_query` already read it.

| Field | Values |
|---|---|
| `assignee` | `user` · `agent:<profile>` · `agent:auto`. Validated against the live spawnable roster when written. |
| `origin` | `{kind: user \| schedule \| proposal \| agent, ref}`: who asked, so the item links back to it |
| `prompt` | What the agent is told. Falls back to title + description. |
| `session_id` | The item's thread: a chat session `todo-<id>` |
| `status` | `pending` · `in_progress` · `awaiting_approval` (a writing agent, not yet started) · `awaiting_review` (the agent finished) · `done`, plus `snoozed_until` |
| bookkeeping | `dispatched_at`, `dispatch_count`, `result_note_id` |

**What becomes an item**
- **Your own todos.**
- **The brain's asks.** `notify_user` gains `needs: action`, which also creates an item assigned to `user` with the notification's session as its thread ("this API needs a key", "apply to this job?"). A plain `notify_user` stays a read.
- **Code proposals.** The proposal writer creates the item, and the existing files are backfilled once.

**What stays out**
- **System signals** (entity merges, failed tasks, dead jobs, broken schedules) are computed digests, not rows. 188 merge items would bury everything else.

**An agent-assigned item is a Task with a delay**, as in the delayed-prompt plan:
- `create_task` routes through `chains/run-agent.yaml`, and the agent's own loop does the multi-step work.
- The last step writes the result as a note (`result_note_id`), posts it into the item's thread and sets `awaiting_review`.
- **Review actions:** approve (→ `done`) or **send back** with a comment, which runs again with the comment and the previous result in the prompt and counts toward `dispatch_count`.

**"Make it a schedule"** is a switch on the item:
- `schedule-author` drafts a ScheduledTask from the item's prompt. It is created disabled and linked `(:Todo)-[:BECAME]->(:ScheduledTask)`, and opens in the schedule drawer for review.
- Enabling it closes the item as "became a schedule".

### 9.3 The feed

Work → Inbox becomes **Feed**, one list of what wants the user, from one endpoint (`GET /api/feed`). It has three row kinds:

1. **Items for you:**
   - which items: `assignee: user` and open, plus every `awaiting_approval` and `awaiting_review` item;
   - inline actions: done, snooze, reply (opens the thread), hand to an agent (the §9.4 pickers), approve and send back;
   - order: overdue first, then awaiting you, then newest.
2. **Reads:** unread notifications without `needs: action`, such as the Daily Brief and the News Brief. Opening one opens its session and marks it read.
3. **Digests:** one card each for "188 entity merges to decide", "3 tasks failed this week" and "1 schedule with broken steps". Each drills to its lane, and each count is drillable to its rows.

Around it:
- **The Chat notification banner goes.** The nav badge, Home's Needs-you tile and the Feed header all count the same `/api/feed` total, so they can't disagree.
- **Todos lane** becomes the full item list, filtered by assignee (mine · agents · all) and status.
- **Todo Review stops opening a new chat every day** (45 of the last 100 notifications). Open items assigned to you go into the Daily Brief as a reminder block (the delayed-prompt plan's Phase 4), and the Feed is where they live.

### 9.4 Models wherever an agent works

**Backend**
- **A step's `model`.** `ChainStep` gains `model`:
  - It is resolved the way a profile's `model_preference` already is (`resolve_agent_model`), into `SELECTED_LLM`, and the routing policy still admits it.
  - A refused or unknown model shows on the job, never silently falls back.
  - `provider_hint` is derived from the model (local → `ollama`), so the two can't contradict. The semaphore match uses provider names that exist.
  - Schedule upsert validation rejects an unknown model and a contradicting hint (the Zero Trust case).
- **`run_agent` gains an explicit `model` override**, also admitted by policy.

**Frontend**
- **`<ModelPicker>`:**
  - built from `/api/models` and the provider registry;
  - grouped by provider, with billing, data-policy and cooling-down badges;
  - warns when private work would be refused there;
  - has an "agent default (X)" choice.
  - It is used in the step editor, the agent editor, the chat header and every assist.
- **`<AgentAssist>`:** one widget for asking an agent to help.
  - It holds a prompt, an agent (each widget's natural default), a model (the agent's own by default) and a run button.
  - The run shows the tool trace the way the chat dock does, then puts the result into the form.
  - **Uses:** the schedule AI-assist (replacing the inline one), the todo form ("draft the prompt", "break this down"), and the agents editor ("draft a profile").
  - The lint rules in today's assist prompt are deleted from the frontend; `schedule-author` is their one home.

### 9.5 Schedules by topic and kind

- **Declared:**
  - `topic:` (a `topics/*.yaml` name, or none) and `kind:` (`monitor` · `brief` · `learning` · `maintenance` · `dataset`), set in `schedules/*.yaml`, on the ScheduledTask node, and in the HBI form;
  - the seeder validates `topic` against the declared topics;
  - starting values for all 28 are drafted for the user to review, as with topics in 4b.
- **Computed:**
  - the 30-day topic mix of the notes a schedule's runs produced, from the lineage `clients/work.rs` already reconstructs (schedule → task → jobs → notes);
  - shown next to the declared topic, with a mismatch flagged and a suggestion where none is declared (the same declared-vs-computed pattern as the architecture view).
- **List and timeline:**
  - group by topic, kind or none (remembered);
  - collapsible groups with on/off counts;
  - "no topic" is a group of its own, never hidden.

### 9.6 Claims that stand alone

**Display, for every existing claim (Phase 8):**
- the claim drawer shows **Asserted in**: the source note's title, URL ↗ and date;
- the 25 without an edge say "no source recorded".

**Provenance:** claims are LLM-extracted, so they are `synthesis_inference`, not `user_input`. Fix `store_claim`, and correct the existing 4,510.

**Extraction:**
- The model returns `about: [names]` with each claim.
- **Checked mechanically:** at least one `about` name must appear in the claim text.
  - A miss gets one repair call with the source.
  - A second miss drops the claim, counted in the tool result.
- `source_url` and `source_title` are stored on the claim itself, so a reader that gets it through search sees where it came from without a traversal.
- The claim's topic is classified from the claim plus its source title.

**Rewrite all 4,510 (decided):**
- **Calls:** one cloud call per source note (1,054 sources, ~3.8k characters on average), rewriting all its claims with the source text.
- **Guards:**
  - id, status, `asserted_by` and evidence edges (635 corroborated, 29 contradicted) are kept;
  - the old text is kept in `content_v1`;
  - every number and proper noun in the old text must survive into the new, checked mechanically, or that claim keeps its old text;
  - each rewritten claim is re-embedded and its topic re-classified;
  - a snapshot is taken first.
- **When the source can't resolve it:** a video claim's source is the video's summary, not its transcript. If the summary can't resolve a referent, the claim keeps its text and is marked `ambiguous: true`, a visible count, rather than the model inventing a referent.
- **Order:** a dry run on 50 claims for the user to review, then the full run. `eval-retrieval` must be no worse.

**Provenance for every note, not only claims (added to Phase 9 by the user, 2026-10-08).**

The problem:
- **`store_note` defaults to `user_input`** whenever the caller passes no `provenance` (`skills/knowledge.rs`), and almost no brain writer passes one.
- **Live counts on 2026-10-08:**
  - 11,513 notes are `user_input` and 11,015 `synthesis_inference`; 769 have none.
  - The `user_input` ones are nearly all machine-written: 3,979 video summaries, 3,753 `scheduler_dispatch` run records, 2,096 `claim_evidence`, 329 `chat_session`, 223 `sleep` outcomes, 161 `learn_chain`, 121 news briefs, …
- **Recall shows the label to the model**, so it reads most of the brain's own output as something a person stored.

The fix:
- **The default comes from who is calling.** `store_note` reads the route context instead of assuming a person:
  - **queue job, sub-agent or chat turn** (the model chose the words) → `synthesis_inference`;
  - **no route context at all**, meaning a direct MCP call from an outside client → `user_input`, as today;
  - **an explicit `provenance` argument** still wins.
  
  `POST /api/notes` (the HBI) already writes `user_input` explicitly. Other raw-Cypher writers that hardcode `user_input` are found by grep and fixed the same way: `repository/task.rs`'s run record, and any others the audit turns up.
- **Backfill, after the deploy:**
  - Brain-written `user_input` notes and the 769 with none become `synthesis_inference`, chosen by `source_context`.
  - Only the human sources keep `user_input`: `hbi`, `resume`, and any other the audit shows came from a person.
  - A script in `scripts/`, with a dry-run count by `source_context` first.
- **Decision (taken with §9.9's picks):** notes the chat model stores during a conversation are `synthesis_inference` even when they repeat the user's words, because the model chose what to keep. No new provenance value is added: `note_type: source_record` already says a note records an external source.
- **Done when:** every `user_input` note traces to a human path, and nothing has no provenance.

### 9.7 Nav sidebar

The sidebar collapses to an icon rail (~52 px):
- labels move into tooltips;
- the badge stays;
- the choice is remembered per browser.

### 9.8 Phases

| Phase | Scope | Done when |
|---|---|---|
| **8. Honest fixes** | §9.7 nav collapse; claim **Asserted in** in the drawer; claim provenance (new and existing); the Provider dropdown relabelled to what it does ("local" / "cloud: the current chat model"), and the step help text stops listing `model` until it exists. | The Paragon claim's drawer links the Citizen Lab article; no control in the HBI claims to choose something it doesn't. |
| **9. Claims stand alone + honest provenance** | §9.6 extraction (`about` check, source fields, topic from claim + source); dry run on 50 → user review → rewrite all. `store_note` provenance from the caller, raw-Cypher writers audited, backfill of ~12.3k notes. | Every claim is self-contained, marked `ambiguous`, or was dropped with a count; `eval-retrieval` no worse; the Paragon claim names the software and links its article; every `user_input` note traces to a human path and none has no provenance. |
| **10. Models everywhere** | §9.4: step `model`, `run_agent` `model`, validation, `<ModelPicker>`, `<AgentAssist>` in the schedule form, todo form and agents editor. | A step pinned to model X shows X in the `model_usage` ledger; a refusal is visible on the job; the Zero Trust schedule's model either runs or is rejected at upsert. |
| **11. Schedules by topic** | §9.5: `topic` / `kind` fields and seeds (user reviews), the computed mix, grouping in the list and timeline. | Every schedule is in a group or in "no topic"; a declared-vs-computed mismatch is shown. |
| **12. Items + Feed** | §9.2 fields + `manage_todo` (delayed-prompt Phase 1); `notify_user needs: action`; proposal items; `/api/feed`; Feed replaces Inbox; banner gone; Todos lane filters. | Everything that wants the user is in one list, and the badge, the Home tile and the Feed count the same thing. |
| **13. Agent work** | Assign (roster-validated), `side_effects` (delayed-prompt Phase 2), `dispatch_due_todos` (Phase 3: auto for read-only agents, `awaiting_approval` otherwise), thread + review / send back, "make it a schedule", Brief reminders (Phase 4). | "Research Sodium batteries" assigned to `researcher` runs, its result lands in the item's thread awaiting review, and "make it a schedule" produces a disabled draft. |

**Order rationale:**
- **8** is cheap and removes things that mislead today.
- **9 comes early** because extraction runs every day, so each day of delay adds claims to rewrite.
- **10 comes before 13** because assigning work needs the model picker and `<AgentAssist>`.
- **12 comes before 13** because dispatch writes its results into items.

### 9.9 Decisions (taken 2026-10-08: every recommendation accepted)

1. **Does every agent result wait for review** (`awaiting_review`), including read-only agents'? *Recommend yes for v1.* Approving is one click, and the first runs are what show whether auto-close can be trusted.
2. **Proposals as items?** *Recommend yes.* Reviewing a code change is work for the user.
3. **Replace the daily Todo Review chat** with a reminder block in the Daily Brief? *Recommend yes.*
4. **Video claims the summary can't resolve:** mark `ambiguous` rather than rewrite? *Recommend yes.* Re-fetching 3,949 claims' transcripts is a separate cost decision.
5. **Who may assign items to agents?** *Recommend the user only, for v1.* The brain already creates Tasks for itself.

### Phase 8 outcome (2026-10-08)

Built:
- **Nav rail collapses** to 52 px icons, with labels in tooltips and the badge kept, remembered per browser (`hbi.nav.collapsed`).
- **Claims show where they were asserted.** `GET /api/notes/{id}` returns `asserted_in`: each source note's title, URL and outlet, read by `claims::source_reference` from the summary JSON article and video records carry. Reports and briefs fall back to their first line. The note drawer shows it as **Asserted in** with a link to the source note and the article ↗. The Paragon claim now reads "The Secrets of a US Spyware King - The Citizen Lab · citizenlab.ca ↗ · The Citizen Lab".
- **Claim provenance:** `store_claim` writes `synthesis_inference`. All 4,510 existing claims relabelled by `scripts/fix_claim_provenance.cypher`, run after the deploy, which leaves none labelled `user_input`.
- **"Runs on" instead of "Provider":** the step editor offers *Local model* or *Cloud: the brain's active model (gpt-oss:120b-cloud)* and labels any other hint it finds with what it does ("ollama-cloud: also cloud, on …"). The raw-JSON help no longer lists `model`; it says a per-step model isn't read yet.
- **Tests:** 2 Rust unit tests (837 lib), 4 vitest tests (82). Checked on the live stack in headless Chrome: the Paragon drawer, the collapse surviving a reload, and the Zero Trust step editor.

Fixed on the way (found by this phase, worse than the todo):
- **The schedule and routing-chain editors dropped step fields on save.** The draft converter kept only `tool_name` / `arguments` / `provider_hint` / `priority`. Saving a schedule in the HBI therefore silently removed `data_class` (public work became private, so the routing policy narrowed), `max_attempts`, `description`, and on routing chains `is_evaluator` / `min_score` / `context_profile`. 25 of the YAML schedules carry such fields. A step with no `provider_hint`, which runs on cloud, was saved back as `ollama`. Every other field now rides in `StepDraft.extra` and is listed under the step as "Also kept", and a missing hint stays missing. The Zero Trust schedule's ignored `model: gemma4:latest` now shows there as "not read: steps can't pick a model yet".

Not built here: a claim's source on the claim node itself (Phase 9 stores `source_url` / `source_title`). `store_note` still defaults every brain-written note, `source_record` summaries included, to `provenance: user_input`; only claims were in scope. Moved into Phase 9 (§9.6).

**Done criterion met:** the Paragon claim's drawer links the Citizen Lab article, and no control in the step editor claims to choose something it doesn't.

### Phase 9 outcome (2026-10-08; full rewrite applied 2026-10-09)

Built and deployed:
- **Every note's provenance is decided by who writes it** (`knowledge::effective_provenance`):
  - the brain's own work (anything under a route context: queue jobs, chat turns, sub-agents) is `synthesis_inference`, and can't claim `user_input` even when a model passes it;
  - an outside MCP client's own call (`CLIENT_CALL`, `mcp/server.rs`) is `user_input`.
  - **New value `system_record`** for records the brain's code builds (scheduler dispatches, claim evidence, code-version notes). Calling them LLM synthesis would have been the opposite mislabel.
  - Recall reads a missing value as `unrecorded`, not `user_input`. The HBI's fallback note path now writes `user_input` explicitly.
- **Data fix applied** (`scripts/fix_note_provenance.cypher`, run after the deploy): 5,942 notes became `system_record` and 6,327 `synthesis_inference`. Only the 13 résumé notes are still `user_input`, and none has no provenance. Each relabelled note keeps `provenance_was`.
- **Claims must stand alone at extraction** (`claims::standalone_problems`, mechanical):
  - the model declares `about: [{name, what}]`;
  - every name must be in the claim, and be a name, not a vague noun;
  - every bare "the + vague noun" must be declared and named;
  - the claim may not open with a pronoun.
  
  A failing claim gets one repair (`repair_claim`) and is otherwise dropped, listed in the tool's `dropped`. Stored claims carry `about`, `source_url`, `source_title` and `source_outlet`. The topic classifier reads the source title with the claim.
  - **Measured on the local model** (Gemma E4B, which does all live extraction), 8 real sources: 21 claims stored, 2 repaired, 2 dropped. One dropped claim was a meta-claim ("The video discusses …"); the other said "seven financial institutions" without saying where.
- **`rewrite-claims`** (`services/claim_rewrite.rs`, CLI):
  - one cloud call per source note;
  - a rewrite replaces a claim only if it stands alone **and** loses no number or name (`rewrite_losses`, which folds Unicode hyphens and spaces first);
  - one targeted repair, otherwise the text is kept and the claim marked `ambiguous` with the reason;
  - the old text goes to `content_v1`, and the claim is re-embedded and re-queued for topics;
  - a snapshot is taken first, and a stopped run resumes.
  - The claim drawer shows **About**, the ambiguity reason, and the original text of a rewritten claim.
- **Tests:** 846 Rust lib tests (14 new), 82 vitest.

**Dry run on 50 claims across writers** (cloud, nothing written): 53 claims from 10 sources.
- **Outcome:** 30 rewritten, 20 already stood alone, 2 ambiguous, 1 kept by the meaning guard.
- **Iterations to get there:**
  - the first run kept 12 for typography (non-breaking hyphens, number ranges);
  - the next lost acronyms (US → United States) and dates (→ "September 28");
  - both were fixed in the comparison and the prompt.
- **The Paragon claim (889d47f8)** now reads: "Paragon Solutions' CEO Andrew Boyd said that Paragon Solutions lacks a mechanism to verify customer misuse of its spyware or to deploy a 'kill switch' to disable the Paragon Solutions spyware." That needed the rule "if the source gives no name, say whose it is". The source never names the product.
- **Not caught by any check:** "admitted" became "said". Verbs that soften are invisible to a mechanical guard, which is why the user reads the dry-run report before the full run.

**Full run, 2026-10-09** (after the user approved the dry run): 4,510 claims from 1,054 sources in 87 minutes, 0 calls failed. Snapshot first: `snapshot_pre_claim_rewrite_20261009_040444.json.gz`.
- **Outcome:**
  - 2,712 rewritten;
  - 1,578 already stood alone;
  - 152 ambiguous;
  - 43 kept by the meaning guard;
  - 25 with no source note left.
  
  That makes 220 claims marked `ambiguous`, each with its reason. The rewritten claims dropped `topics_version`, so the idle topic pass classifies them again.
- **Before the run, `judge` counted a typography-only change as a rewrite.** It now compares lowercase letters and digits only, so "2024" vs "2024." and "64%" vs "64 %" count as unchanged.
- **`eval-retrieval`:** recall@10 went from 0.920 to 0.880, and MRR from 0.757 to 0.780.
  - One case improved, from rank 3 to rank 1 (Leavitt).
  - One was lost: "Can Reticulum firmware run directly on Meshtastic LoRa radios?" It was a hit at rank 10, at the very edge, and is now outside the top 30.
  - Its target claim (77b8c4ed) was not changed; it already stood alone. It is outranked by rewritten neighbours that now name Reticulum and Meshtastic outright ("A bridge (such as a Raspberry Pi) can now link the Meshtastic application/firmware with the Reticulum networking protocol"). Those neighbours answer the same question.
  - On 25 cases a single case is 0.04, so this is read as a re-rank among relevant claims, not damage. The snapshot restores the old text if it proves otherwise.

Not built: a check that catches a missing location or target ("AI agents were deployed", with no target named).


### Phase 10 outcome (2026-10-09)

Built and deployed:
- **A step's `model`** (`ChainStep.model`, `services/queue.rs`):
  - a catalog model id or name, matched exactly (`model_router::find_model`; "gemma4" never becomes `gemma4:31b-cloud`);
  - stamped as `__model`, it takes precedence over `required_capabilities`;
  - judged by the routing policy at execution (`model_router::pinned_config`). An unknown or refused model kills the job at once with "Step model cannot run: …". The owning task fails with that reason, and meta-learning is skipped, because the fix is the step.
  - `provider_hint` is derived from the model at enqueue (`provider_hint_for`: a local provider gives `ollama`, otherwise the provider's own name). A cloud pin that later hits its quota still walks the usual fallback chain, recorded in the ledger.
- **Upsert validation** (`validation::validate_step_models`, also run by `audit`): an unknown model, or a hint that contradicts the model, is a hard reject. A pin the policy would refuse right now is a soft `step_warning`. A pinned step is no longer asked for a hint.
- **`run_agent`** gains `model`, and takes a step's `__model` when its own isn't set. An unknown or refused model rejects the run with the reason. It also gains `read_only`, which strips write tools, so a draft is never stored. The result names the `model` it ran on.
- **Chat turns** take `model`. An unknown or refused one ends the turn with the reason and calls nothing.
- **`/api/models`** gives every catalog row an `admission` field: the policy's verdict per purpose (background, subagent, chat) × data class, `null` or the refusal. Spend caps and cooldowns surface through it.
- **Frontend:**
  - `<ModelPicker>` (`components/ui/ModelPicker.tsx`, `lib/models.ts`): grouped by provider, local first, with billing and data-policy badges, "refused here" on each model the policy would refuse, a warning under the chosen one, and a stored model outside the catalog kept and flagged.
  - Used in: the step editor (where "Runs on" is derived once a model is pinned), the agents editor's model preference, the chat header (a per-conversation choice, not remembered), and every assist.
  - `<AgentAssist>` (`components/ui/AgentAssist.tsx`, `lib/assist.ts`): a prompt, an agent, a model and a run, read-only, followed by the tools it actually ran. Used in:
    - the schedule form (schedule-author; the lint rules were deleted from the frontend prompt, and the agent's `{name, interval_seconds, steps}` object is read);
    - the todo form (knowledge-worker: "Draft a description", "Break it down");
    - the agents editor (knowledge-worker: "Draft profile").
  - `schedule-author` gained one rule, on `model`.

**Data fix:** Zero Trust Architecture Research (93cb3808, runtime-owned) carried `model: gemma4:latest` on both steps, never read until now. Step 1 also had `provider_hint: ollama-cloud`. Honouring it would have moved both steps onto host Ollama, which shares the 8 GB GPU with llama.cpp. `scripts/fix_step_models.cypher` removed the field before the deploy, so the schedule runs as it always has; the old steps are kept in `steps_before_model_fix`. No other schedule, chain or pending job named a model.

**Verified live:**
- **The done-condition:** a disabled throwaway schedule whose `reason` step pinned `gpt-oss:20b-cloud` (unpinned, it would run on `gpt-oss:120b-cloud`) logged "Step runs on its pinned model", and `/api/models/usage` showed exactly one `gpt-oss:20b-cloud` call.
- **Refusal:** pinned to `gemini-2.5-flash`, the step died with the policy's reason, the task failed with it, and no meta-learning ran.
- **Upsert rejection:** the Zero Trust shape and an unknown `gemma4` were both rejected.
- **Assist:** `run_agent` with knowledge-worker, `gpt-oss:20b-cloud` and `read_only` answered on that model.
- **Browser:** the step picker warned on Gemini for private background work, and the saved step carried `model` plus the derived `provider_hint: ollama-cloud`.
- The throwaway schedule was deleted.

**Tests:** 852 Rust lib tests (6 new) and 92 vitest (10 new); clippy is clean.

**Not built:**
- the delegate tool in chat can't choose a model; a delegated agent uses its profile's own;
- the YAML schedule seeder does not run the model check; `audit` does.

### Phase 11 outcome (2026-10-09)

Built and deployed:
- **Declared `topic` and `kind`** on every ScheduledTask: in `schedules/*.yaml`, on the node, and in the HBI form.
  - **Checked on every write path:** the seeder, `manage_scheduled_task` upsert, and REST `POST`/`PUT /scheduled-tasks` (`schedule_seeder::check_labels`). A test fails the build on a bad schedule file.
  - **The schedule is now the one home of the topic link.** The 8 entries in topics/*.yaml `schedules:` became starting values and the lists were removed. A topic's "Watched by" is computed from `ScheduledTask.topic`.
  - **Starting values for all 28 were drafted:** 25 in the YAML files, plus 3 runtime-owned ones (Trump Benefit Daily Monitor `us-politics`, Zero Trust `cybersecurity`, SELF-TEST `agent-brain`), set through REST. The user reviews them in the HBI, with the computed mix beside each.
- **The computed mix** (`GET /api/work/schedules/topic-mix`, 30 days, ~0.2 s):
  - each note is traced by run stamp, job result, `source_context`, or a claim of one of those, and counted once with its route;
  - each note's main topic is counted, chunks left out;
  - a verdict per schedule: matches, mismatch, suggest, spans, or too few.
- **Every note a job writes is now stamped `produced_by_task`** (a `CURRENT_OWNER_TASK` task-local in the queue; the note store and `store_claim`).
- **HBI:**
  - List and timeline grouped by topic, kind or none (remembered), with collapsible groups (remembered), on/off counts, "No topic" last, and the mismatch or suggestion on each row.
  - The schedule drawer's **Topic** section: declared topic and kind, the mix with shares, and how many notes each route traced.
- **REST schedule saves now run the step-model check.** Phase 10 had put it only on `manage_scheduled_task`, while the HBI saves through `/scheduled-tasks`.

**Found while building:**
- **Completed AgentJobs are pruned after about two days.** Lineage through jobs alone would have called two days "30 days". Hence the stamp, and the `source_context` route for history. 13 of the 28 schedules write a fixed `source_context`, and no two share one.
- **Chunk notes are never classified, only their parents are.** Counting them showed dozens of notes "waiting" while coverage said 0.
- **A single-node pattern comprehension (`[(s:ScheduledTask {topic: t.name}) | …]`) is a syntax error in Neo4j 5.** It took `/api/topics` down until it was replaced with a `COLLECT { }` subquery, tested in cypher-shell first.

**Verified live:**
- **Groupings:** every schedule sits in exactly one group (28 by topic: 9 topics plus "No topic" 13; 28 by kind).
- **The mismatch is shown (the done-condition):** Trump Benefit Daily Monitor is declared `us-politics`, but its notes are mostly `geopolitics` (13 of 28; us-politics 6; 9 fit no topic).
- **Suggestions for schedules with none:**
  - Overdue TODO review and Todo snapshot → `agent-brain`;
  - Weekly news briefing → `geopolitics`;
  - Daily news analysis → `ai-ml` (15 of 29).
- **Matches:** intelligence-agency, off-grid, SLM watch, hardware tripwire, capability mining, API sync.
- **Run stamp:** a throwaway schedule's note carried its run's task id and was counted under `run_stamp`. The schedule and the note were deleted.
- Bad topics, kinds and step models are rejected by REST with the reason.

**Tests:** 856 Rust lib tests (4 new) and 95 vitest (3 new).

**Waiting on the user:** review the 28 starting values, above all the Trump monitor (us-politics or geopolitics?) and whether to take the four suggestions.

**Not built:** schedules whose steps write no fixed `source_context` (media watch, claim verification, deep dive, …) show "too few" until runs stamped from now on accumulate.
