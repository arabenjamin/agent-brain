# Graph Context Plan — the right context is a query away

**Status: planned 2026-10-13; Phase 0 mined 2026-10-13 (§3), the test set
(`eval/cypher_golden.yaml`, 47 draft cases) awaits the owner's grading.**

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

### Phase 2 — the query spec and its compiler

- A small JSON spec, designed from the Phase 0 shapes: start (label, or "search
  by meaning" with the brain embedding the text), filters, a path to follow, a time
  window, fields to return, an order and a limit.
- The compiler writes Cypher with UTC datetime handling, a default `LIMIT`,
  read-only, and names checked against the live schema. Its output is
  inspectable (the Cypher it wrote is shown with the rows).
- Whatever writes the spec (E4B with a JSON schema, or Nimble picking a template
  and its slots) is measured on the same test set against Phase 1.
- Free Cypher stays as a fallback for what the spec cannot express, through the
  existing schema guard.

### Phase 3 — the "graph needed?" gate

- Before the model answers, a classifier names the context a turn needs:
  structural, by meaning, both, or none. The brain fetches it and puts it in
  front of the model (the graph-first block before `search_web`, extended to all
  of chat).
- **Candidate:** `nimble` (a closed choice with probabilities, the alias-judge
  pattern). **Baseline to beat:** bge-m3 similarity to example turns.
- **Test set:** chat turns where context was missing, flagged by the owner, plus
  turns that rightly needed none.

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
- Should the gate (Phase 3) run on every chat turn, or only for the front door's
  profile? Nimble's first call loads ~9.5 GB from disk, so a cold chat turn
  waits for it.
- Does the spec replace `neo4j_query` in agent allowlists, or sit beside it?
