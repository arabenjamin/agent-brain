# MCP Tools Reference

Complete documentation for all 81 MCP tools exposed by the Agent Brain server.

---

## ApiSkill (14 tools)

### `ingest_openapi`
Parses OpenAPI specs (URL or file path) and loads into Neo4j.
- Input: `{ "source": "https://example.com/openapi.json" }`
- Returns: count of resources, endpoints, schemas, parameters created

### `graph_query_endpoint`
Search endpoints by path pattern or keywords.
- Input: `{ "query": "users" }`
- Returns: matching endpoints with parameters and schemas

### `execute_http_request`
Execute HTTP requests with auto-credential injection.
- Input: `{ "method": "GET", "url": "...", "headers": {}, "body": {} }`
- Returns: status code, response body, duration, headers

### `get_api_context`
Retrieve API summaries from in-memory context.
- Input: `{ "api_name": "Petstore", "format": "summary" }` (both optional)
- Formats: `summary` (default), `detailed`, `compact`

### `list_loaded_apis`
List all APIs currently in the context store.
- Input: `{}` — returns names, versions, endpoint counts, load timestamps

### `clear_api_context`
Remove APIs from in-memory context (data remains in Neo4j).
- Input: `{ "api_name": "Petstore" }` (optional — clears all if omitted)

### `discover_openapi`
Auto-discover OpenAPI specifications for an API.
- Input: `{ "base_url": "https://api.example.com", "use_llm": true, "auto_ingest": false }`

### `build_openapi_from_docs`
Generate OpenAPI specs from documentation pages.
- Input: `{ "doc_urls": [...], "api_title": "...", "api_version": "...", "base_url": "...", "output_format": "json", "auto_ingest": false }`

### `build_openapi_from_repo`
Generate OpenAPI specs from repository source code.
- Input: `{ "repo_url": "...", "api_title": "...", "api_version": "...", "base_url": "...", "ref_name": "main", "subdirectory": "...", "merge_strategy": "enhance", "output_format": "json", "auto_ingest": false }`
- Merge strategies: `enhance`, `replace`, `ignore`

### `export_openapi`
Export healed knowledge graph back to OpenAPI 3.0 spec.
- Input: `{ "format": "yaml", "include_annotations": true, "include_broken": false }`

### `diff_api_spec`
Compare original spec vs current healed graph state.
- Input: `{ "api_name": "...", "format": "markdown", "breaking_only": false }`
- Formats: `markdown`, `changelog`, `json`

### `configure_api_credential`
Store API credentials for automatic injection.
- Input: `{ "api_name": "...", "credential_type": "api_key", "inject_location": "query", "inject_key": "appid", "secret_value": "..." }`
- Types: `api_key`, `bearer`, `basic`, `oauth2_client_credentials`

### `list_api_credentials`
List all configured API credentials (secrets masked).
- Input: `{}`

### `delete_api_credential`
Remove an API credential.
- Input: `{ "api_name": "..." }`

---

## KnowledgeSkill (16 tools)

### `store_note`
Persist a text note in the knowledge graph.
- Input: `{ "content": "...", "note_type": "semantic", "source_context": "...", "event_at": "..." }`
- Types: `semantic`, `episodic`, `reflection`, `consolidated`, `outcome`, `inference`
- Long notes (>1500 chars) auto-chunked into sub-notes with `PART_OF` edges
- Returns: `{ "note_id": "...", "links_created": N, "success": true }`

### `search_notes`
Hybrid BM25 + vector search with graph expansion.
- Input: `{ "query": "...", "limit": 5, "graph_hops": 2, "entity_expansion": false }`
- Merges results via Reciprocal Rank Fusion (RRF) with freshness boost
- Returns: `{ "count": N, "notes": [...] }`

### `find_related_notes`
Find notes linked via RELATES_TO graph edges.
- Input: `{ "note_id": "..." }`

### `prune_old_notes`
Delete stale notes using adaptive decay or time-based pruning.
- Input: `{ "score_threshold": 0.1, "lambda": 0.1, "dry_run": false, "days_stale": 30, "min_accesses": 2 }`
- Protected types: `consolidated`, `reflection` (never deleted)

### `consolidate_memories`
LLM-powered memory consolidation.
- Input: `{ "topic": "...", "limit": 10 }`
- Vector-searches top-N notes by topic, synthesizes via LLM
- Creates `SUMMARIZED_BY` edges from sources to consolidated note
- Auto-snapshots knowledge graph before LLM call (when configured)
- Returns: `{ "consolidated_note_id": "...", "source_count": N, "preview": "..." }`

### `review_due_notes`
Fetch notes whose spaced-repetition review interval has elapsed.
- Input: `{ "limit": 10 }`
- Returns notes where `next_review_at <= now()`

### `list_notes`
List recently created notes, optionally filtered by type.
- Input: `{ "limit": 20, "note_type": "episodic" }` (both optional)
- Returns notes in reverse-chronological order

### `search_by_entity`
Find notes that mention a named entity.
- Input: `{ "entity_name": "neo4j", "entity_type": "technology", "limit": 5 }`

### `reason`
Retrieve relevant notes and derive new inferences via LLM.
- Input: `{ "question": "...", "limit": 8, "store_inference": true }`
- Stores inference as a Note with `DERIVED_FROM` edges

### `audit_action`
Check a proposed action against stored values and principles.
- Input: `{ "action": "...", "context": "..." }`
- Returns: `{ "aligned": bool, "confidence", "concerns", "suggestions", "reasoning" }`

### `explain_reasoning`
Narrate why a decision was taken, citing knowledge sources.
- Input: `{ "decision": "...", "task_id": "...", "limit": 10 }`

### `ask_clarification`
Analyze a request for ambiguity before acting.
- Input: `{ "request": "...", "context": "...", "available_tools": [...] }`
- Returns: `{ "needs_clarification": bool, "ambiguities": [...], "clarifying_questions": [...] }`

### `get_note`
Fetch a single note by UUID (updates access stats).
- Input: `{ "id": "..." }`

### `delete_note`
Permanently delete a note and all its relationships (DETACH DELETE).
- Input: `{ "id": "..." }`

### `update_note`
Update note content in-place, preserving all graph edges and metadata.
- Input: `{ "id": "...", "content": "..." }`

### `export_graph_visualization`
Export the full knowledge graph as JSON for visualization.
- Input: `{ "max_nodes": 200 }`
- Returns Note, Entity, Task nodes and all 7 edge types

---

## TaskSkill (6 tools)

### `create_task`
Create and persist a high-level goal.
- Input: `{ "goal": "...", "context": "..." }` — returns `{ "task_id": "...", "status": "created" }`

### `reflect_on_work`
Critique current progress against a goal using LLM.
- Input: `{ "goal": "...", "current_state": "...", "plan": "...", "task_id": "..." }`
- Persists a reflection Note with `REFLECTS_ON` edge when `task_id` provided

### `decompose_goal`
Break a task into ordered sub-tasks using LLM.
- Input: `{ "goal_task_id": "...", "context": "...", "max_steps": 5 }`
- Creates subtask nodes with `SUBTASK_OF` edges; supports `depends_on_step` field

### `update_task`
Update a task's status and optionally attach a progress note.
- Input: `{ "task_id": "...", "status": "completed", "note": "..." }`
- Status values: `in_progress`, `completed`, `failed`, `blocked`

### `list_tasks`
List tasks with optional status filter.
- Input: `{ "status": "...", "limit": 20 }`
- Returns `parent_id` and `depends_on` for sub-tasks

### `record_outcome`
Store an episodic outcome note for a tool call or task attempt.
- Input: `{ "tool_name": "...", "summary": "...", "success": bool, "task_id": "..." }`
- On failure with `task_id`, auto-enqueues `reflect_on_work → store_note` chain

---

## AgentSkill (6 tools)

### `enqueue_jobs`
Submit one or more background jobs. Pass a single step to queue one job, or multiple
steps for a sequential chain. Priority: 0=low, 1=normal, 2=high, 3=critical.
- Input: `{ "steps": [{ "tool_name": "...", "arguments": {}, "priority": 1, "max_attempts": 3, "provider_hint": "ollama" }], "session_id": "..." }`
- Step 1 queued immediately; steps 2..N stored as `parked` until predecessor completes
- Returns: `{ "count": N, "ids": ["..."], "message": "..." }`

### `queue_status`
Get current queue statistics.
- Input: `{}` — returns `{ "in_memory_pending", "running_now", "max_concurrent", "enabled", "by_status" }`

### `cancel_job`
Cancel a queued or running job.
- Input: `{ "job_id": "..." }`

### `retry_job`
Requeue a failed, dead, or cancelled job (resets attempt_count to 0).
- Input: `{ "job_id": "..." }`

### `set_worker_config`
Update queue worker settings at runtime.
- Input: `{ "max_concurrent": N, "max_concurrent_ollama": N, "max_concurrent_anthropic": N, "max_concurrent_gemini": N, "enabled": bool, "poll_interval_secs": N }`
- Per-provider semaphores resized atomically; in-flight jobs keep their old permits

### `drain_queue`
Cancel all currently pending (queued) jobs.
- Input: `{}`

### `get_job_result` (dynamic tool)
Get the status and result of a specific job. Seeded at boot as a raw-Cypher dynamic
tool (not a native AgentSkill tool).
- Input: `{ "job_id": "..." }` — returns full AgentJob JSON

---

## AdminSkill (10 tools)

### `delete_api`
Cascade-delete all graph nodes for a specific ingested API.
- Input: `{ "api_name": "Petstore", "dry_run": false }`

### `purge_duplicate_endpoints`
Find and remove duplicate Endpoint nodes (same path + method).
- Input: `{ "dry_run": false }`

### `purge_orphaned_schemas`
Delete Schema nodes with no Endpoint relationships.
- Input: `{ "dry_run": false }`

### `reset_graph`
Wipe all API data from the graph (knowledge data preserved).
- Input: `{ "confirm": true, "dry_run": false }` — requires `confirm: true`

### `backfill_endpoint_embeddings`
Generate embeddings for Endpoint nodes missing them.
- Input: `{ "dry_run": false }`

### `snapshot_knowledge`
Take a compressed snapshot of the entire knowledge graph.
- Input: `{ "label": "optional-label" }`
- Returns: `{ "file", "exported_at", "notes", "tasks", "entities", "procedures", "size_bytes" }`

### `restore_knowledge`
Restore knowledge graph from a snapshot file.
- Input: `{ "file": "snapshots/snapshot_20260101_120000.json.gz", "dry_run": false }`
- MERGE-based restore: safe to run on non-empty graph
- Returns `RestoreStats` + reminder to run `backfill_endpoint_embeddings`

### `list_snapshots`
List all available knowledge snapshots.
- Input: `{}` — returns sorted newest-first with metadata

### `verify_knowledge_integrity`
Detect corrupted or suspicious notes in the knowledge graph.
- Input: `{ "content_min_length": 10 }`
- Checks: empty notes, orphaned chunks, hallucinated consolidated notes, duplicates (LIMIT 50)
- Returns: `{ "checks": {...}, "total_issues": N }`

### `analyze_own_structure`
Walk source files and live tool registry to produce a structural health report.
- Input: `{ "store_as_note": false }`
- Returns: Rust source file counts per module, registered tool counts, JSON structure

---

## ModelSkill (4 tools)

### `list_models`
List available LLM providers and all registered model specs.
- Input: `{}`

### `use_model`
Switch the active LLM provider and model at runtime.
- Input: `{ "provider": "Ollama"|"Anthropic"|"Gemini"|"VLlm", "model": "...", "api_key": "..." }`

### `select_model`
Auto-select the cheapest capable model for given requirements.
- Input: `{ "required_capabilities": [...], "max_cost_per_1k": N }`

### `reload_models`
Re-read `models.yaml` and sync into DuckDB without restarting the server.
- Input: `{}`

> **Note:** For model usage analytics (previously `get_model_stats` / `get_cloud_usage`),
> use the generic `duckdb_query` tool against the `model_usage` table. Example:
> `duckdb_query(sql="SELECT model_name, COUNT(*) AS total, AVG(duration_ms) AS avg_ms FROM model_usage GROUP BY model_name")`.

---

## SchedulerSkill (5 tools)

### `start_scheduler`
Enable the autonomous scheduler loop.
- Input: `{ "interval_secs": 300, "session_id": "..." }`

### `stop_scheduler`
Pause the scheduler loop (in-flight jobs continue).
- Input: `{}`

### `get_scheduler_status`
Return current scheduler config and runtime state.
- Input: `{}` — returns config + state with `tasks_dispatched`, `consecutive_errors`, `last_run_at`

### `configure_scheduler`
Update scheduler settings at runtime.
- Input: `{ "interval_secs": N, "enabled": bool, "max_tasks_per_run": N, "error_budget": N, "session_id": "..." }`

### `run_scheduler_tick`
Execute a scheduler tick immediately (bypasses timer).
- Input: `{}` — returns `{ "tasks_found": N, "tasks_dispatched": K, "skipped": M }`

---

## ContextSkill (4 tools)

### `list_context_profiles`
List all loaded context profiles.
- Input: `{}` — returns profile names, tool allowlists, and system prompt summaries

### `get_context_profile`
Fetch full details of a context profile.
- Input: `{ "name": "knowledge-worker" }`
- Returns: complete tools list, system prompt, token budget, model hints

### `auto_assign_context`
Auto-assign a context profile to a goal string using keyword matching.
- Input: `{ "goal": "..." }` — returns `{ "profile": "...", "method": "keyword|fallback" }`

### `build_agent_context`
Build a runtime context bundle for a profile (notes + tool list).
- Input: `{ "profile": "knowledge-worker" }` — returns pre-loaded notes + resolved tools list

---

## WorkingMemorySkill (4 tools)

### `push_context`
Append an entry to the session working-memory scratchpad.
- Input: `{ "session_id": "...", "content": "...", "role": "observation" }`
- Roles: `observation`, `plan`, `result`, `error`

### `get_context`
Retrieve working-memory entries for a session (in turn order).
- Input: `{ "session_id": "...", "limit": 20 }`

### `summarise_session`
LLM-summarise a session and persist to long-term memory.
- Input: `{ "session_id": "...", "delete_after_summarise": false }`

### `list_sessions`
List all active working memory session IDs.
- Input: `{}`

---

## ProcedureSkill (2 tools)

### `store_procedure`
Store a named multi-step workflow.
- Input: `{ "name": "...", "description": "...", "steps": [{ "tool", "args", "purpose" }] }`

### `search_procedures`
Search stored procedures by keyword.
- Input: `{ "query": "...", "limit": 5 }`

---

## DynamicSkill (4 static + N runtime tools)

### `define_tool`
Define a new MCP tool at runtime backed by a procedure pipeline.
- Input: `{ "name": "...", "description": "...", "input_schema": {...}, "steps": [...], "test_input": {...} }`
- Supports `{{input.field}}`, `{{context.var}}` template substitution; `output_var`, `condition`, `on_failure` fields

### `execute_procedure`
Execute a stored procedure by ID with optional input.
- Input: `{ "procedure_id": "...", "input": {}, "dry_run": false }`

### `list_dynamic_tools`
List all runtime-defined tools.
- Input: `{}`

### `remove_dynamic_tool`
Remove a runtime-defined tool by name.
- Input: `{ "name": "..." }`

---

## SearchSkill (1 tool)

### `search_web`
Search the web for information.
- Input: `{ "query": "rust async patterns", "engine": "serpapi", "count": 5 }`
- Engines: `serpapi` (default), `brave`, `google`
- Requires: `SERPAPI_KEY`, `BRAVE_API_KEY`, or `GOOGLE_API_KEY`+`GOOGLE_CX`

---

## SleepSkill (2 tools)

### `digest_experiences`
Export successful interactions to JSONL training datasets.
- Input: `{ "min_score": N }` — requires `TELEMETRY_DB_PATH`

### `analyze_gaps`
Identify knowledge gaps and missing capabilities from telemetry.
- Input: `{ "limit": 20 }` — reads from DuckDB `knowledge_gaps` table
