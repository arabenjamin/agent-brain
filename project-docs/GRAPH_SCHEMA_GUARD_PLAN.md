# Graph-Schema Guard + Deterministic Query Tools

**Status:** Phase 1 + Phase 2 implemented 2026-09-10 (uncommitted on `dev`).

## Why

A chat-turn evaluation on 2026-09-10 (worker `gpt-oss:120b-cloud`, voice
`gemma4:31b-cloud`) found a recurring **quality** failure — zero errors in
`model_usage`, but the model *missed things* and narrated the gap as fact.
Verified against the live graph:

| Question | Model said | Ground truth |
|---|---|---|
| "last 5 tasks that ran" | 5 stale consolidation/UI tasks + *"completion timestamps were not stored"* | actual last 5 were job-search + gap-fills + video-watch; `Task.updated_at` **is** a native DATETIME on every node |
| "reflection notes today" | "none found" | 4 reflection notes existed |
| "what APIs do we have?" | "no concrete, fully-ingested API resources" | 3 ingested contexts: agent-brain (48 endpoints), aidevboard (27), home_assistant (20) |
| "do we know HomeAssistant?" | only found it **after the user pushed back** | it was there all along |

The pattern is identical every time: for a **structural / temporal / typed /
aggregate** question the model reaches for semantic `search_notes` (which cannot
count, order, or list), gets a thin result, then reports absence confidently —
including a fabricated schema claim ("timestamps weren't stored"). This is the
same disease `recall_recent` fixed for "what did we learn today", resurfacing
for other structural shapes.

**A smaller/local model will not fix this** — the failure is query construction
and tool selection, where a ~4B local model is weaker, not stronger. The fix is
deterministic tools + a schema the model can see + an enforcement guard, all
**model-agnostic**, so a later move of knowledge lookups to local `gemma4:e4b`
becomes *safe* (the tools carry query correctness the small model no longer has
to invent).

## What "enforce correct Cypher" can and cannot mean

It cannot mean *prove the query answers the question* — you can't statically
know `ORDER BY updated_at` was intended. Exactly like `services/validation.rs`
(structural, not semantic), the guard enforces a **known bad-set**:

- unknown node **label** / **relationship type** referenced in a read
- reference to a **property** no node of that label carries
- the **datetime-vs-string** comparison anti-pattern
- (already in `neo4j_query`) write keywords in readonly mode

A linter over a known bad-set, not a prover.

## Design

**Schema source: live-introspected at boot, never a hand-authored file.** The
graph *is* the source of truth (honors the no-hardcoded-data rule and never goes
stale). `services/graph_schema.rs::introspect()` runs `db.schema` metadata
procs; lazily cached on first use in `QuerySkill`. Fails open (guard skips if
introspection fails).

### Phase 1 — deterministic tools + visibility (fixes every observed miss)

All in `QuerySkill` (`skills/query.rs`) — it already owns `neo4j` and is
registered in both the registry and the handler vec:

- **`describe_graph`** — returns the live compact schema (labels, properties,
  which are temporal, relationship types). Lets the model *see* that
  `Task.updated_at` exists instead of inventing its absence.
- **`recent_tasks(limit, status?)`** — bakes `ORDER BY t.updated_at DESC`. The
  session-A miss, gone.
- **`list_apis()`** — the `(:ApiContext)-[:EXPOSES]->(:ApiEndpoint)` aggregate.
  The session-B miss, gone.

Profiles (`contexts/general.yaml`, `contexts/knowledge-worker.yaml`): add the
three tools (+`neo4j_query` to knowledge-worker, which lacked it — the exact
reason a *delegated* structural question could not be answered), plus two
position-zero rules:

- **STRUCTURAL QUERY RULE** — counts / "last N" / "what X do we have" / a
  specific note_type go to a deterministic tool or `neo4j_query` with the right
  MATCH/ORDER BY, never semantic search.
- **ABSENCE RULE** (fix #2, the behavioural half of the grounding guard) — never
  report "not found / none / not stored" off a semantic search alone; a negative
  claim about the graph requires a deterministic query that actually ran this
  turn (the tool-trace receipt already shows what ran).

### Phase 2 — the Cypher guard

`services/cypher_guard.rs::check(cypher, &schema, readonly) -> Vec<Violation>`
(reuses `validation::{Severity, Violation}`), called inside
`neo4j_query` before dispatch:

- **readonly reads only:** unknown label / unknown rel type → **Hard** reject
  (a read of something that cannot exist is a bug; naming the valid set lets the
  model self-correct). Skipped for writes — `CREATE`ing a new label is legal.
- **any mode:** datetime-vs-string literal, and a `var.prop` where `var:Label`
  is bound and `prop` isn't in the label's introspected properties → **Soft**
  (advisory; introspection only sees properties present on ≥1 node, so a
  brand-new optional property must not hard-block). Soft violations ride out as
  a `warning` in the result envelope.

Fails open: no schema (introspection failed) ⇒ no guard, query runs.

## Not done / follow-ups

- Relationship endpoint-pair validation (`(:A)-[:R]->(:B)` shape) — only rel
  *types* are checked now; endpoint pairs need a data scan or `db.schema.visualization`.
- A hard code-level block on absence claims (vs the prompt rule) — deferred; the
  receipt + rule is the shippable form tonight.
- Routing knowledge lookups to local `gemma4:e4b` — **wired** 2026-09-10 (a
  `llamacpp` branch in `config_for_catalog_entry`, `LlamaCpp` in the sub-agent
  unmetered clamp, `LlamaCpp`→`run_ollama_cloud_loop` dispatch, a `gemma4-e4b`
  catalog entry, and the spawnable `contexts/knowledge-local.yaml`). **Unverified
  until the probe** (`scripts/probe_e4b_subagent.py`, run after a rebuild) shows
  E4B actually tool-calls across rounds; `knowledge-worker` stays on cloud until
  it passes. See CLAUDE.md § Context Profiles.
