# Graph Context Plan — the right context is a query away

**Status: planned 2026-10-13, nothing built.**

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
| Logged `neo4j_query` calls | 10,022 (of 62,612 interactions) |
| Distinct queries | ~3,609 (Cypher pulled out with a rough regex) |
| Errors | 49 |
| Rejected by the schema guard | 10 |
| **Zero rows returned** | **3,822 (38%)** |

**Reading:** syntax is almost never the problem; a query that runs fine and returns
the wrong rows is. Many of the empty results are legitimate (deletes, existence
checks, real empties). But an empty result is also what a quietly wrong query looks
like: a datetime compared to a string (null, row dropped; `schema.md`
§ Timestamps), a property name slightly off.

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

### Phase 1 — baselines on the test set

- Text2Cypher Gemma 3 4B (GGUF on host Ollama, `num_ctx` set).
- Local E4B with the schema in the prompt and a GBNF grammar on the sidecar
  (labels, relationship types and properties as closed lists), so it can only
  write Cypher with names that exist.
- A cloud model as the best case (when quota allows: the Ollama Cloud
  monthly quota is spent until the reset; free Mistral/OpenRouter otherwise).
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

## 5. Open questions

- Does the owner grade the test set alone, or does a cloud model draft the
  questions for the owner to correct?
- Should the gate (Phase 3) run on every chat turn, or only for the front door's
  profile? Nimble's first call loads ~9.5 GB from disk, so a cold chat turn
  waits for it.
- Does the spec replace `neo4j_query` in agent allowlists, or sit beside it?
