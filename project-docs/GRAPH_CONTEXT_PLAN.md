# Graph Context Plan — the right context is a query away

**Status: planned 2026-10-13. Phases 0 (mined), 1 (baselines) and 2 (views and
`lookup`, built and measured) done 2026-10-13; the owner graded the five
shape cases 2026-10-14 (constrained E4B 18/30 full, 20 near-full, 8/9 empty
right, against 10/33 for free Cypher). Phase 3 rewritten 2026-10-14 from a
"graph needed?" gate to tool routing: the right agent and tool for a turn,
loose wording included. Steps 1–4 built 2026-10-14 (a front-door router,
E4B + a stored-first rule). Phase B, behaviors first and the agent loop as
the fallback (owner, 2026-10-14): the first behavior is "what did we learn
today".**

The brain is only as good as the context in front of the model, and that context
is only as good as the query that fetched it. This plan makes graph context
deterministic:

- **When:** the brain decides that a turn needs the graph, instead of hoping the
  model reaches for `neo4j_query`.
- **How:** queries come from a small spec that code compiles to Cypher, not from
  free-form model output.

Accuracy first; precision can be refined later (the owner, 2026-10-13).

## 1. What the brain does today (measured 2026-10-13)

### Every query is already logged

DuckDB `interactions` logs each `neo4j_query` call with its Cypher (in `prompt`)
and its result (in `response`).

| | Calls |
|---|---|
| Logged `neo4j_query` calls | 10,016 parsed (logging began 2026-06-12) |
| Reads / writes | 6,542 / 3,474 |
| Distinct query shapes (literals stripped) | 356 (334 reads) |
| Errors | 29, plus 8 schema-guard and 5 read-only rejections |
| **Reads returning zero rows** | **369 of 6,506 (5.7%)** |

A first count of 38% empty included writes, which return no rows by design.

**Reading:** syntax is almost never the problem; a query that runs fine and returns
the wrong rows is. Phase 0 (§3) found the clearest case: a datetime compared to a
string, which returns nothing without an error.

### The schema is small

36 labels, 24 relationship types, 272 property keys. The whole schema fits in a
prompt, and every name fits in a grammar as a closed list.

### Where it goes wrong (the owner's read)

1. **Mostly not knowing a query was needed.** The model answers without the graph
   context it should have fetched.
2. **When it does query, not always the right query** for what was asked.
3. **Reviewing Cypher in workflow steps on the frontend is painful.** In chat the
   query is visible as it forms; in the workflow editor it is escaped text in a step's
   arguments. 18 workflows carry raw Cypher.

### Models that exist

Neo4j publishes Text2Cypher models and data on Hugging Face (public, not gated,
checked 2026-10-13):

| Model / data | Size / licence | Fit here |
|---|---|---|
| `neo4j/text-to-cypher-Gemma-3-4B-Instruct-2025.04.0` | Gemma licence; GGUF builds by `mradermacher` | ~3 GB at Q4, runs on host Ollama beside the sidecar |
| `neo4j/text-to-cypher-Gemma-3-27B-Instruct-2025.04.0` | Gemma licence | too big for the 8 GB GPU |
| `neo4j/text2cypher-gemma-2-9b-it-finetuned-2024v1` | LoRA adapter on Gemma 2 9B | needs merging; the 4B is newer |
| `neo4j/text2cypher-2025v1` (dataset) | Apache-2.0 | training data if we fine-tune |

Expect these to do worse on our schema than on Neo4j's benchmark: text-to-Cypher
models carry over poorly to schemas they were not trained on. They are a
baseline, not the answer.

## 2. Principles

1. **Right rows, not right text.** A query is scored by what it returns compared to the
   reference query's rows, as `eval-retrieval` scores retrieval.
2. **The model decides what to ask; code decides how to write it.** Datetime
   handling, `LIMIT`, read-only and known names are enforced once, in the
   compiler, for every query.
3. **Measure on the brain's real queries before building** (the
   `SOURCE_QUALITY_PLAN.md` lesson). The spec is designed from the logged query
   shapes, not guessed.
4. **Empty is a finding, not an answer.** A query that returns nothing says what
   it looked for and what does exist (CLAUDE.md: failures must be visible).
5. **Graph context comes in two kinds.** Structural questions are pure queries.
   Fuzzy ones need vector + BM25 retrieval, which Cypher cannot do without an
   embedding the brain computes. The best context is often both: retrieval hits
   as starting points, Cypher to expand around them. The spec covers both.

## 3. Phases

### Phase 0 — mine the logged queries (no model calls)

- Group the ~3,609 distinct queries by shape: start label, traversal, filters,
  return, write vs read, which workflow or chat turn made them.
- Flag the suspicious empty results by pattern: date properties compared to string
  literals, property names not in the schema, labels that are spelled
  differently.
- Draw about 50 candidates for the test set (a question, a reference query and
  its expected rows); the owner grades them. The question for a workflow query
  comes from the step's purpose; for a chat query, from the turn that asked.
- **Output:** a shape catalogue, a suspicious-empties list, and
  `eval/cypher_golden.yaml` with a scorer (an `eval-cypher` CLI next to
  `eval-retrieval`).

**Results (2026-10-13).** The telemetry file was copied out of the container and
read with the DuckDB CLI; calls were tied to workflows through
`__owner_task_id` → `Task.workflow` (1,609 of 3,843 tasks still exist).

- **What the reads are for:** mostly plumbing. `WorkingMemory` (workflow session
  state) is in 4,896 of 6,542 reads; then `Todo` 497, `AgentJob` 370, `Note` 361,
  `Task` 326. Queries that fetch context for a question are a small slice.
- **Date compared to a string: 207 calls, 200 empty, no error.** Every one was
  from 2026-06 to 2026-09, and there are none in October (the schema guard's
  warning and the workflow rewrites). The biggest was `overdue-todo-review`'s
  `t.due_at < toString(date())`: 179 runs, 172 empty, so for three months
  overdue todos were never reported. `AgentNotification … created_at > '<date>'`
  made another 21.
- **Empty reads by month:** 43% in June, 1.2% in October.
- **Unknown names** are mostly drift: `ScheduledTask` (98 calls),
  `SchedulerChain`, `ChainStep` and `HAS_STEP` before workflows replaced them
  (Phase 2b).
- **`neo4j_query` binds a list or map parameter as JSON text** (its schema says
  "string values only"), so `UNWIND $ids` quietly matches nothing. Only 2 logged
  calls hit it (both errored), but it is a trap a model will walk into.

**Chat.** 388 user turns are stored in `WorkingMemory` (2026-02 to 2026-10). Of the
213 since logging began, 23 (11%) fetched nothing. The rest reached mostly for
retrieval by meaning: `search_notes` in 96 turns, `recall_recent` in 88,
`neo4j_query` in 74. So "not knowing a query was needed", taken narrowly, is
about 1 turn in 9; whether the retrieval chosen was the right kind is what the
test set answers.

**The test set:** 47 draft cases in `eval/cypher_golden.yaml`: 34 graph, 6 both,
3 by meaning, 3 needing nothing, 1 telemetry (DuckDB). The questions are verbatim
from chat, plus two workflow purposes. Every reference runs on the live graph.
The scorer CLI comes after grading.

**Drafting the references made the plan's case.** Five of the first drafts were
wrong in exactly the ways this plan is about, and none of them errored:
- `Topic {label: 'nia'}` where the short name is `name`: 3 queries empty.
- A keyword match ("shutdown") that returned a ransomware claim.
- A guessed JSON key (`tool` for `tool_name`) that made the broken-tools audit
  report two broken tools that were not.
- Plus one real syntax error (`ORDER BY` after an aggregation).

Grading also turned up what a query writer has to know:
- Entity names are stored lower-case.
- The API is `home_assistant`, not "HomeAssistant".
- One question's premise was wrong: the stored claims name Governor Newsom
  (California), not "the Biden administration".

### Phase 1 — baselines on the test set

- Text2Cypher Gemma 3 4B (GGUF on host Ollama, `num_ctx` set).
- Local E4B with the schema in the prompt and a GBNF grammar on the sidecar
  (labels, relationship types and properties as closed lists), so it can only
  write Cypher with names that exist.
- A cloud model as the best case, **not on Ollama Cloud** (rate-limited; the
  owner, 2026-10-13): free Mistral or OpenRouter.
- **Read the failures:** wrong thing asked for (→ spec + routing) vs right
  thing written wrong (→ grammar, fine-tune).

**Results (2026-10-13, on the draft references: re-run after grading).**

**Setup:**
- **Prompt:** each contender got the same prompt: Neo4j's Text2Cypher instruction,
  the live schema in its training format (properties with examples, the
  available options or most common values, datetime ranges, relationship
  patterns; ~39K chars), and three lines of conventions (UTC datetimes, local
  zone America/Detroit, today's date).
- **Execution:** temperature 0. Each query ran read-only through `neo4j_query`,
  next to its reference.
- **Scores:** "rows" means every reference row's identifying value came back
  (accuracy first); "full" means every column too.
- **Empty cases:** 7 cases have an empty right answer, and all three passed 5
  of them by returning nothing.

| Contender | Rows (all 40) | Rows (33 non-empty) | Full | Ran | Median s |
|---|---|---|---|---|---|
| E4B (`gemma4:e4b`, host Ollama, no grammar) | 15 | **10** | 10 | 39 | 4.7 |
| codestral (free Mistral) | 13 | 8 | 9 | 35 | 3.0 |
| Text2Cypher Gemma 3 4B (Q4) | 7 | 2 | 5 | 37 | 2.3 |

**Why E4B missed (25 cases; codestral's misses look the same):**
- **About 15: it did not know where the thing lives in this brain.** It looked
  for briefs in `Workflow` or `Task` instead of the brief notes, for the résumé
  in `Task.goal`, and for agents in note text. Twice it treated "what have we
  learned" as a sentence to find in chat history. The schema names every label
  and value; it cannot say that `source_context: 'scheduled_daily_news_brief'`
  *is* the daily brief.
- **3: the right place, written wrong.** The day was taken as UTC midnight, not
  local midnight (`notes-stored-today`, 104 vs 90), and entity names were matched
  as typed (`{name: 'Adam Kinzinger'}`; stored lower-case).
- **3: the question's words matched as text** ("Biden administration" +
  "emergency-shutdown button"; Nebula).
- **3: the wrong shape:** notes where counts were needed.

Codestral also wrote 4 syntax errors (aggregation scope, a `UNION` with
mismatched columns, APOC that is not installed). The schema guard rejected 2
invented relationships from it.

**Reading:**
- **Free-form Cypher fails on brain knowledge, not on Cypher.** Neither a bigger
  model nor a grammar supplies what "the daily brief" or "the agents" are. That
  is the spec's job (Phase 2): named things (`daily brief`, `agents`, `todos`,
  `monitor findings`, `learned in a window`) mapped once, in code, to the query
  that fetches them. Writing the queries is the smaller problem.
- **Text2Cypher 4B is out:** it ignored most of the schema (it answered "how
  many tools" from `ModelDef`) and wrote 2 of 33 right.
- **E4B is as good as the free cloud model here,** so the local model can stay
  the spec-writer to beat.

**Caveats:**
- The references are ungraded drafts.
- "Rows" rewards a broad dump (E4B passed `mesh-networks` with 182 rows), so
  precision is still to measure.
- The identifying value is a heuristic (the longest text column), and it
  under-scored one case (`apis-known` keyed on `base_url`). An explicit `key`
  per case can fix that while grading.

**Harness note:** the test client first opened a new MCP session per call and
never closed it, and hit the brain's cap ("Maximum sessions reached: 1000";
sessions expire after an hour idle). It now keeps one session and DELETEs it on
exit.

### Phase 2 — views: named things, a note spec, and its compiler (designed 2026-10-13)

Phase 1 decided the shape. Free-form Cypher failed on *where things live in this
brain*, not on Cypher. So the unit of a query is a **view**: a named thing
("the daily brief", "open todos", "the agents"), declared once as data, with
typed slots a model fills. The model never writes Cypher; it picks a view from a
closed list and fills slots that are closed lists or typed values.

#### What the 40 graph cases need

| Family | Cases | Shape |
|---|---|---|
| **Notes**, filtered | 27 | One label (`Note`) under different filters: a time window, note kinds, where they came from (`source_context`), a topic, an entity, words, claim status; returned as rows, a count, or counts by topic/kind/source |
| **Fixed lookups** | 12 | Todos (open, overdue), tasks (recent, a workflow's runs), jobs (queue, stuck), a workflow's schedule, workflows naming unknown tools, agents, tool count, APIs and their endpoints, topics with counts, a claim's evidence, videos ingested in a window |
| Neither | 1 | `where-semantic-notes-went` (retype trail): stays free Cypher |

27 of 40 are one label, so notes get a **spec with a compiler**; everything else
is a **fixed query with parameters**. Both are views.

#### A view is a file

`views/<name>.yaml` in a bundle, loaded like declared tools (`bundle_tools.rs`:
same checks, same read path through `neo4j_query`), but **not registered as
MCP tools**. Thirty views as thirty tools would grow the tool list, and a small
model picks tools badly. The bundle that owns the data owns the view:
`epistemics` the claim views, `media` the video view, `personal` the résumé,
`core` the rest.

```yaml
# kind: notes — a preset over the note spec. `words` are what people call it.
name: daily_brief
kind: notes
description: The scheduled daily news brief, one note per day.
words: [daily brief, news brief, briefing, the brief]
preset: { from: [scheduled_daily_news_brief], kinds: [news] }
defaults: { order: newest, limit: 1 }      # slots the model may still set: window, limit
---
# kind: cypher — a fixed query; its slots are its parameters (the declared-tool rules).
name: overdue_todos
kind: cypher
description: Todos still open whose due date is before today (local).
words: [overdue, late todos, past due]
input_schema: { type: object, properties: {} }
cypher: |
  MATCH (t:Todo) WHERE t.status IN ['pending','in_progress']
    AND t.due_at IS NOT NULL AND date(t.due_at) < date($today)
  RETURN t.title AS title, t.due_at AS due ORDER BY t.due_at
```

The compiler supplies reserved parameters (`$today`, `$now`, `$day_start`), so
no view writes time arithmetic of its own.

#### The note spec (what `kind: notes` compiles)

| Slot | Type | Compiles to |
|---|---|---|
| `window` | `{hours: N}` · `{days: N}` · `{day: today\|yesterday\|YYYY-MM-DD}` · `{month: this\|last\|YYYY-MM}` · `{since, until}` | `$since`/`$until` bounds in **local** time (`clock::local_midnight_utc`), never a string compared to a datetime |
| `on` | `created` (default) · `verified` · `asserted` | which datetime the window bounds |
| `kinds` | list from `note_type` (closed) | `n.note_type IN $kinds`; the word "learned" is a named set (`semantic, claim, source_record, news, reflection, inference`; the owner decides it while grading) |
| `from` | list of source names from the views catalog | `n.source_context IN $from` |
| `topic` | `Topic.name` (closed, live) | `(n)-[:IN_TOPIC]->(:Topic {name: $topic})` |
| `entity` | text | resolved first: lower-case `Entity.name`, then `EntityAlias`; no match → the answer says so and names the nearest entities |
| `words` | text | case-insensitive `CONTAINS` on content (all words) |
| `status` | `unverified\|corroborated\|refuted` | `n.claim_status` |
| `as` | `rows` (default) · `count` · `by_topic` · `by_kind` · `by_source` · `by_day` | the RETURN; rows carry kind, source, status and age (the epistemic labels survive) |
| `order`, `limit` | `newest\|oldest`, 1–200 (default 30) | `ORDER BY`, `LIMIT`, plus the total, so a cut is marked |

- **Every slot is optional** and the empty spec is "the 30 newest notes".
- **An empty result is a finding:** the answer repeats the filter in words and
  gives the count without the window, and without the topic or entity, so "none
  today" and "none ever" read differently.
- **The compiled Cypher is shown with the rows** (inspectable, and Phase 4's
  preview).

#### One tool: `lookup`

- `lookup {view, ...slots}` runs a view.
- `lookup {}` lists the views (name, description, words, slots): the catalog is
  drillable, like `list_capabilities`.
- A wrong slot is refused with that view's slot schema, so the model fixes it
  in one round.
- `neo4j_query` stays, for what no view covers.

#### Who fills it, measured on the same test set

Each test case gains `view:` and `slots:` (what the right lookup is), so picking
the view and filling slots score separately. Contenders:

1. **E4B with constrained output:** the catalog in the prompt; Ollama `format` /
   llama.cpp JSON schema with the view names and closed slots as enums. It can
   only emit a lookup that exists, which is the "only the right query" goal at
   the spec level.
2. **Nimble picks the view** (a closed choice over descriptions and words, the
   alias-judge pattern), then E4B fills only that view's slots.
3. **A cloud model calling `lookup` as a tool** (free Mistral or OpenRouter, not
   Ollama Cloud).

Phase 1's free-Cypher scores (E4B 10/33, codestral 8/33) are the floor. Each view
is run on its own cases before any model writes a spec, so a miss is the
writer's and not the view's.

#### Build order

1. **Bind lists and maps in `neo4j_query`** as real Cypher values; today they
   become JSON text, and `UNWIND $ids` matches nothing (Phase 0). Views need
   list slots.
2. **The `views/` loader** (kinds `cypher` and `notes`; declared-tool checks;
   `words` and `description` required).
3. **The note-spec compiler,** with unit tests on the compiled Cypher. Every
   golden notes case is run through its view and must match the reference
   rows.
4. **The views for the 13 fixed cases and the named note sets** (daily brief,
   monitor findings, deep dives, video notes, résumé, claims, learned).
5. **`lookup`,** then the three writers measured.

**Built 2026-10-13 (steps 1–4):**
- **Lists and maps bind natively in `neo4j_query`** (`bolt_param`, neo4rs's
  JSON-to-Bolt conversion).
- **The loader and `lookup`** are in `crates/host/src/views.rs`. `views/` is a
  merged seed kind, and `manage_plugin` checks view files. `lookup` is
  registered before the declared tools and is in `knowledge-worker`'s
  allowlist, with one prompt line: a named thing or a period → `lookup`.
- **24 views:**
  - core: `notes`, `learned`, `intel_findings`, `deep_dives`, `todos_open`,
    `todos_overdue`, `tasks_recent`, `workflow_runs`, `workflow`,
    `workflows_unknown_tools`, `jobs_open`, `agents`, `tools`, `apis`,
    `api_endpoints`, `topics`
  - epistemics: `claims`, `claim_evidence`
  - media: `videos`, `video_notes`
  - personal (private repo): `daily_brief`, `resume`, `off_grid_findings`,
    `trump_monitor`
- **Windows take simple forms** (`{hours}`, `{days}`, `{day}`, `{month}`,
  `{since, until}`) instead of ISO durations, which is easier for a small model
  to fill.
- **Every golden case gained `view:` and `slots:`.** Run through its view, **36
  of 39 returned exactly the reference rows**. The other 3 were cut at the
  default 30 rows and marked `truncated` with the total.
- **Empty answers explain themselves** (checked live). "No notes of kind news,
  from scheduled_daily_news_brief, created on 2026-04-16. Without the window:
  129." An unknown entity names the nearest ones ("nearest: … adam kinzinger").

**Step 5, who writes the lookup call (2026-10-13).** Each writer turned a
golden question into `{view, slots}`; the call ran through `lookup`, and its
rows were scored against the reference (same measure as Phase 1).

| Writer | View right (39) | All rows (32 non-empty) | ≥ 90% of rows | Median s |
|---|---|---|---|---|
| **E4B, one step**, output constrained to view names and slots, catalog with live values | 35 | **15** | **18** | 2.1 |
| codestral (free Mistral), same prompt, JSON mode | 34 | 16 | 18 | 0.9 |
| E4B, two steps (pick the view, then fill only its slots) | 33 | 11 | 13 | 3.1 |
| Nimble picks the view, E4B fills it (first round, before live values) | 32 | 11 | — | 33.9 |
| *Phase 1: free Cypher, E4B / codestral* | — | *10 / 8 (of 33)* | — | — |

**What changed between rounds:**
- **Live closed values.** The first round's misses were names a model cannot
  guess: `entity: "mesh networks"` for the topic `off-grid-networking`,
  workflow `daily_brief` for `daily-news`, an invented source `knowledge`. The
  catalog now gives the `topic`, `from` and `kinds` slots their live values,
  and a cypher view's slot may declare `values_from: <query>` (used by
  `workflow`, `workflow_runs`, `api_endpoints`). An unknown topic or value is
  refused with the list. After this, E4B named topics and workflows right.
- **`words` matches any word, ranked by how many a note holds** (a count or
  breakdown still needs all of them). E4B passed the whole question ("Biden
  administration emergency-shutdown button powerful AI models"), and an
  all-words match missed the Newsom claim. It now comes first.
- **Empty values are unset slots** (`entity: ""`, `kinds: []`, `window: {}`),
  and a list of strings for a text slot is joined (codestral sent `words` as a
  list).

**Reading:**
- **Views beat free Cypher:** E4B went from 10 to 15 of 32, and 18 counting
  near-complete answers. The constrained local model ties the free cloud one,
  so E4B with constrained output is the writer.
- **Nimble is out for this.** It picked worse (32 of 39) and cost 34 s a call
  swapping models on host Ollama.
- **Two steps is worse:** without the other views in view, E4B picked
  `learned` for "claims from the last 2 days".
- **E4B's remaining misses (14):**
  - 5 are reference shapes the owner has not graded: "learned in two weeks /
    this month / September" as counts, notes counted by type, and "all
    briefs" at 200 rows.
  - 2 are the 30-row default, marked `truncated`.
  - The rest are over-filling: slots the question did not ask for
    (`status: unverified`, `window: today`, a list of 40 sources) and one
    ambiguous reading ("a couple days ago" meant the conversation, not the
    window).

**Not in Phase 2:**
- Folding `recall_recent`, `recent_tasks`, `list_tasks`, `list_apis` and
  `list_capabilities` into views (they are views in all but name). That happens
  after measurement, with Phase 4.
- Search by meaning as a view slot (the `both` cases use `words` until Phase 3).

**Decisions (the owner, 2026-10-13):**
1. One tool, named `lookup`.
2. Default `limit` 30.
3. "Learned" means the information the brain ingested, and anything not
   corroborated says so. The named set stays as drafted for now and is
   iterated later. Every row carries its standing: a claim shows its
   `claim_status`, and a note no check has looked at says "not checked".

#### Grading the shapes (the owner, 2026-10-14)

The owner graded the five cases whose right answer was a choice of shape:

| Case | Graded answer |
|---|---|
| learned in two weeks, this month, September | counts by topic; a `(no topic)` row counts the untopiced |
| what notes are stored (all 23,053) | counts by kind |
| summarize all the briefs (129, over a million characters) | the index of days (`as: by_day`); the reply offers a map-reduce job |

**What grading changed in the code:**
- **`by_topic` now counts untopiced notes as `(no topic)`.** Before, it dropped
  them: 3,376 of September's 6,543 learned notes have no topic, and the counts
  hid them.
- **`lookup` says when it cut off an answer.** `neo4j_query`'s default
  `LIMIT 100` cut aggregates silently. The brief index stopped at 2026-09-16,
  and the 403 sources showed as 100. Aggregates now run to 1,000 groups, and a
  cypher view's limit is marked: `run_capped` asks for one row more than the
  limit.
- **A notes view can declare `wide: <by_* shape>`.** A call that asks for no
  shape and matches more notes than the limit gets that shape instead, with a
  `wide` line saying why. `learned` declares `wide: by_topic`. The same rule
  written in its description was ignored 6 of 6 times by E4B and codestral; in
  `notes` and `daily_brief`, the description sentence worked.

**Re-measured (all 39 view cases, references now graded where it mattered):**

| Writer | View right | All rows (of 30 non-empty) | ≥90% of rows | Right empty (of 9) | Median |
|---|---|---|---|---|---|
| E4B, constrained | 34 | 18 | 20 | 8 | 2.1 s |
| codestral | 34 | 18 | 19 | 9 | 0.9 s |

E4B gets all seven "learned" and notes cases. Its remaining misses are
over-filling (a `status`, a `topic` the question did not ask for), the 30-row
default on 3 cases, one JSON cut off at 300 tokens, and questions that go
through an entity or words where a topic view holds the answer.

The other 42 cases (34 of them with a view) are still `status: draft`. Their references were written to
match what the views return, but the owner has not graded them.

### Phase 3 — the right tool for the turn (rewritten 2026-10-14)

**The owner's aim:** use the right tool for the task. That means not the graph
when the question needs the web, and not the web when the answer is stored,
and so on for every tool. Requests are loose ("what did we learn today", "our
Opensource Intelligence Agency"), and the brain should work through that
ambiguity to fulfil the request instead of failing on it. The "graph needed?"
gate first drafted here was one corner of this.

**Where the choice happens today:**
- **The front door (`GeneralDefault-Orchrestator`) holds no retrieval tools.**
  It delegates to an agent (`run_agent`), and the agent picks the tools. An
  agent's allowlist decides what it can do: `researcher` can only search the
  web, and `knowledge-worker` can only look in the graph. So the first wrong
  turn is usually the agent, and the right tool is then out of reach.
- **The intelligence-agency turns (about 13, 2026-08-08 to 10-12, under three
  names).** Two turns searched the web (2026-09-07). Every turn ran
  `search_notes` again and again, up to 4 times, matching words, while the
  answer is a named thing: the `nia` topic, its workflows and their runs. The
  brain does not know its own names: "Opensource Intelligence Agency" never
  resolves to `nia`.

**Ambiguity, by a default order instead of a question back:**
1. A name the brain owns (a topic, workflow, agent or monitor) is looked up first.
2. Then what is stored.
3. Then the web.
4. Ask the owner only when the readings would lead to different actions
   (send vs draft).

**Factual questions, the owner's rule (2026-10-14).** Answer from what the
model knows, with a confidence. Below 85%, check the graph, then the web
("Do I already know this? How certain am I? Below 85% I fact-check myself").
- A question about us ("our agency", "what we learned") always checks:
  training data cannot know it.
- So does a fact that changes (news, versions, prices, "latest").
- A model's stated confidence is not trusted until it is measured. On the test
  set, its answers at "≥ 85%" must be right at least 85% of the time, or the
  threshold moves. Confidence in the answer is a different number from
  confidence in the route.

**Two of our own things with one name** (topic `nia` and the intelligence
agency watch): look up both and label which is which (owner, 2026-10-14).

**Steps:**
1. **Mine the turns.** For each chat turn, record the agent it went to, the
   tools that ran, and how it ended. Draft what it should have used: the
   agent, and the first tool. The owner grades the drafts one at a time, as with
   the shapes. Then the test set goes in `eval/routing_golden.yaml`.
2. **Name resolution.** The brain's own names, with aliases (`nia` = "Niche /
   Opensource Intelligence Agency"), as a closed list for the router. The topic,
   workflow and agent names come from the graph already (`lookup`'s live
   values). Aliases are data, on the node they name.
3. **Measure routers on the test set:** `nimble` (a choice with probabilities),
   E4B constrained to the roster, and bge-m3 similarity to example turns. Score
   the right agent and the right first tool. Check whether Nimble's
   probability separates its right picks from its wrong ones: a confident pick
   goes, a split runs both readings in the order above, and a flat one asks.
   Nimble (9.5 GB) sits mostly in system RAM beside the llama.cpp E4B, so its
   latency is measured warm, alone, before it is ruled in or out. The views
   test (34 s) swapped it with E4B on host Ollama, so that number does not settle it.
4. **Wire the winner into the front door,** with the order above for a split pick.

**Step 1, mined 2026-10-14.** The chat logs each turn cleanly. The turn
opens with `push_context` (role user) and closes with the
`Chat turn [session: …] — profile: …` note, and `model_usage`
`subagent:<agent>` rows inside it name the agents. That gives 324 turns from
2026-06-12 to 10-12, 149 distinct questions. Profiles: the front door 157,
`general` (every tool itself, before delegation) 125, `orchestrator` 30. 111
turns called no tool. The draft set is `eval/routing_golden.yaml`: 67 cases,
each with a route (an agent, a front-door tool, or `answer`), the first tool
inside it, `then` for the next step by the order, `readings` for 16
ambiguous turns and `decide` for 14 choices. The routes are knowledge-worker
43, researcher 7, answer 6, code-analyst 3, and 8 others.
- **The brain's first stop agrees with the draft on 49 of 67.** Of the 18
  misses, 12 answered with no tool where a lookup was needed (tool and agent
  counts, the todo list, "learned in 24 hrs" asked 19 times, "this agency").
  One sent an advice question through tasks and the web. One applied to a job
  without `job-applicant`.
- **Within the right agent, the tool was the bigger miss.** The NIA turns ran
  `search_notes` up to 5 times where topic `nia` holds 35 notes. That half is
  graded with `first_tool`.
- **Two agencies share a name.** Topic `nia` is the owner's program (Niche, or
  Opensource, Intelligence Agency). The intelligence agency *watch*
  (`intelligence_agency_watch`, view `intel_findings`) monitors claims about
  real agencies. The 2026-09-24 session crossed them ("this agency's findings
  on NHI").
- **Graded 2026-10-14, the 14 `decide` cases (13 accepted, 1 dropped).**
  Three rules came out of them, and they now carry most of the routing:
  - **About us always checks.** A question about our things (claims, means,
    agencies) is looked up before it is answered, even when it asks for advice
    ("dig deeper or wait?", "what should our next moves be?").
  - **The tool that answers correctly, not the nearest.** The front door's
    `list_tasks` ignores its limit and lists failures first, so "the last 5
    tasks" goes to knowledge-worker's `lookup tasks_recent`.
  - **Check what exists before creating.** "We need to be monitoring such
    things" extends the Trump monitor (topic `nia`) instead of a new
    workflow. Workflows already share a topic.

  Also graded: "a couple of days" is 3; "what could we learn more about" is
  unverified claims by topic, then thin topics; investigating SLMs for the
  system goes to deep-diver (code, notes and web); an OpenAPI doc is
  api-integrator alone; an advice question never writes. The job-search case
  was a test session with no roles in it, so it is dropped. 53 cases without a
  `decide` line are still drafts.
- **Step 2 built 2026-10-14: topic aliases.** A topic's `aliases:` (in YAML,
  or through the topic API, which keeps them when an edit leaves them out) are
  stored on the node. `lookup` runs a topic given by name, label or alias, in
  any case, and its answer says what it resolved ("topic 'our intelligence
  agency' is 'nia'"). The catalog lists the aliases with the label, so a
  writer or router sees them. Seeded from the owner's chat wording: `nia`
  (Opensource / Open Source Intelligence Agency, our intelligence agency),
  `off-grid-networking` (mesh networks, the off-grid project) and `ai-ml`
  ("AI"). Agents, views and workflows already have readable names or `words`.
- **Step 3 measured 2026-10-14: routers on the 66 cases** (one dropped). Every
  router chose from the live roster (11 spawnable agents with their
  `specialty`), the front door's own tools, and `answer`. A follow-up carried
  the turn before it. `rules` adds the routing rules graded above. Those rules
  came from grading these same cases, so `rules` scores flatter themselves;
  `roster` is the clean number.

  | Router | roster | rules | median s | Picks at ≥ 0.85 conf (rules) |
  |---|---|---|---|---|
  | E4B, constrained (host Ollama) | 42 | **52** | 1.4 | 66 of 66 at ≥ 0.85, 79% right: no signal |
  | codestral (free Mistral) | 41 | 50 | 0.8 | 62 at ≥ 0.85, 79% right: no signal |
  | **Nimble** (warm, alone) | 31 | 48 | 5.0 | **36 at ≥ 0.85, 89% right; below, 57% and 43%** |
  | bge-m3 kNN over the other cases | 46 | — | 0.01 | learns only from this set |
  | bge-m3 to the roster text | 8 | — | 0.04 | — |

  - **Nimble's probability is calibrated at the owner's threshold.** Its
    confident picks are right 89% of the time. E4B's and codestral's stated
    confidence is ~0.95 whether right or wrong. Warm and alone, Nimble takes
    3.7–5 s, not the 34 s of the views test (that run swapped it with E4B).
  - **E4B picks best; Nimble knows when it is unsure.** Where they agree (55
    of 66), the pick is right 45 times (82%). Where they disagree (11), the right
    route is one of the two picks in 10. So: agree → go; disagree → run both
    readings in the order (stored first), as the plan's split rule says.
  - **The roster alone sends us to the web.** Without rules, Nimble sent 16
    questions about us to `researcher` and E4B sent 8, because
    knowledge-worker's specialty ("what the brain already knows") reads weaker
    than researcher's. That is the owner's original complaint, measured.
  - **What the rules do not fix:** something in the world that we have also
    stored ("Have you looked at Nebula", "tell me more about Adam Kinzinger",
    the emergency-shutdown claim, the 60 Minutes video, "dig deeper into these
    claims") still goes to the web first, from both models. The fact rule says
    stored before the web for these too.
  - In production, E4B runs in the llama.cpp sidecar and Nimble in host Ollama,
    so the two can run side by side: about 5 s added to a chat turn.
- **Step 4 built 2026-10-14 (owner: E4B + the rule now, Nimble switchable).**
  Two constraints were measured first:
  - **Nimble and bge-m3 cannot share host Ollama's GPU slice.** Each evicts the
    other: Nimble reloads in about 15 s, bge-m3 in about 4 s.
  - **CPU-only Nimble coexists but costs ~8.5 s a call**, because the CPU reads
    the whole prompt each time.

  On the 66 cases, Nimble's added value came to 4 fewer confidently wrong
  picks and 3 more right routes tried, at 16 two-step turns instead of 5.

  - **The profile's `routing:` block holds the router's data.** That is the
    rules, the non-agent choices (the front door's own tools, `answer`), the
    stored and web agents, the `web_words` pattern and `confident`. The front
    door (`GeneralDefault-Orchrestator`) declares one.
  - **The router runs in `crates/chat/src/chat/route.rs`.** The picker is the
    local E4B, its output held to the choice list. A web pick that the turn
    did not ask for goes to the stored agent first. The checker
    (`CHAT_ROUTER_CHECKER=nimble`, off by default) splits a disagreement,
    stored first. A failure or timeout leaves the turn unrouted.
  - **The pick goes into the prompt** as a short block. It also shows in the
    stream (`⚙ route: …`) and is logged to DuckDB as `route: {…}`, so later
    mining can check whether the front door followed it.
  - **Named topics:** a turn or a delegated task that names one of our topics
    (by name, label or alias) says to read it with `lookup`.
  - **Live, 2026-10-14:**
    - "What about our Opensource Intelligence Agency" routed to
      knowledge-worker in 0.5 s. The front door passed "(topic: nia)", and the
      worker's first call was `lookup {topic: nia}`. Before, it ran five word
      searches and said nothing was recorded.
    - "Have you looked at Nebula" went to stored knowledge first and was
      answered from the stored notes, where both models had sent it to the web.
- **Found on the way:** a `lookup` total counted all the words while its rows
  matched any of them, so 30 rows showed "total 12" and no cut. A count taken
  for rows (the total, a `wide` breakdown) now matches the way the rows did.


### Phase B — behaviors first, the agent loop as the fallback (the owner, 2026-10-14)

**The direction.** The owner pointed to swamp-club.com/behavior as a good
description of the goal. Its argument: use frontier intelligence at *build
time* to encode intent into behaviors with typed inputs and structured
outputs, and at runtime call a model only for one narrow job (pulling the
parameters out of a request). "Each LLM call is scoped to a single,
well-defined job and never drives the control flow of the broader system."
(The article is Swamp's own pitch, and its footnote says the cost numbers
compare different models and setups. The principle stands on our own
measurements.)

**Where the brain is already there:**
- **Workflows** run their steps verbatim, with arguments validated against
  each tool's schema.
- **Views behind `lookup`**: a named thing with typed slots and live closed
  values, and the Cypher is written by code. Free Cypher from E4B got 10 of 33
  right; filled slots got 18 of 30.
- **Rules a model was measured to ignore became mechanisms:** `wide`
  (ignored 6 of 6), stored before the web, names resolved through aliases,
  marked cuts.

**Where it is not:** the chat path. The front door decides, a sub-agent
loops, and the sub-agent picks its tools. On 2026-10-14 knowledge-worker
searched "Opensource Intelligence Agency" five times and concluded nothing
was stored, until a deterministic `lookup` line was added. The Phase 3 router
is one of the patches the article lists ("model routers, sub-agents"): it
picks from a closed list under rules in code, but it still hands the turn to
an agent that improvises.

**The plan:**
1. **The router picks a behavior when one fits.** A behavior is a view the
   routing block names, such as `lookup:learned`. For one, code runs the turn:
   - E4B fills the view's slots, held to the view's slot schema (the extract
     step);
   - `lookup` runs;
   - the rows, with the cuts marked, go into the front door's prompt as the
     turn's context.

   The front door's model only phrases the answer, and keeps each row's
   standing. No sub-agent, and no tool choice at runtime.
2. **The agent loop is the fallback** for a turn no behavior fits, or a
   behavior that fails.
3. **The route log is where new behaviors come from.** A fallback turn is a
   candidate. Behaviors are written at build time (by a session like this
   one, toolsmith or schedule-author), graded by the owner on
   `eval/routing_golden.yaml`, and then added to the routing block.
4. **Typed outputs for workflows** (later). Workflow results are free text
   today; that is the biggest gap against the article's versioned schemas.

**First behavior: "what did we learn today"**, asked over 40 times in
chat, the most of any question.

**Built 2026-10-14.**
- **A behavior is data in the routing block:** `{name, view, says, slots,
  when, unless}`. The router offers it as `lookup:<name>`. When it is picked,
  code runs it:
  - E4B fills only `slots`, held to the view's slot schema;
  - `lookup` runs;
  - the rows (marked when cut, widened or empty, each with its standing) go
    into the prompt;
  - that turn's front door has no `delegate`, so it can only phrase the rows.

  A behavior that fails sends the turn to the stored agent.
- **Code decides that a behavior fits; the model only that it is wanted.** As
  a bare choice, `learned` drew 8 turns it did not fit ("what have we
  learned about the Trump analysis / mesh networks / NHI"), and overall routing
  fell from 52 to 42. A `when` pattern (the turn names a time window) and an
  `unless` pattern (it names something narrower: videos, claims, monitors,
  search engines) fixed that. Scored as the live router decides, 57 of 66
  first stops are right (56 without the behavior), and the behavior runs on
  exactly the 6 learned-in-a-window turns. These are regex guards, the first
  use for Phase R. The patterns were written knowing these cases, so live
  turns are the real test.
- **The prompt rule alone did not hold.** On the first live run the front door
  got the rows and delegated anyway: its prompt says "your ONLY option is to
  call delegate". Removing `delegate` for that turn made it phrase the rows.
  It then pasted a row with its header, so the context now says to write the
  answer in its own words and give the count first.
- **Live:**
  - "learned this month" took 1.7 s to the behavior and 3.8 s to the answer,
    against ~22 s through the agent loop, with counts by topic and `(no
    topic)`;
  - "learned in the last 24 hrs" filled `{hours: 24}` and gave 27 rows in
    0.9 s, summarized with standing;
  - "learned today" filled `{day: today}` and found 1 note, said so, and
    labelled it not checked;
  - "learned about mesh networks" fell back to knowledge-worker, which went
    straight to `lookup topic: off-grid-networking`.

  The first behavior after a restart takes ~11 s: the catalog's live values
  scan the notes cold.
- **The loop for new behaviors, built 2026-10-14:**
  - **One record per turn.** Each front-door turn logs one `turn:` row to
    DuckDB `interactions`: the route, the behavior's call, every tool call
    (sub-agents' included, from the stream) and whether it answered.
  - **The owner's thumbs.** HBI chat answers take a 👍/👎 (owner, 2026-10-14):
    `POST /api/sessions/{id}/feedback` sets `feedback_score` on the answer's
    working-memory entry and logs a `feedback:` row. A reloaded session shows
    it.
  - **The weekly report.** `behavior-candidates` (weekly) is one DuckDB query
    and a notification, with no model:
    - a fallback group with 3+ turns, the same first call, and no more 👎
      than 👍 is a CANDIDATE;
    - a behavior with a 👎 is CHECK;
    - the report is rendered in SQL and opens a `behaviors-<date>` chat
      session for the reply.

    A local model given the same rows to write up mis-sorted them, dropped
    one, and contradicted itself, so the SQL renders the report. Writing the
    behaviors stays at build time, with the owner. First run 2026-10-14:
    0 turns (logging began that day).
- **The first 👎 (2026-10-14): right route, nothing to find.** "What have we
  learned today?" ran `learned {day: today}` and found 1 note, the health
  monitor's own analysis, and the answer described it. Three things were
  wrong underneath, none of them the router:
  - **The brain had nearly stopped learning.** Notes per day went 174, 252,
    260, 228 (10-07 to 10-10), then 88, 40, 27, 1. The boot protocol's
    `manage_workflow audit disable_broken` ran during the 10-11 plugin carve
    while `poll_media_sources` was briefly unregistered and paused
    `media-watch`, the poller that feeds `video-learning` and
    `article-learning`. The tool came back and the pause stayed for three
    days. The audit now lifts a pause it made once every tool is live, and
    the health monitor reports non-episodic notes per 24 h against the 7-day
    rate, plus any audit pause.
  - **The brain's upkeep counted as learning.** The owner: maintenance
    inferences are not learned. A notes view takes `except_work_kinds`, and
    `learned` leaves out notes a `maintenance` workflow's task produced.
    Notes from before `produced_by_task` was set (10-11) can't be traced and
    stay.
  - **One note read as all there is.** A short window (a day, up to 24 h)
    that finds under half its usual share gets `thin`: the count against the
    7 days before, by day (today's share is the part of the day gone by).
    The behavior's context carries it and asks the answer to say so.

  The 👎 itself was stored as -1 and read back as 255. neo4rs 0.8.0 decodes
  a negative tiny int (-1..-16) unsigned, so every read of one in the brain
  was wrong. `vendor/neo4rs` carries the one-line fix 0.9 makes, until 0.9
  is stable.
- **Next behaviors:** from the route log, a fallback turn at a time.
  Candidates from the routing set are the open todos, the APIs, an API's
  endpoints, the tool and agent counts, and the claims in a window.

### Phase 4 — workflow steps hold specs, the frontend shows them

- A workflow step stores the spec instead of escaped Cypher; the HBI workflow
  editor shows the spec, the compiled Cypher, and a "run it now" preview of the
  rows.
- The 18 workflows with raw Cypher are migrated where the spec covers them;
  the rest stay raw and show formatted Cypher.

### Phase 5 — fine-tune (only if Phase 1–2 show the gap is writing, not choosing)

- Data: `text2cypher-2025v1` plus our own pairs, made from the schema: generate
  valid queries or specs, run them, keep the ones that return rows, and have a
  cloud model write the question each one answers.
- Follows `FINE_TUNING_PLAYBOOK.md` (GPU lease, the brain rests). A narrow task
  on a fixed schema, so better odds than experiment 1 (Rust), but measured the
  same way: against the base model on the held-out test set.

### Phase R — regex as an advantage (to explore; "a tomorrow problem", the owner, 2026-10-13)

Regex is deterministic and recall-first: it finds every match of a pattern, which
is what "accuracy first" asks for, and precision can be added on top. Questions
to answer before designing anything:

- **Where embeddings blur exact strings:** CVE and case numbers, dollar amounts,
  dates, ticker symbols, names spelled several ways, URLs and domains. Is a
  regex leg in hybrid retrieval (next to vector and BM25) worth it, and for
  which note types?
- **Where it already exists:** Cypher `=~`, DuckDB `regexp_matches` /
  `regexp_extract`, Rust `regex`. `=~` over large text properties is a full
  scan, so measure its cost on the note count before relying on it.
- **Who writes the pattern:** a model writing a regex has the same problem as a
  model writing Cypher. The same answer may apply: patterns from a vetted
  library (named, tested), picked and filled rather than written free-form.
- **As a checker:** regex over model output and logged queries (Phase 0's
  suspicious-empties scan is the first use).
- **Already in use (2026-10-14):** the router's `web_words`, behaviors'
  `when`/`unless`, and topic-name matching are regex guards that decide in
  code what a model was measured to get wrong.

**Step 1, measured 2026-10-14 (`phaseR/exact_eval.py` in the scratch harness).**
- **Exact strings are everywhere in the notes.** Of 23,083 notes: URLs 9,411,
  ISO dates 4,049, written dates 2,015, dollar amounts 1,143, percentages
  1,143, `ABC-123` codes 1,784, version numbers 756, model names 298, CVEs 4.
- **A scan is cheap.** `toLower(content) CONTAINS` over every note takes 0.16 s,
  and a Cypher `=~` regex takes 0.5–1.3 s. Accuracy first is affordable in a
  chat turn.
- **Today's search loses exact strings.** I sampled 54 real strings, each in
  1–15 notes (dollar amounts, dates, codes such as GBU-12 and ICD-10, model
  names, versions, URLs, CVEs). This is the share of the notes holding each
  string that came back in the top 10:

  | Retriever | Mean recall | Found at least one |
  |---|---|---|
  | `search_notes`, the string inside a question ("What do we know about $17.3 billion?") | **0.28** | 22 of 54 |
  | `search_notes`, the bare string | 0.45 | 35 of 54 |
  | Lucene full-text index, the string as a quoted phrase | 0.95 | 53 of 54 |
  | A plain scan (`CONTAINS`) | 1.00 | 54 of 54 |

  By kind, inside a question, `search_notes` found:
  - dollar amounts 0.12, ISO dates 0.04, written dates 0.17, versions 0.00,
    percentages 0.00, codes 0.25, model names 0.30;
  - CVEs 1.00 and URLs 0.67, the only ones it handled.
- **Why:** the BM25 half sends the whole question as loose words ("17.3" OR
  "billion"), so every note about billions competes. The vector half blurs
  numbers and codes, and merging the two buries the exact match. The phrase
  index already exists; the search does not ask it for a phrase.

**Step 2, proposed: an exact leg in hybrid retrieval.**
1. **Find the exact spans in the query with a vetted pattern library**: named,
   tested patterns for money, dates, codes, versions, model names, URLs and
   CVEs, plus anything the user put in quotes. The patterns are data with
   examples, written once at build time; no model writes a pattern. This is
   the plan's "picked, not written" answer for regex, as for Cypher.
2. **Find each span as written:** a Lucene phrase query first (indexed, 0.95),
   then a `CONTAINS` scan when the phrase finds nothing (0.16 s, 1.00).
3. **Notes holding the exact span rank first**, marked `exact` in the search
   trace. Then the hybrid list follows as today. A note that holds what was
   asked for, word for word, is the most accurate answer there is.
4. **Measure both ways:** this set (the target is ~1.0 recall) and
   `eval-retrieval` on the retrieval golden set, so ordinary questions do not
   get worse.

**Step 2, built and measured 2026-10-14.**
- **The library:** `plugins/core/exact-patterns/core.yaml`, 10 patterns
  (money, ISO and written dates, CVEs, upper-case codes, versions, model
  names, percentages, URLs, quoted text). Each carries `matches` (a text and
  the one span it must give) and `rejects`; a pattern failing its own
  examples is skipped at load with a WARN. A merged seed kind, so a plugin
  can add patterns. A query gives at most 4 spans, longest first, a span
  inside a longer one dropped (`knowledge/exact.rs`).
- **The leg:** each span is a Lucene phrase, kept where the text holds it
  literally, plus a `CONTAINS` scan for what the index missed. The scan runs
  every time: with the phrase alone 6 notes were still missing (a date inside
  a timestamp tokenizes differently). Notes holding more spans rank first,
  then the hybrid list as before. `MatchTrace.exact` says how many spans a
  note held.
- **Result, the same 54 strings:**

  | `search_notes` | Before | After |
  |---|---|---|
  | The string inside a question | 0.28 (22 of 54 found any) | **0.95** (54 of 54) |
  | The bare string | 0.45 (35 of 54) | **0.95** (54 of 54) |

  Every remaining shortfall is a chunk of a note that was returned (the
  truth set counts chunks separately), so by note it is complete.
- **Ordinary questions unchanged:** `eval-retrieval` 0.880 / MRR 0.740 before
  and after. A query with spans costs ~0.18 s more (0.31 s against 0.11–0.14
  s); a query without spans costs nothing.

## 4. Decisions so far (the owner, 2026-10-13)

- The compiled spec is the direction ("we need the queries to be more
  deterministic").
- Try the Text2Cypher dataset and models.
- Write the plan before building; Phase 0 is next.
- Regex is part of the work, explored after the plan is written.
- The cloud baseline does not use Ollama Cloud (rate-limited).

## 5. Open questions

- Does the owner grade the test set alone, or does a cloud model draft the
  questions for the owner to correct?
- Does the router (Phase 3) run on every chat turn, or only for the front door's
  profile? Nimble's first call loads ~9.5 GB from disk, so a cold chat turn
  waits for it.
- Does the spec replace `neo4j_query` in agent allowlists, or sit beside it?
