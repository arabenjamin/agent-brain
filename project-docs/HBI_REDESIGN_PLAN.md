# HBI Redesign — from tabs to a dashboard

Status: **Phases 0–2 done (2026-10-08); Phases 3–7 planned.** All §7 decisions taken 2026-10-08 (the recommendations, plus proposals-only entity merges). Supersedes the layout in `HBI_PLAN.md` (Feb 2026), whose transport and SSE notes still hold.

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

**Proposal: declared topics, assigned by embedding.**

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
| B1 | `GET /api/search?q=&mode=auto\|semantic\|keyword\|id&kind=note\|entity\|topic&topic=&limit=` | Semantic goes through the **non-perturbing** `search_notes_inner(track_access=false)` path: a human browsing must not skew freshness/access-count retrieval signals. Keyword uses the BM25 fulltext index, not `CONTAINS`. Each hit returns `match_reason` + score. A truncated result says so. |
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
