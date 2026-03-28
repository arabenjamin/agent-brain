# Agent Brain — Roadmap

Status as of 2026-02-28. Ordered by priority within each tier.
Pick up any section independently — each item is self-contained.

---

## Tier 1 — Brain Core (backend capabilities)

These are the gaps the brain identified in its own self-assessment
and the items needed to close the loop on human-like cognition.

---

### 1.1 Memory Consolidation ("Sleep Cycle")  ⭐ High priority

**What:** Transform episodic notes into distilled semantic knowledge on a schedule,
mimicking human slow-wave sleep consolidation.

**Why:** Without consolidation the Neo4j graph grows unbounded. The brain
already has `consolidate_memories` (LLM synthesis + `SUMMARIZED_BY` edges)
but it is only called on demand, never automatically.

**Plan:**
- Add a `consolidate` step to the scheduler's `perception_scan` / `goal_to_steps` logic:
  when the note count exceeds a threshold (e.g. 500 episodic notes), auto-create a
  `consolidate_memories` task for each topic cluster detected.
- Or: add a dedicated scheduled job that runs nightly (configurable via
  `CONSOLIDATION_INTERVAL_SECS` env), calls `consolidate_memories` per top-N entity clusters.
- Files: `src/services/scheduler.rs` (`perception_scan`), optionally a new
  `src/services/consolidation.rs`.

---

### 1.2 Semantic Chunking for Large Notes

**What:** Notes longer than ~1500 chars are currently split into raw character chunks
(already implemented). Upgrade to sentence/paragraph-aware chunking so each chunk
forms a coherent thought.

**Why:** Embedding a mid-sentence fragment produces a poor vector; chunking at
sentence boundaries makes every sub-note independently searchable.

**Plan:**
- In `knowledge.rs`  `store_note`: replace the fixed-length chunker with a
  sentence-boundary splitter (split on `.  ` / `\n\n`, min 200 chars, max 1500 chars).
- Each chunk still gets its own embedding and `PART_OF` edge.
- Files: `src/services/knowledge.rs` (`store_note`, around the chunking block).

---

### 1.3 Richer Entity Extraction

**What:** The brain extracts named entities when LLM is available, but uses a
simple one-shot prompt. Upgrade to structured extraction with entity types
(`person`, `tool`, `concept`, `url`, `date`) and co-reference resolution.

**Why:** A richer `Entity` graph enables precise `search_by_entity` queries and
powers visualisation of which concepts are central to the knowledge base.

**Plan:**
- Improve the extraction prompt in `knowledge.rs` `extract_entities()` to emit
  structured JSON: `[{"name":"...","type":"person|tool|concept|url|date"}]`.
- Add `entity_type` filter to `search_by_entity` (already supported in schema,
  just needs a better classifier feeding it).
- Consider a second pass: co-reference collapse — if "the brain" and "agent-brain"
  both appear, merge to the same `Entity` node.
- Files: `src/services/knowledge.rs` (`extract_entities`).

---

### 1.4 Multi-Hop Reasoning (Hierarchical Lexical Graph)

**What:** `search_notes` does up to `graph_hops` RELATES_TO traversals. Extend
this to also traverse `MENTIONS` → `Entity` → `MENTIONS` paths so a query about
"Neo4j" surfaces notes that mention a related entity even without direct similarity edges.

**Why:** Bridging through entity nodes unlocks true graph-RAG — notes become linked
through shared concepts, not just vector proximity.

**Plan:**
- Add an optional `entity_expansion: bool` parameter to `search_notes`.
- When enabled: after RRF merge, find all `Entity` nodes mentioned by result notes,
  then fetch other notes that mention those entities (up to depth 1), merge into results
  with a lower weight (e.g. 0.4 × RRF score).
- Add `export_graph_visualization` tool to KnowledgeSkill: returns a JSON graph
  `{nodes:[{id,label,type}], edges:[{source,target,type,weight}]}` for the HBI
  graph panel to render. Expose all Note + Entity + Task nodes and their edges.
- Files: `src/services/knowledge.rs`, `src/skills/knowledge.rs`.

---

### 1.5 `get_note` Tool (by ID)

**What:** A simple `get_note(id)` tool that fetches a single note by its UUID.

**Why:** Currently the only retrieval path is `search_notes`. The HBI graph panel
needs to fetch a note's full content when the user clicks a node. `search_notes`
with the node label as query is a fragile workaround.

**Plan:**
- Add `get_note_by_id(id: &str)` to `Neo4jClient` in `repository/` (or inline in
  `KnowledgeService`).
- Expose as a new KnowledgeSkill tool `get_note` with input `{ "id": "..." }`.
- Files: `src/services/knowledge.rs`, `src/skills/knowledge.rs`.
- Tool count becomes 67.

---

### 1.6 Procedural Memory — Control Flow

**What:** Dynamic Tools / Procedures support `{{input.field}}` substitution but
have no branching or looping. Add simple conditional steps (`if` / `unless`) and
output piping between steps (`{{steps.0.result}}`).

**Why:** Most real workflows need "if step 1 failed, skip step 2" — without this,
Dynamic Tools are brittle for anything non-linear.

**Plan:**
- Extend `ChainStep` (or the `ProcedureStep` struct) with optional `condition: Option<String>`
  evaluated against previous step outputs.
- Add `{{steps.N.result}}` substitution syntax in `procedure_executor.rs`.
- Add `on_failure: "skip" | "abort" | "continue"` field per step.
- Files: `src/services/procedure_executor.rs`, `src/models/`.

---

## Tier 2 — HBI Frontend Polish

Items from `NEXTSTEP.md`. Ordered by user-impact.

---

### 2.1 Graph Container Sizing

**File:** `hbi-frontend/src/components/graph/GraphPanel.tsx`
**Fix:** `ResizeObserver` + `useLayoutEffect` to pass measured `width`/`height` to `ForceGraph2D`.
See `NEXTSTEP.md §1` for the exact code snippet.

---

### 2.2 MCP Reconnect on Transport Error

**File:** `hbi-frontend/src/api/mcp.ts`
**Fix:** Wrap `callTool()` to catch transport errors, call `resetMcpClient()`, retry once.
See `NEXTSTEP.md §3` for the exact code snippet.

---

### 2.3 Knowledge Panel — Meaningful Initial Load

**File:** `hbi-frontend/src/components/knowledge/KnowledgePanel.tsx`
**Fix:** Replace the `query: " "` hack. On mount call `review_due_notes` (spaced-rep overdue
notes) for a meaningful default, or render an empty state and only query on user input.

---

### 2.4 Graph Node Click → Open Note

**File:** `hbi-frontend/src/components/graph/GraphPanel.tsx`
**Requires:** Brain item 1.5 (`get_note` tool) for a clean implementation.
**Fix:** `onNodeClick` → call `get_note({ id: node.id })` → slide-in content panel.
See `NEXTSTEP.md §2` for the workaround using `search_notes` if 1.5 is not yet done.

---

### 2.5 Task Panel — Subtask Tree View

**File:** `hbi-frontend/src/components/tasks/TaskPanel.tsx`
**Fix:** Group by `parent_id`, render subtasks indented under their parent.
See `NEXTSTEP.md §5` for the grouping snippet.

---

### 2.6 Graph Panel — Render from `export_graph_visualization`

**Requires:** Brain item 1.4 (`export_graph_visualization` tool).
**Fix:** Replace the current ad-hoc note → edge construction in `GraphPanel.tsx` with
a single call to `export_graph_visualization`. The brain returns the full graph JSON;
the frontend just renders it. This also surfaces `Entity` nodes and `Task` nodes,
not just `Note` nodes.

---

### 2.7 Auth UI — Settings Screen

**Fix:** Read API key from `localStorage`, add a gear-icon modal to edit Brain URL + API key.
See `NEXTSTEP.md §6` for the `config.ts` snippet.

---

### 2.8 Logs Panel

**What:** Expose the agent-brain's structured log output (or job history) in a panel.
**Options:**
- Stream `docker logs -f agent-brain` via a WebSocket proxy.
- Or: use `AgentSkill.get_job_result` / `queue_status` to build a job-history timeline.
- Simpler: just a rolling feed of recent `AgentJob` completions polled every 5s.

---

## Tier 3 — Infrastructure / CI

---

### 3.1 Branch Setup

Create the `dev`, `test`, and `prod` branches in GitHub to activate the full
branch-tier CI pipeline defined in `.github/workflows/ci.yml`:

```bash
git checkout -b dev && git push -u origin dev
git checkout -b test && git push -u origin test
git checkout -b prod && git push -u origin prod
git checkout master
```

After this, `master` is the working branch; merge to `dev` for a clean unit-test
run, `test` for integration tests, `prod` to publish the Docker image to GHCR.

---

### 3.2 Docker Compose — HBI Frontend Service

Add an `hbi-frontend` service to `docker-compose.yml` that builds and serves the
React app alongside the brain:

```yaml
hbi-frontend:
  build:
    context: ./hbi-frontend
    dockerfile: Dockerfile   # needs to be created — nginx serving dist/
  ports:
    - "5173:80"
  environment:
    - VITE_BRAIN_URL=http://agent-brain:3001
  depends_on:
    - agent-brain
  networks:
    - ai-network
```

Requires a new `hbi-frontend/Dockerfile` (multi-stage: `node:22` build → `nginx:alpine` serve).

---

### 3.3 GHCR Package Visibility

After the first `prod` push triggers the Docker workflow, the package will be
created as private. Set it to public in GitHub:
`github.com/arabenjamin → Packages → agent-brain → Package settings → Change visibility → Public`

---

## Quick-reference: Key Files

| Area | File |
|---|---|
| Scheduler / perception | `src/services/scheduler.rs` |
| Knowledge / search / memory | `src/services/knowledge.rs` |
| Knowledge skill (tools) | `src/skills/knowledge.rs` |
| Task skill | `src/skills/task.rs` |
| Procedure executor | `src/services/procedure_executor.rs` |
| Graph panel | `hbi-frontend/src/components/graph/GraphPanel.tsx` |
| MCP client | `hbi-frontend/src/api/mcp.ts` |
| Config (URL / key) | `hbi-frontend/src/api/config.ts` |
| Docker compose | `docker-compose.yml` |
| CI workflow | `.github/workflows/ci.yml` |
