# Generic Tools Refactor Plan

> **Goal:** Replace ~31 thin-wrapper tools and 7 HTTP-calling tools with a small set of
> generic primitives (`neo4j_query`, `http_request`) backed by a data-driven procedure +
> context layer. Make the scheduler's `goal_to_steps()` async so routing chains live in
> Neo4j rather than Rust code.
>
> Work through phases in order. Each phase is independently deployable.

---

## Motivation

The current 76-tool surface has two populations:

| Population | Count | Problem |
|---|---|---|
| Thin Neo4j wrappers | ~31 | Each is one Cypher query wrapped in Rust boilerplate |
| HTTP-calling tools | 7 | Hardcode specific APIs; can't reach new ones without a recompile |
| Algorithmic skills | ~30 | Real logic (RRF, chunking, embedding, decay scoring) — **keep as code** |
| Control/status tools | ~8 | In-process state management — **keep as code** |

The scheduler's `goal_to_steps()` is a `sync fn` with hardcoded keyword→chain mappings.
This means adding a new scheduler route requires a Rust recompile and redeploy.
Making it `async` allows routing rules to be stored as Neo4j nodes that the agent can
write and modify itself at runtime.

---

## New Node Types (Neo4j Schema)

```cypher
// API context — credentials + base URL for an HTTP API
(:ApiContext {
    id: String,           // UUID
    name: String,         // unique key, e.g. "github", "serpapi", "brave"
    base_url: String,     // https://api.github.com
    auth_scheme: String,  // "bearer" | "query_param" | "header" | "none"
    auth_param: String,   // header name ("Authorization") or query param name ("api_key")
    auth_env_var: String, // env var holding the secret, e.g. "GITHUB_TOKEN"
    default_headers: String,  // JSON object of headers always sent
    result_path: String,  // JSONPath to extract results from response body
    description: String,
    created_at: DateTime
})

// Scheduler routing chain — replaces a branch in goal_to_steps()
(:SchedulerChain {
    id: String,           // UUID
    pattern: String,      // substring to match in goal (lowercased)
    priority: Integer,    // checked in ascending order (lower = checked first)
    steps: String,        // JSON: Vec<ChainStep> serialized
    description: String,
    created_at: DateTime,
    updated_at: DateTime
})
```

---

## Phase Overview

```
Phase 0  — Data model decisions + doc updates (no code)         ✅ DONE
Phase 1  — goal_to_steps() → async                              ✅ DONE
Phase 2  — neo4j_query + duckdb_query primitives                ✅ DONE
Phase 3  — http_request primitive + ApiContext                  ✅ DONE
Phase 4  — GitHub tools → http_request + procedures             ✅ DONE
Phase 5  — search_web credentials → ApiContext                  ✅ DONE
Phase 6  — SchedulerChain CRUD tools in SchedulerSkill          ✅ DONE
Phase 7  — Thin wrapper procedures (gradual)    ✅ DONE
```

---

## Phase 0 — Documentation & Schema Decisions

**Status:** Complete (this document)

Decisions locked in:
- `ApiContext` schema above — name is the lookup key; credentials resolved from env at call time (not stored in Neo4j)
- `SchedulerChain` pattern matching is substring-based, same as current `goal.to_lowercase().contains()`
- Thin wrappers migrate to `Procedure` nodes seeded via `init-db` or boot protocol
- `search_web` stays as a convenience Rust wrapper that delegates to `http_request` internally (multi-engine fallback logic is non-trivial)
- No tool names change — procedure-backed tools keep the same names for backward compat

**Files to update when phases complete:**
- `CLAUDE.md` — add ApiContext and SchedulerChain to node list
- `project-docs/schema.md` — add new node types
- `project-docs/STATUS.md` — update tool counts
- `project-docs/tools.md` — add new generic tools

---

## Phase 1 — `goal_to_steps()` → Async

**Files:** `crates/app/src/services/scheduler.rs`

**What changes:**

1. Change signature:
   ```rust
   // Before
   fn goal_to_steps(goal: &str, task_id: &str) -> Vec<ChainStep>

   // After
   async fn goal_to_steps(&self, goal: &str, task_id: &str) -> Vec<ChainStep>
   ```

2. Update all call sites in `do_tick()`:
   ```rust
   // Before
   let steps = Self::goal_to_steps(&task_goal, &task_id);

   // After
   let steps = self.goal_to_steps(&task_goal, &task_id).await;
   ```

3. At the START of `goal_to_steps`, attempt Neo4j lookup for a matching `SchedulerChain`:
   ```rust
   // Try Neo4j first — agent-defined routing rules take priority
   if let Ok(chain_steps) = self.try_load_chain_from_neo4j(goal, task_id).await {
       return chain_steps;
   }
   // Fall back to hardcoded keyword heuristics (existing code unchanged below)
   ```

4. Add `try_load_chain_from_neo4j(&self, goal: &str, task_id: &str)`:
   ```rust
   async fn try_load_chain_from_neo4j(...) -> Result<Vec<ChainStep>, ()> {
       let cypher = "
         MATCH (c:SchedulerChain)
         WHERE toLower($goal) CONTAINS toLower(c.pattern)
         RETURN c.steps ORDER BY c.priority ASC LIMIT 1
       ";
       // deserialize steps JSON → Vec<ChainStep>
       // substitute {{task_id}} placeholder in args
   }
   ```

**Why this phase first:** All dynamic routing (Phase 6) depends on this. It's also a
small, self-contained change with no interface breaks.

**Test:** Existing scheduler integration tests must still pass. Add a test that stores a
`SchedulerChain` node and verifies `goal_to_steps` uses it instead of the hardcoded branch.

---

## Phase 2 — `neo4j_query` + `duckdb_query` Generic Primitives

**Files:**
- `crates/app/src/skills/query.rs` (new)
- `crates/app/src/mcp/server.rs` — register in `build_skills()`

**New Skill: `QuerySkill`** — 3 tools

### Tool: `neo4j_query`

```json
{
  "name": "neo4j_query",
  "description": "Execute a Cypher query against Neo4j. Use readonly=true for MATCH queries. Write queries (CREATE/MERGE/SET/DELETE) require readonly=false.",
  "input_schema": {
    "type": "object",
    "properties": {
      "cypher":   { "type": "string", "description": "Cypher query string" },
      "params":   { "type": "object", "description": "Query parameters as key-value pairs" },
      "readonly": { "type": "boolean", "description": "true for read-only queries (default: true)" },
      "limit":    { "type": "integer", "description": "Max rows to return (default: 100)" }
    },
    "required": ["cypher"]
  }
}
```

**Safety guard:** When `readonly=true` (default), validate the Cypher does not contain
write keywords (`CREATE`, `MERGE`, `SET`, `DELETE`, `REMOVE`, `DETACH`). Return an
error if the guard fires.

### Tool: `duckdb_query`

```json
{
  "name": "duckdb_query",
  "description": "Execute a read-only SQL query against the DuckDB analytics database (telemetry, model usage stats, interaction logs). Always read-only — no INSERT/UPDATE/DELETE.",
  "input_schema": {
    "type": "object",
    "properties": {
      "sql":    { "type": "string", "description": "SQL SELECT query" },
      "limit":  { "type": "integer", "description": "Max rows to return (default: 100)" }
    },
    "required": ["sql"]
  }
}
```

**Safety guard:** DuckDB is analytics-only. Always reject queries containing `INSERT`,
`UPDATE`, `DELETE`, `DROP`, `CREATE`, `ALTER`. Returns an error if the guard fires.
Only available when `TELEMETRY_DB_PATH` is set (same gate as `SleepSkill`).

**Why DuckDB alongside Neo4j:** `ModelSkill`'s `get_model_stats` and `select_model` are
thin DuckDB wrappers — identical problem to the Neo4j thin wrappers. `duckdb_query` lets
the agent ask arbitrary analytics questions (e.g. "which tool has the most failures this
week?") without a purpose-built tool per query.

### Tool: `explain_query` (optional, Phase 2b)

Returns the Neo4j query plan without executing — useful for the agent to verify a query
before running it.

**Why now:** Once `neo4j_query` and `duckdb_query` exist, the agent can read any stored
data without needing a purpose-built Rust tool. Phase 7 (thin wrapper procedures)
depends on these being available.

---

## Phase 3 — `http_request` Primitive + ApiContext

**Files:**
- `crates/app/src/skills/http.rs` (new)
- `crates/app/src/mcp/server.rs` — register in `build_skills()`

**New Skill: `HttpSkill`** — 4 tools

### Tool: `http_request`

```json
{
  "name": "http_request",
  "description": "Execute an HTTP request. Pass context_name to auto-inject auth headers from a stored ApiContext.",
  "input_schema": {
    "type": "object",
    "properties": {
      "method":       { "type": "string", "enum": ["GET","POST","PUT","PATCH","DELETE"] },
      "url":          { "type": "string" },
      "headers":      { "type": "object" },
      "body":         { "type": "object" },
      "context_name": { "type": "string", "description": "Name of a stored ApiContext for auto-auth" },
      "timeout_ms":   { "type": "integer", "default": 10000 }
    },
    "required": ["method", "url"]
  }
}
```

**Auth injection:** When `context_name` is set:
1. Query Neo4j for `(:ApiContext {name: $name})`
2. Read `auth_env_var` → resolve from env
3. Inject into header or query param per `auth_scheme`

### Tool: `define_api_context`

```json
{
  "name": "define_api_context",
  "description": "Store an API context (base URL, auth config) in Neo4j for reuse by http_request.",
  "input_schema": {
    "type": "object",
    "properties": {
      "name":         { "type": "string" },
      "base_url":     { "type": "string" },
      "auth_scheme":  { "type": "string", "enum": ["bearer","query_param","header","none"] },
      "auth_param":   { "type": "string" },
      "auth_env_var": { "type": "string" },
      "default_headers": { "type": "object" },
      "result_path":  { "type": "string" },
      "description":  { "type": "string" }
    },
    "required": ["name", "base_url"]
  }
}
```

### Tool: `list_api_contexts`

Returns all stored `ApiContext` nodes (name, base_url, auth_scheme, description).

### Tool: `load_api_context`

Fetches a single `ApiContext` by name (excludes the raw `auth_env_var` value for security).

**Seeded contexts at startup (in `build_skills`):**

```rust
// Upsert these on every boot — idempotent MERGE
seed_api_context("github",  "https://api.github.com", "bearer", "Authorization", "GITHUB_TOKEN", ...);
seed_api_context("serpapi", "https://serpapi.com",    "query_param", "api_key",  "SERPAPI_KEY",  ...);
seed_api_context("brave",   "https://api.search.brave.com", "header", "X-Subscription-Token", "BRAVE_API_KEY", ...);
seed_api_context("google_cse", "https://www.googleapis.com/customsearch/v1", "query_param", "key", "GOOGLE_API_KEY", ...);
```

---

## Phase 4 — GitHub Tools → `http_request` (DONE)

**Files:** `crates/app/src/skills/codebase.rs`, `crates/app/src/mcp/server.rs`

The three direct-HTTP GitHub tools were **removed entirely** (simpler than wrapping
them as procedure-backed dynamic tools):

- `github_read_file` — deleted
- `github_list_files` — deleted
- `github_get_commits` — deleted

**Replacement:** Agents call the generic `http_request` tool with `context_name="github"`.
The `github` `ApiContext` is already seeded at boot (see Phase 3) with `base_url`,
`auth_scheme="bearer"`, `auth_param="Authorization"`, and `auth_env_var="GITHUB_TOKEN"`.
This gives the same functionality with zero tool surface:

```json
http_request({
  "method":       "GET",
  "url":          "/repos/arabenjamin/agent-brain/contents/Cargo.toml",
  "context_name": "github"
})
```

**Also removed in this pass:**
- `CodebaseConfig.github_token / github_repo / github_default_branch` (fields, env-var reads)
- `CodebaseSkill.client / neo4j / github_repo / github_default_branch` (fields)
- `CodebaseSkill::build_github_headers()` (helper)
- `base64` and `urlencoding` crate dependencies (no longer referenced anywhere)
- Stale env vars: `GITHUB_REPO`, `GITHUB_DEFAULT_BRANCH` (GitHub tooling is now
  driven by the `github` `ApiContext` node and `GITHUB_TOKEN` alone)

**Net impact:** `CodebaseSkill` tool count 10 → 7. Total static tools 76 → 73.

---

## Phase 5 — `search_web` → `http_request` + Procedure

**Files:** `crates/app/src/skills/search.rs`, `crates/app/src/services/` (search service)

`search_web` currently does:
1. Try SerpApi
2. On failure: try Brave
3. On failure: try Google CSE
4. Normalize results into `[{title, url, snippet}]`

Options:
- **Option A (preferred):** Keep `search_web` as a Rust skill but have it call `http_request` internally via the `HttpSkill` service, resolving engine contexts from Neo4j. The multi-engine fallback logic stays in Rust; credentials move to `ApiContext`.
- **Option B:** Convert to a multi-step Procedure with conditional `on_failure: continue` steps. Requires `procedure_executor` to support conditional fallback. More elegant but higher complexity.

**Decision:** Option A for now. Removes hardcoded credential handling; routing/fallback stays in Rust where it's reliable.

---

## Phase 6 — SchedulerChain Nodes (Enabled by Phase 1)

**Files:** `crates/app/src/services/scheduler.rs`, `crates/app/src/skills/scheduler.rs`

With Phase 1 done, `goal_to_steps` already queries `SchedulerChain` nodes first.
Phase 6 adds the tooling so the agent can manage its own routing rules.

**New tools added to `SchedulerSkill`:**

### `define_scheduler_chain`
```json
{
  "name": "define_scheduler_chain",
  "description": "Define a new routing chain for the scheduler. When a task goal matches 'pattern', the scheduler will dispatch the given steps instead of using the built-in heuristic.",
  "input_schema": {
    "type": "object",
    "properties": {
      "pattern":     { "type": "string", "description": "Substring to match in the task goal (case-insensitive)" },
      "steps":       { "type": "array",  "description": "ChainStep array (same format as job chains)" },
      "priority":    { "type": "integer","description": "Lower = checked first (default: 100)" },
      "description": { "type": "string" }
    },
    "required": ["pattern", "steps"]
  }
}
```

### `list_scheduler_chains`

Returns all `SchedulerChain` nodes (id, pattern, priority, description, step_count).

### `remove_scheduler_chain`

Deletes a `SchedulerChain` by id.

**Seeding:** On first boot (empty graph), seed all current hardcoded heuristic branches
as `SchedulerChain` nodes so they are visible and editable without code access.

---

## Phase 7 — Thin Wrapper Procedures (Gradual Migration)

**Status:** ✅ DONE — Batch 1 seeded at boot (2026-03-29).

Six read-only `Procedure` nodes are now seeded idempotently at every `build_skills()` call
via `seed_standard_procedures()` in `crates/app/src/mcp/server.rs`.  Uses `MERGE … ON CREATE
SET` so user-edited procedures survive restarts.  No `DynamicTool` nodes created (avoids
shadowing existing Rust tools); procedures are discoverable via `search_procedures` and
runnable via `execute_procedure`.

**Seeded procedures (Batch 1 — read-only thin wrappers):**
- `list_tasks` — `MATCH (t:Task) RETURN id, goal, status, created_at ORDER BY created_at DESC`
- `list_notes` — `MATCH (n:Note) RETURN id, note_type, content, created_at ORDER BY created_at DESC`
- `list_sessions` — `MATCH (w:WorkingMemory) RETURN DISTINCT session_id ORDER BY session_id`
- `get_job_result` — `MATCH (j:AgentJob {id: '{{input.job_id}}'}) RETURN id, tool_name, status, result, error`
- `search_procedures_by_name` — keyword search on name/description using `{{input.query}}`
- `list_dynamic_tools` — `MATCH (d:DynamicTool) RETURN id, name, description ORDER BY name`

**Remaining (future batches):**

Batch 2 — simple mutations: `cancel_job`, `retry_job`, `drain_queue`, `remove_dynamic_tool`

Batch 3 — multi-step / conditional: `record_outcome`, `update_task`, `find_related_notes`

**Not migrated (complex logic stays in Rust forever):**
- `store_note` — chunking + embedding + entity extraction
- `search_notes` — BM25 + vector RRF + freshness boost
- `consolidate_memories`, `prune_old_notes`, `reason`
- `decompose_goal`, `reflect_on_work`, `summarise_session`
- `enqueue_jobs` — graph dependency resolution (unified single + chain tool, replaces `enqueue_agent` + `enqueue_chain`)
- `run_scheduler_tick` — full tick orchestration
- All WebSocket tools (`ws_*`)

---

## Tool Count After Full Migration

| Category | Before | After |
|---|---|---|
| Generic primitives (new) | 0 | 7 (`neo4j_query`, `duckdb_query`, `http_request`, `define_api_context`, `list_api_contexts`, `load_api_context`, `explain_query`) |
| Scheduler chain tools (new) | 0 | 3 |
| Thin wrappers (removed) | ~31 | ~8 (remaining complex ones) |
| HTTP-specific skills (removed) | 7 | 3 (ws_* stays) |
| Algorithmic skills (unchanged) | ~30 | ~30 |
| Control/status (unchanged) | ~8 | ~8 |
| **Total** | **~76** | **~58** |

Plus N dynamic/procedure-backed tools registered at runtime (was already true).

---

## Implementation Notes

### Cargo dependencies

Phase 3 (`HttpSkill`) needs `reqwest` with the `json` feature — already present.
No new dependencies required for any phase.

### `goal_to_steps` ChainStep substitution

`SchedulerChain.steps` is stored as JSON. When loaded, substitute:
- `{{task_id}}` → the actual task UUID
- `{{goal}}` → the task goal string
- `{{date}}` → `chrono::Utc::now().format("%Y-%m-%d")`

This mirrors what the hardcoded branches do today.

### Security considerations

- `neo4j_query` with `readonly=false` is a powerful tool. Consider requiring an explicit
  confirmation parameter or restricting to specific node types.
- `http_request` with no `context_name` makes arbitrary outbound calls. Consider an
  allowlist of domains or require `context_name` for production deployments.
- `ApiContext.auth_env_var` stores the env var NAME, not the value — credentials never
  touch Neo4j.

### Branch strategy

- Each phase on its own `feature/generic-tools-phaseN` branch
- Merge to `dev` after tests pass
- Phase 1 is the only one touching the scheduler — keep it narrow

---

## Open Questions

1. **`procedure_executor` conditional fallback** — **Resolved: Option A for `search_web`.** `on_failure: "continue"` already exists in the executor (it is the default). The real obstacle is result normalization: SerpApi, Brave, and Google CSE return completely different JSON shapes (`organic_results[].link` vs `web.results[].url` vs `items[].link`). A procedure cannot remap these into a unified `[{title, url, snippet}]` array — that is logic, not data. `search_web` therefore stays as Rust code (Option A) and simply delegates its outbound HTTP calls to `HttpSkill` instead of owning a `reqwest::Client` directly.
2. **`init-db` vs boot protocol for seeding** — **Decided: `boot.yaml`.** `SchedulerChain` and `Procedure` seeds run via boot protocol on every startup using idempotent `MERGE` so upgrades automatically apply new chains without manual `init-db` reruns.
3. **DuckDB / model contexts** — **Decided: yes, add `duckdb_query`.** See Phase 2 — `duckdb_query` is added to `QuerySkill` alongside `neo4j_query`. DuckDB is analytics-only (telemetry, model usage stats), so it is naturally read-only and lower risk than Neo4j writes.
