# Agent Brain — Deployment and Usage Guide

An autonomous AI agent brain exposed via the Model Context Protocol (MCP). Backed by Neo4j for persistent memory, Tokio for async job execution, and any Ollama/Anthropic/Gemini LLM for reasoning.

## Quick Deployment (Docker)

```bash
# Start Neo4j + MCP server (HTTP transport on :3000)
docker compose up -d --build

# With API key authentication
MCP_API_KEY=your-secret docker compose up -d --build

# View logs
docker compose logs -f agent-brain

# Health check
curl http://localhost:3000/health
```

## Environment Variables

Copy `.env.example` to `.env` and configure:

```bash
# Neo4j (required)
NEO4J_URI=bolt://localhost:7688
NEO4J_USER=neo4j
NEO4J_PASSWORD=your-password

# LLM (choose one provider)
LLM_PROVIDER=ollama            # or: anthropic, gemini
OLLAMA_URL=http://localhost:11434
OLLAMA_MODEL=granite4:latest
OLLAMA_EMBED_MODEL=bge-m3:latest
# ANTHROPIC_API_KEY=sk-ant-...
# GEMINI_API_KEY=...

# MCP Transport
MCP_TRANSPORT=http             # or: stdio
MCP_HTTP_BIND=0.0.0.0:3000
MCP_API_KEY=your-secret-key    # optional

# Autonomous Scheduler
SCHEDULER_ENABLED=true
SCHEDULER_INTERVAL_SECS=300    # poll every 5 minutes

# Web Search (configure at least one)
SERPAPI_KEY=...
# BRAVE_API_KEY=...
# GOOGLE_API_KEY=... + GOOGLE_CX=...
```

## Session Lifecycle (HTTP Transport)

Every HTTP client must complete a handshake before calling tools:

```bash
# Step 1: Initialize — capture the session ID from the response header
SESSION_ID=$(curl -si -X POST http://localhost:3000/mcp \
  -H "Content-Type: application/json" \
  -H "mcp-protocol-version: 2024-11-05" \
  -d '{
    "jsonrpc": "2.0", "id": 1, "method": "initialize",
    "params": {
      "protocolVersion": "2024-11-05",
      "capabilities": {},
      "clientInfo": {"name": "curl", "version": "1.0"}
    }
  }' | grep -i mcp-session-id | awk '{print $2}' | tr -d '\r')

# Step 2: Confirm initialization (required — server stays in Initializing state without this)
curl -s -X POST http://localhost:3000/mcp \
  -H "Content-Type: application/json" \
  -H "mcp-protocol-version: 2024-11-05" \
  -H "mcp-session-id: $SESSION_ID" \
  -d '{"jsonrpc": "2.0", "method": "notifications/initialized"}'

# Step 3: Call any tool
curl -s -X POST http://localhost:3000/mcp \
  -H "Content-Type: application/json" \
  -H "mcp-protocol-version: 2024-11-05" \
  -H "mcp-session-id: $SESSION_ID" \
  -d '{
    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
    "params": {"name": "get_scheduler_status", "arguments": {}}
  }'
```

## Skill Capabilities (64 tools across 12 skills)

### 1. API Knowledge (`ApiSkill` — 14 tools)

Ingest, query, and self-heal REST API documentation.

```
ingest_openapi          — Load an OpenAPI spec from URL or file path
graph_query_endpoint    — Search endpoints by path pattern or keyword
execute_http_request    — Call an API with automatic credential injection + self-healing
get_api_context         — Retrieve loaded API summaries
list_loaded_apis        — List all APIs in the context store
clear_api_context       — Evict APIs from in-memory cache
discover_openapi        — Auto-probe a base URL for OpenAPI specs
build_openapi_from_docs — Generate a spec from documentation pages via LLM
build_openapi_from_repo — Generate a spec from GitHub/GitLab source code via LLM
export_openapi          — Export the healed graph back to OpenAPI 3.0 YAML/JSON
diff_api_spec           — Compare original spec vs healed state
configure_api_credential — Store credentials for automatic injection
list_api_credentials    — List stored credentials (secrets masked)
delete_api_credential   — Remove a credential
```

### 2. Knowledge & Memory (`KnowledgeSkill` — 10 tools)

Persistent long-term memory with hybrid BM25 + vector RAG.

```
store_note          — Persist a note with embeddings, entity extraction, and auto-linking
search_notes        — Hybrid BM25+vector RRF search with multi-hop graph expansion
find_related_notes  — Find notes linked by similarity edges
prune_old_notes     — Delete stale notes via adaptive decay scoring
consolidate_memories — LLM-synthesise multiple notes into a summary
review_due_notes    — Spaced-repetition: return notes due for review
search_by_entity    — Find notes mentioning a named entity
reason              — RAG + LLM inference; stores DERIVED_FROM edges
audit_action        — Check a proposed action against stored principles
explain_reasoning   — Narrate why a decision was made, citing sources
```

### 3. Task Management (`TaskSkill` — 6 tools)

Goal tracking, decomposition, and reflection.

```
create_task      — Create a high-level goal node in Neo4j
reflect_on_work  — LLM critique of progress; persists reflection Note with REFLECTS_ON edge
decompose_goal   — LLM-breaks a task into sub-tasks with SUBTASK_OF edges
update_task      — Set status (in_progress/completed/failed/blocked)
list_tasks       — List tasks filtered by status; shows parent_id for sub-tasks
record_outcome   — Store an episodic outcome note for a task or tool call
```

### 4. Background Jobs (`AgentSkill` — 8 tools)

Durable priority job queue — submit any MCP tool as an async background job.

```
enqueue_agent   — Submit a tool call as a background job (priority 0-3, persistent)
enqueue_chain   — Submit an ordered chain; each step auto-promotes when predecessor completes
queue_status    — Stats: pending, running, per-status counts, per-provider breakdown
get_job_result  — Poll a job for its status and result
cancel_job      — Cancel a queued or running job
retry_job       — Requeue a failed/dead/cancelled job
set_worker_config — Update concurrency, enable/pause, poll interval
drain_queue     — Cancel all currently pending jobs
```

### 5. Autonomous Scheduler (`SchedulerSkill` — 5 tools)

Background Tokio task that wakes periodically, queries Neo4j for `created` tasks, maps each task's goal to a job chain, and enqueues the chain automatically.

```
start_scheduler      — Enable scheduling; optionally set interval and session_id
stop_scheduler       — Pause the scheduler (jobs already queued are unaffected)
get_scheduler_status — Config snapshot + stats (tasks_dispatched, last_run_at, errors)
configure_scheduler  — Update interval, max_tasks_per_run, error_budget, session_id
run_scheduler_tick   — Trigger an immediate tick and return results
```

### 6. Procedural Memory (`ProcedureSkill` — 2 tools)

Store and search named multi-step workflows.

```
store_procedure    — Persist a named workflow with ordered steps
search_procedures  — Search procedures by name or description
```

### 7. Working Memory (`WorkingMemorySkill` — 3 tools)

Session-scoped scratchpad with LLM summarisation into long-term memory.

```
push_context       — Append an entry to the session scratchpad
get_context        — Retrieve entries in turn order
summarise_session  — LLM-summarise the session into a long-term Note
```

### 8. Dynamic Tool Builder (`DynamicSkill` — 4 + runtime tools)

Define new MCP tools at runtime backed by stored procedure pipelines.

```
define_tool       — Create a new tool with JSON schema + procedure steps; hot-registered immediately
execute_procedure — Run a stored procedure with {{input.field}} template substitution
list_dynamic_tools — List all runtime-defined tools
remove_dynamic_tool — Delete and unregister a dynamic tool live
```

### 9. Graph Admin (`AdminSkill` — 4 tools)

Maintenance tools for the Neo4j knowledge graph.

```
delete_api                — Cascade-delete all nodes for one ingested API
purge_duplicate_endpoints — Remove duplicate Endpoint nodes
purge_orphaned_schemas    — Delete Schema nodes with no Endpoint relationships
reset_graph               — Wipe all API data (knowledge data preserved; requires confirm: true)
```

### 10. Model Registry (`ModelSkill` — 5 tools)

LLM provider management and intelligent model selection.

```
list_models    — List all providers and registered model specs
use_model      — Switch the active LLM provider and model at runtime
register_model — Register a model spec (capabilities, cost, context window)
select_model   — Auto-select the cheapest capable model for given requirements
get_model_stats — Usage statistics for a model from AgentJob history
```

### 11. Web Search (`SearchSkill` — 1 tool)

```
search_web — Search the web (SerpApi / Brave / Google Custom Search)
             Input: { "query": "...", "engine": "serpapi", "count": 5 }
```

### 12. Sleep / Telemetry (`SleepSkill` — 2 tools)

Export experience data and identify knowledge gaps from DuckDB telemetry.

```
digest_experiences — Export successful interactions to JSONL for fine-tuning
analyze_gaps       — Identify underexplored topics from interaction telemetry
```

## Common Usage Patterns

### Ingest and query an API

```json
{"name": "ingest_openapi", "arguments": {"source": "https://petstore3.swagger.io/api/v3/openapi.json"}}
{"name": "graph_query_endpoint", "arguments": {"query": "pets"}}
{"name": "execute_http_request", "arguments": {"method": "GET", "url": "https://petstore3.swagger.io/api/v3/pet/1"}}
```

### Store knowledge and reason over it

```json
{"name": "store_note", "arguments": {"content": "The Stripe API uses idempotency keys via the Idempotency-Key header."}}
{"name": "reason", "arguments": {"question": "How should I handle Stripe duplicate requests?"}}
```

### Decompose a goal and let the scheduler execute it

```json
{"name": "create_task", "arguments": {"goal": "Audit all loaded APIs for broken endpoints"}}
{"name": "decompose_goal", "arguments": {"goal_task_id": "<task-id>", "max_steps": 5}}
{"name": "run_scheduler_tick", "arguments": {}}
{"name": "queue_status", "arguments": {}}
```

### Submit a background job chain

```json
{
  "name": "enqueue_chain",
  "arguments": {
    "steps": [
      {"tool_name": "search_notes", "arguments": {"query": "API authentication patterns"}},
      {"tool_name": "reason", "arguments": {"question": "Which auth pattern is best for public APIs?"}}
    ]
  }
}
```

### Switch LLM provider at runtime

```json
{"name": "use_model", "arguments": {"provider": "Anthropic", "model": "claude-haiku-4-5-20251001"}}
{"name": "list_models", "arguments": {}}
```
