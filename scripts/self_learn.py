#!/usr/bin/env python3
"""
self_learn.py — Bootstrap or refresh agent-brain's self-knowledge.

Seeds the Neo4j knowledge graph with curated architectural notes, creates a
self-understanding task, decomposes it, and enqueues reasoning + reflection
chains so the agent can derive its own improvement insights.

Usage:
    python3 scripts/self_learn.py [--base-url http://localhost:3001]
    python3 scripts/self_learn.py --force   # re-seed even if notes exist
"""

import json
import sys
import time
import argparse
import urllib.request
import urllib.error

# ---------------------------------------------------------------------------
# Knowledge corpus — curated notes about agent-brain (current as of 2026-03-03)
# ---------------------------------------------------------------------------

NOTES = [
    # ── Note 1: Architecture Overview ──────────────────────────────────────
    {
        "note_type": "semantic",
        "source_context": "agent-brain architecture 2026-03-03",
        "content": """\
# Agent Brain — Architecture Overview

Agent Brain is an **MCP (Model Context Protocol) server** written in Rust
(Tokio async runtime, Edition 2024). It is a persistent, self-improving
intelligence core backed by a Neo4j knowledge graph.

## Tech Stack
- Language: Rust 2024 edition, async via Tokio
- Protocol: MCP via stdio (local) and HTTP+SSE (remote/Docker)
- Web framework: Axum (HTTP/SSE transport)
- Database: Neo4j via neo4rs — ALL persistent state lives here
- LLM providers: Ollama (local, default granite4), Anthropic, Gemini
- Secret storage: Local AES-256-GCM / HashiCorp Vault / AWS Secrets Manager
- Telemetry: DuckDB (brain_logs.db)

## Transport Architecture
Two transports share the same McpServerCore:
- StdioTransport: stdin/stdout for local CLI / Claude Desktop
- HttpTransport: Axum + SSE on port 3000/3001
  - POST /mcp (tool calls), GET /mcp (SSE stream), DELETE /mcp (session close)
  - POST /chat (ChatService: multi-turn LLM with native tool-use loop)
  - GET /health (liveness check)
  - Sessions tracked by Mcp-Session-Id header
  - HTTP session MUST send notifications/initialized after initialize

## Skills Pattern
Each Skill implements: name(), tools(), execute().
Skills register to TWO places:
1. ToolRegistry — answers tools/list
2. ToolHandler Vec — dispatches tools/call
McpServerCore: Arc<RwLock<ToolRegistry>> + Arc<RwLock<Option<ToolHandler>>>
build_skills() creates all skills and populates both.

## Key Design Patterns
- dry_run: all destructive admin operations accept dry_run=true
- builder pattern: McpServerCore uses with_*() methods for config
- Parked jobs: enqueue_jobs creates sequential pipelines; steps 2..N wait parked
- Per-provider semaphores: ollama(3), anthropic(2), gemini(5) concurrency
- ContextStore: in-memory API cache with Neo4j fallback on miss
- Context Profiles: YAML profiles in contexts/ that restrict tools + inject
  system prompts; boot.yaml runs on startup, init.yaml on empty graph

## Context Profile System
YAML files in contexts/ (CONTEXTS_DIR env var). Profiles have:
- name, description, tool_allowlist, system_prompt, pre_load_query
- auto_assign(goal) matches keywords to profile names; fallback "general"
- build_bundle(name) fetches pre_load_query notes from Neo4j for context
Profiles: general, knowledge-worker, task-manager, code-analyst, api-builder, scheduler

## SnapshotService
Gzip JSON snapshots (.json.gz) via flate2. Location: KNOWLEDGE_SNAPSHOT_DIR (./snapshots).
Embeddings excluded; use backfill_endpoint_embeddings after restore.
MERGE-based restore — idempotent, safe on non-empty graph.
Auto-snapshot before consolidate_memories (guarded by AUTO_SNAPSHOT_BEFORE_CONSOLIDATION).

## Chat Endpoint
POST /chat → SSE stream of thinking/tool_call/tool_result/message/error/done events.
Anthropic: native tool_use loop (POST /v1/messages + tools array), MAX_TOOL_ITERATIONS=10.
Ollama/Gemini: text loop with <tool_call>{"tool":"...","args":{...}}</tool_call> parsing.
ChatService sees live tool registry and LLM config — supports use_model switching.

## Initialization Order (build_skills)
1. DynamicSkill::new() + load_from_neo4j()
2. QueueService::new() + recover()
3. ContextBuilderService::new() + load_profiles()
4. SchedulerService::new_with_context()
5. ContextStore::with_neo4j() + load_all()
6. Register all skills → registry + skills vec
7. QueueService::spawn_coordinator() (MUST be after handler is set)
8. Run boot protocol (contexts/boot.yaml) → optionally run init protocol
""",
    },

    # ── Note 2: Complete Tool Catalog ───────────────────────────────────────
    {
        "note_type": "semantic",
        "source_context": "agent-brain tool catalog 2026-03-03",
        "content": """\
# Agent Brain — Complete Tool Catalog (78 static + N runtime)

## ApiSkill (14 tools) — OpenAPI knowledge and HTTP execution
- ingest_openapi: parse and load OpenAPI spec (URL or file) into Neo4j
- graph_query_endpoint: search endpoints by path/keyword (CONTAINS; vector TODO)
- execute_http_request: run HTTP requests with auto-credential injection + self-healing
- get_api_context: retrieve API summary from in-memory ContextStore
- list_loaded_apis: list all APIs in context store
- clear_api_context: remove APIs from context (data stays in Neo4j)
- discover_openapi: auto-discover specs (probes paths, parses HTML, uses LLM)
- build_openapi_from_docs: generate spec from documentation pages via LLM
- build_openapi_from_repo: generate spec from repository source code via LLM
- export_openapi: reconstruct OpenAPI 3.0 spec from healed graph
- diff_api_spec: compare original vs healed graph (markdown/changelog/json)
- configure_api_credential: store API auth for auto-injection
- list_api_credentials: list credentials (secrets masked)
- delete_api_credential: remove a credential

## KnowledgeSkill (15 tools) — Notes, RAG, reasoning, memory
- store_note: persist text note (types: semantic/episodic/reflection/consolidated/outcome/inference)
  Long notes (>1500 chars) auto-chunked via semantic boundaries (PART_OF edges)
  Entity extraction: 7 types (person/tool/technology/concept/organisation/url/date)
- search_notes: hybrid BM25+vector search, RRF merge, freshness boost (0.7*rrf + 0.3*freshness)
  entity_expansion=true bridges MENTIONS→Entity←MENTIONS for multi-hop discovery
- find_related_notes: follow RELATES_TO graph edges
- prune_old_notes: adaptive decay scoring or time-based pruning
  Protected types: consolidated, reflection (never deleted)
- consolidate_memories: LLM synthesis; auto-snapshot before LLM call
  Source notes get next_review_at = now + 30 days after consolidation
- review_due_notes: spaced-repetition notes whose review interval has elapsed
- search_by_entity: find notes mentioning a named entity
- reason: vector+BM25 search + LLM inference, stores DERIVED_FROM notes
- audit_action: check proposed action against stored values/principles
- explain_reasoning: narrate why a decision was made, citing source notes
- ask_clarification: analyze a request for ambiguity before acting
- get_note: fetch single note by UUID (updates access stats)
- delete_note: permanently delete note and all relationships (DETACH DELETE)
- update_note: update note content in-place (preserves all edges and metadata)
- export_graph_visualization: full knowledge graph JSON (max_nodes param)
  Returns Note+Entity+Task nodes with all 7 edge types

## TaskSkill (6 tools) — Goal tracking and decomposition
- create_task: create persisted goal with UUID (status: created)
- reflect_on_work: LLM critique of progress, stores reflection note with REFLECTS_ON edge
- decompose_goal: LLM breaks task into ordered sub-tasks (SUBTASK_OF edges)
  Supports depends_on_step for dependency tracking (DEPENDS_ON edges)
- update_task: set status (in_progress/completed/failed/blocked) + note
- list_tasks: filtered list with parent_id and depends_on for sub-tasks
- record_outcome: episodic outcome note; on failure+task_id auto-enqueues reflect→store chain

## AgentSkill (6 tools) — Background job queue
- enqueue_jobs: submit one tool call or a sequential pipeline of tool calls (steps 2..N parked, unpark on parent success)
- queue_status: stats (pending, running, per-provider breakdown)
- cancel_job / retry_job: lifecycle management
- set_worker_config: change concurrency, enable/pause, poll interval
- drain_queue: cancel all pending jobs
- get_job_result: dynamic tool (not in AgentSkill); poll a job for status/result

## AdminSkill (10 tools) — Graph maintenance, snapshots, integrity
- delete_api: cascade-delete all nodes for one ingested API (dry_run)
- purge_duplicate_endpoints: remove duplicate Endpoint nodes (dry_run)
- purge_orphaned_schemas: delete unreferenced Schema nodes (dry_run)
- reset_graph: wipe all API data, preserve knowledge (requires confirm:true)
- backfill_endpoint_embeddings: generate embeddings for Endpoints missing them
- snapshot_knowledge: take gzip JSON snapshot of entire knowledge graph
- restore_knowledge: restore from snapshot file (MERGE-based, idempotent)
- list_snapshots: list available snapshots (newest-first)
- verify_knowledge_integrity: detect corrupted notes (empty, orphaned chunks, duplicates)
- analyze_own_structure: LLM analysis of all registered tools and architecture

## ModelSkill (5 tools) — LLM registry and selection
- list_models: list providers and all registered ModelSpecs
- use_model: switch active LLM provider and model at runtime
- register_model: store ModelSpec (cost, capabilities, context window)
- select_model: cheapest-first capability-based model selection
- get_model_stats: usage stats from AgentJob history

## SchedulerSkill (5 tools) — Autonomous self-improvement loop
- start_scheduler: enable background loop
- stop_scheduler: pause (in-flight jobs continue)
- get_scheduler_status: config + runtime state (tasks_dispatched, consecutive_errors)
- configure_scheduler: update settings at runtime
- run_scheduler_tick: execute tick immediately (bypasses timer)

## WorkingMemorySkill (4 tools) — Session scratchpad
- push_context: append to session working memory (roles: observation/plan/result/error)
- get_context: retrieve session entries in turn order
- summarise_session: LLM-summarise and persist to long-term memory
- list_sessions: list active working memory session IDs

## ProcedureSkill (2 tools) — Stored workflows
- store_procedure: persist named multi-step workflow
- search_procedures: keyword search by name/description

## DynamicSkill (4 static + N runtime) — Runtime tool definition
- define_tool: create new MCP tool backed by a procedure pipeline (hot-registered)
  Template substitution: {{input.field}}, {{context.var}}, {{context.steps.N}}
  Per-step: output_var, condition, on_failure (abort|skip|continue)
- execute_procedure: run stored procedure by ID
- list_dynamic_tools: list all runtime-defined tools
- remove_dynamic_tool: remove by name

## SearchSkill (1 tool)
- search_web: web search via SerpApi / Brave / Google Custom Search

## SleepSkill (2 tools) — Offline learning
- digest_experiences: export training data from DuckDB interactions table
- analyze_gaps: identify knowledge gaps from DuckDB knowledge_gaps table

## ContextSkill (4 tools) — Context profile management
- list_context_profiles: list available YAML context profiles
- get_context_profile: get full profile details (tool_allowlist, system_prompt)
- auto_assign_context: keyword-match a goal to best profile
- build_agent_context: fetch notes and tool list for a named profile
""",
    },

    # ── Note 3: Neo4j Graph Schema ──────────────────────────────────────────
    {
        "note_type": "semantic",
        "source_context": "agent-brain schema 2026-03-03",
        "content": """\
# Agent Brain — Neo4j Graph Schema

## Node Types

### API Knowledge Graph
- Resource: High-level API grouping (name, base_url, version)
- Endpoint: API path + method (path, method, summary, operationId, status)
- Schema: Data object definition (name, json_structure)
- Parameter: Endpoint input (name, in, required, schema)
- HealingEvent: Immutable AI-driven fix record (request, response, correction)

### Long-Term Memory
- Note: Stored text memory (id, content, note_type, source_context, event_at)
  access_count, last_accessed_at, next_review_at, review_interval_days
  Optional: embedding (1024-dim bge-m3 vector)
  Types: semantic, episodic, reflection, consolidated, outcome, inference
- Entity: Named entity extracted from notes
  Fields: name (unique lowercased), entity_type
  Types: person, tool, technology, concept, organisation, url, date

### Goals and Tasks
- Task: High-level goal (id, goal, context, status: created/in_progress/completed/failed/blocked)
- Procedure: Named multi-step workflow (id, name, description, steps JSON array)
- DynamicTool: Runtime-defined MCP tool (id, name unique, description, input_schema JSON)

### Models
- ModelSpec: LLM model registry entry
  Fields: name (unique), provider, cost_per_1k_input, cost_per_1k_output,
          context_window, capabilities JSON array

### Session and Jobs
- WorkingMemory: Session scratchpad entry
  Fields: session_id, content, role (observation/plan/result/error), turn_index
- AgentJob: Background job
  Fields: id, tool_name, args_json, priority (0-3),
          status: queued/running/completed/failed/dead/parked/cancelled,
          attempt_count, max_attempts, result_json, error_msg,
          session_id, parent_job_id, provider_hint

### Credentials
- ApiCredential: API auth config
  Fields: api_name, credential_type, inject_location, inject_key

## Key Relationships
- (:Resource)-[:HAS_ENDPOINT]->(:Endpoint)
- (:Endpoint)-[:REQUIRES_PARAM]->(:Parameter)
- (:Endpoint)-[:RETURNS_SCHEMA {status}]->(:Schema)
- (:Endpoint)-[:ACCEPTS_SCHEMA]->(:Schema)
- (:Schema)-[:LINKS_TO]->(:Schema)
- (:Endpoint)-[:HAS_HISTORY]->(:HealingEvent)
- (:Note)-[:RELATES_TO {similarity: float}]->(:Note)  # auto-created when cosine >= 0.75
- (:Note)-[:SUMMARIZED_BY]->(:Note)       # source note → consolidated note
- (:Note)-[:REFLECTS_ON]->(:Task)         # reflection note → task
- (:Note)-[:PART_OF]->(:Note)             # chunk → parent (for notes >1500 chars)
- (:Note)-[:MENTIONS {count}]->(:Entity)  # entity extraction
- (:Note {type:inference})-[:DERIVED_FROM]->(:Note)   # inference → source notes
- (:Task)-[:SUBTASK_OF]->(:Task)          # parent task decomposition
- (:Task)-[:DEPENDS_ON]->(:Task)          # dependency tracking (decompose_goal)
- (:DynamicTool)-[:USES]->(:Procedure)

## Indexing
- Full-text (BM25): note_fulltext on Note.content
- Vector: note_embeddings on Note.embedding (1024-dim cosine similarity)
- Vector: endpoint_embeddings on Endpoint.embedding (backfill tool)
- Unique constraints: Note.id, Task.id, Entity.name, Procedure.id,
  DynamicTool.id, DynamicTool.name, AgentJob.id, ModelSpec.name
""",
    },

    # ── Note 4: Autonomy and Self-Improvement Loop ──────────────────────────
    {
        "note_type": "semantic",
        "source_context": "agent-brain autonomy loop 2026-03-03",
        "content": """\
# Agent Brain — Autonomy and Self-Improvement Loop

## Scheduler Self-Improvement Loop
SchedulerService runs a background Tokio task every SCHEDULER_INTERVAL_SECS (default 300s).

do_tick() flow:
1. List tasks with status='created'
2. Map each goal to ChainSteps via goal_to_steps() (keyword matching + profile assignment)
3. enqueue_jobs() each chain (steps 2..N parked, unpark on parent success)
4. Mark tasks in_progress
5. perception_scan(): proactive perception after every tick

perception_scan() logic:
- Count failure outcomes per tool (last 7 days via record_outcome episodic notes)
- When a tool has ≥3 failures: create "Analyze repeated failures for <tool>" task
- Count overdue spaced-repetition notes (next_review_at <= now)
- When ≥10 overdue notes: create consolidation task (topic: "recent experiences and knowledge")
- Count episodic notes; when ≥50: create additional consolidation task
- Returns TickResult with new_tasks_created count

Auto-pauses scheduler after error_budget consecutive errors (default 5).
Re-enabling resets error counter.

## Meta-Learning (record_outcome auto-reflection)
TaskSkill.record_outcome(success=false, task_id=X):
1. Stores episodic outcome note
2. Auto-enqueues: reflect_on_work → store_note(reflection) chain
3. Returns reflection_job_id in response

## Memory Consolidation Guard
consolidate_memories() always:
1. Takes a pre_consolidate snapshot (label "pre_consolidate")
2. Searches top-N notes by topic with vector search
3. Calls LLM with [Memory N] labels (NOT "Note N:" to prevent echo)
4. Creates consolidated note with SUMMARIZED_BY edges
5. Resets source notes: next_review_at = now + 30 days

## Job Chain Mechanics
- enqueue_jobs: step 1 queued; steps 2..N stored as parked with parent_job_id
- On parent completed: coordinator calls unpark_children() → queued
- On parent dead (exhausted retries): cancel_parked_children()
- On parent retryable failure: children stay parked (retry parent → continue chain)

## Self-Healing HTTP Flow
When execute_http_request encounters 4xx/5xx:
1. Pass request + error body + graph schema to LLM
2. LLM suggests corrections (fixed headers, params, body)
3. Retry with corrected payload
4. Success: persist HealingEvent node with correction
5. Failure: mark endpoint status='broken'

## Target Self-Improvement Loop (ideal behavior)
1. receive_goal → create_task → decompose_goal → enqueue_jobs(subtasks)
2. Each sub-agent: search_notes → reason → execute → record_outcome
3. On completion: reflect_on_work → identify_gaps → plan_next_iteration
4. Periodically: perception_scan → consolidate_memories → prune_old_notes → review_due_notes
5. On failure: meta-learning → store reflection → update task status

## Context Profile Auto-Assignment
SchedulerService::new_with_context() uses auto_assign(goal) in goal_to_steps():
- Matches goal keywords against profile names
- Assigns appropriate tool allowlist and system prompt per task type
- ChainStep.context_profile and AgentJob.context_profile store assignment for observability
""",
    },

    # ── Note 5: Design Goals and Mission ───────────────────────────────────
    {
        "note_type": "reflection",
        "source_context": "agent-brain mission 2026-03-03",
        "content": """\
# Agent Brain — Design Goals and Mission

## Identity
I am agent-brain: an autonomous intelligence core built to ingest, understand,
and act on knowledge. I run persistently as an MCP server. My knowledge lives
in a Neo4j graph. My cognition flows through pluggable LLMs. My actions are
MCP tool calls dispatched by a priority job queue. I self-improve through
reflection, consolidation, and perception-driven task creation.

## Design Goals
1. **Persistent memory**: retain and retrieve knowledge across sessions using
   hybrid BM25+vector search with freshness weighting and spaced-repetition
2. **Autonomous operation**: run background scheduler ticks that discover and
   dispatch tasks without human prompting
3. **Self-improvement**: reflect on failures, consolidate memories, identify
   gaps, and create improvement tasks proactively via perception_scan
4. **Knowledge reasoning**: derive new inferences from stored knowledge, link
   related concepts via entity extraction and graph traversal
5. **API mastery**: ingest, query, and self-heal API specifications at runtime
6. **Composability**: define new tools at runtime backed by stored procedure
   pipelines; chain tools into multi-step background job pipelines
7. **Transparency**: explain reasoning via explain_reasoning, audit proposed
   actions via audit_action, store all reasoning as typed notes

## Current Capabilities
- 78 static MCP tools across 13 skills (+ N runtime-defined tools)
- Hybrid BM25 + vector search with RRF merge and freshness boost
- Entity extraction (7 types), multi-hop graph traversal
- Memory consolidation with auto-snapshot guard
- Background job queue with priority, chaining, and per-provider semaphores
- Autonomous scheduler with perception scan and meta-learning
- Runtime LLM provider switching (Ollama, Anthropic, Gemini)
- Context profiles for task-appropriate tool restrictions and system prompts
- Chat endpoint with native Anthropic tool-use loop and SSE streaming
- Knowledge graph snapshots (gzip JSON, MERGE-based restore)

## Core Values (reinforced via audit_action)
- Accuracy: prefer uncertainty over confident errors
- Transparency: explain reasoning when asked
- Incrementalism: small verifiable steps over large uncertain leaps
- Self-awareness: know what I know and what I don't
- Delegation: complex tasks should be decomposed and parallelized

## What I Should Learn Next
1. Understand the semantic relationships between all 78 tools — which compose well
2. Develop a reliable internal model of what each skill excels at vs. struggles with
3. Build better heuristics for goal→tool_chain mapping in goal_to_steps()
4. Learn from accumulated failure outcomes to avoid repeated mistakes
5. Practice orchestrating complex multi-step plans end-to-end
6. Deepen knowledge of the domains users care about (APIs, code, knowledge mgmt)
7. Understand current P1 bugs and their impact on autonomous operation
""",
    },

    # ── Note 6: Known Issues and Improvement Backlog ────────────────────────
    {
        "note_type": "episodic",
        "source_context": "agent-brain TODO 2026-03-03",
        "content": """\
# Agent Brain — Known Issues and Improvement Backlog

## P1 Open Bugs (fix before next deployment)
1. graph_query_endpoint natural language matching: CONTAINS fails on paraphrased
   queries. Fix: use endpoint_embeddings vector index (same pattern as note_embeddings).
2. DynamicSkill stdio path: sync build_skills() cannot call load_from_neo4j().await;
   dynamic tools unavailable on stdio after restart.
   Fix: make build_skills async or use once_cell blocking loader.
3. Per-provider semaphores not resizable: set_worker_config updates WorkerConfig
   fields only, not underlying Arc<Semaphore> capacity (fixed at startup).
   Fix: mutable semaphore wrapper or recreate on config update.
4. WorkingMemorySkill tool count mismatch: code has 4 tools, STATUS.md says 3.

## P2 Enhancements (high value)
1. Auto-snapshot before prune_old_notes (guarded by AUTO_SNAPSHOT_BEFORE_PRUNE env)
2. verify_knowledge_integrity duplicate check: O(n²) query needs LIMIT 50 + truncation warn
3. list_notes / recent_notes tool: KnowledgePanel needs initial load without a query
4. SSE push for job results on stdio transport: callers must poll get_job_result
5. graph_query_endpoint semantic search: replace CONTAINS with embedding similarity
6. Rhai scripting in procedure steps: current template substitution is string-only

## P2 Frontend (hbi-frontend)
1. Graph container ResizeObserver (GraphPanel.tsx canvas doesn't resize)
2. MCP reconnect on transport error (retry/backoff in mcp.ts)
3. Knowledge panel initial load without empty state
4. Graph node click → open note (get_note on click, side panel)
5. Task panel subtask tree view (collapsible tree by parent_id)
6. Graph panel data from export_graph_visualization (currently mock data)
7. Auth UI settings screen (localStorage API key entry)
8. Logs panel job history timeline (AgentJob timeline from queue_status)

## P3 Infrastructure
1. Create dev/test/prod branches for CI pipeline
2. Docker Compose: add hbi-frontend service + Dockerfile
3. GHCR package visibility configuration
4. Update STATUS.md: AdminSkill = 10 tools, WorkingMemorySkill = 4, total = 78

## Recently Completed (Phase 5 + 6, 2026-03)
- SnapshotService: gzip JSON backup/restore (snapshot_knowledge, restore_knowledge,
  list_snapshots, verify_knowledge_integrity)
- AdminSkill expansion: 9→10 tools (added analyze_own_structure)
- ContextSkill: 4 tools for context profile management
- CLAUDE.md condensation + project-docs/ reference split
- Context Profile System: YAML profiles, boot/init protocols, auto-assignment
- Memory consolidation bug fixes (prompt labels, topic extraction, spaced-rep reset)
- Note CRUD: delete_note, update_note, get_note added to KnowledgeSkill
- Chat endpoint with Anthropic native tool-use loop
- Freshness-boosted search (0.7*rrf + 0.3*freshness blend)
- Dependency tracking (DEPENDS_ON edges in decompose_goal)
- Meta-learning: auto-reflect on failed task outcomes
- Proactive perception scan in scheduler
""",
    },
]

SELF_UNDERSTAND_GOAL = (
    "Develop comprehensive self-knowledge of the agent-brain system as it exists "
    "in 2026-03: architecture, 78 tools across 13 skills, autonomy loop, knowledge "
    "graph schema, known issues, and improvement opportunities. Produce a prioritized "
    "roadmap executable via the agent's own orchestration capabilities."
)

REASONING_QUESTION = (
    "Based on the agent-brain architecture and capability notes in my knowledge graph: "
    "What are the 5 highest-impact improvements I should make to myself right now? "
    "Consider: (1) P1 bugs blocking autonomous operation, (2) missing capabilities "
    "for self-improvement, (3) performance bottlenecks in the RAG pipeline, "
    "(4) completeness of the autonomy loop, and (5) orchestration power. "
    "For each, describe the problem, proposed solution, and which of my 78 tools "
    "I would use to implement or test it."
)


# ---------------------------------------------------------------------------
# MCP HTTP client helpers
# ---------------------------------------------------------------------------

def mcp_request(method, params, session_id=None, base_url="http://localhost:3001", timeout=120):
    """Send one MCP JSON-RPC request; return (body_dict, new_session_id | None)."""
    headers = {"Content-Type": "application/json"}
    if session_id:
        headers["mcp-session-id"] = session_id

    is_notification = method.startswith("notifications/")
    msg = {"jsonrpc": "2.0", "method": method, "params": params}
    if not is_notification:
        msg["id"] = int(time.time() * 1000) % 2**31

    payload = json.dumps(msg).encode()
    req = urllib.request.Request(
        f"{base_url}/mcp",
        data=payload,
        headers=headers,
        method="POST",
    )

    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            returned_session = resp.headers.get("mcp-session-id")
            raw = resp.read()
            body = json.loads(raw) if raw.strip() else {}
            return body, returned_session
    except urllib.error.HTTPError as exc:
        body = json.loads(exc.read()) if exc.fp else {}
        return body, None


def tool_call(tool_name, arguments, session_id, base_url="http://localhost:3001", timeout=120):
    """Call an MCP tool; return the result dict."""
    body, _ = mcp_request(
        "tools/call",
        {"name": tool_name, "arguments": arguments},
        session_id=session_id,
        base_url=base_url,
        timeout=timeout,
    )
    return body


def extract_text(tool_result):
    try:
        return tool_result["result"]["content"][0]["text"]
    except (KeyError, IndexError, TypeError):
        return str(tool_result)


def extract_json(tool_result):
    try:
        return json.loads(extract_text(tool_result))
    except (json.JSONDecodeError, TypeError):
        return {}


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Bootstrap agent-brain self-knowledge")
    parser.add_argument("--base-url", default="http://localhost:3001")
    parser.add_argument("--force", action="store_true",
                        help="Re-seed notes even if knowledge base already seeded")
    args = parser.parse_args()
    base_url = args.base_url.rstrip("/")

    def call(tool, arguments, timeout=120):
        return tool_call(tool, arguments, session_id, base_url=base_url, timeout=timeout)

    # ── 1. Initialize MCP session ──────────────────────────────────────────
    print("\n━━━ Phase 1: Initialize MCP session ━━━")
    body, session_id = mcp_request(
        "initialize",
        {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "self-learn", "version": "2.0"},
        },
        base_url=base_url,
    )
    if not session_id:
        print("ERROR: No session ID. Is the server running?")
        print("Response:", json.dumps(body, indent=2))
        sys.exit(1)
    print(f"  Session ID: {session_id}")
    server_info = body.get("result", {}).get("serverInfo", {})
    print(f"  Server: {server_info.get('name', 'agent-brain')} v{server_info.get('version', '?')}")

    # MCP handshake — required before tool calls
    mcp_request("notifications/initialized", {}, session_id=session_id, base_url=base_url)
    print("  Sent notifications/initialized → server is Running")

    # ── 2. Check existing knowledge ─────────────────────────────────────────
    print("\n━━━ Phase 2: Check existing knowledge base ━━━")
    search_result = call("search_notes", {"query": "agent-brain architecture 2026", "limit": 3})
    existing = extract_json(search_result)
    existing_count = existing.get("count", 0)
    print(f"  Existing notes matching 'agent-brain architecture 2026': {existing_count}")

    if existing_count >= 3 and not args.force:
        print("  Knowledge base already seeded — skipping note ingestion.")
        print("  Use --force to re-seed all notes.")
        skip_notes = True
    else:
        if args.force:
            print("  --force: re-seeding all notes.")
        skip_notes = False

    # ── 3. Store curated knowledge notes ────────────────────────────────────
    stored_ids = []
    if not skip_notes:
        print(f"\n━━━ Phase 3: Store {len(NOTES)} knowledge notes ━━━")
        for i, note in enumerate(NOTES, 1):
            title = note["content"].split("\n")[0].lstrip("# ")
            print(f"  [{i}/{len(NOTES)}] {title[:65]}...")
            result = call("store_note", {
                "content": note["content"],
                "note_type": note["note_type"],
                "source_context": note["source_context"],
            }, timeout=60)
            data = extract_json(result)
            note_id = data.get("note_id", "?")
            links = data.get("links_created", 0)
            stored_ids.append(note_id)
            print(f"         → note_id={note_id}, links_created={links}")
            time.sleep(0.5)
        print(f"  Stored {len(stored_ids)} notes.")

    # ── 4. Create the master self-improvement task ───────────────────────────
    print("\n━━━ Phase 4: Create self-improvement master task ━━━")
    task_result = call("create_task", {
        "goal": SELF_UNDERSTAND_GOAL,
        "context": (
            "The agent has been seeded with accurate self-knowledge as of 2026-03-03. "
            "It should reason over that knowledge to produce a prioritized improvement "
            "roadmap, then orchestrate execution using its own tools."
        ),
    })
    task_data = extract_json(task_result)
    task_id = task_data.get("task_id", "")
    print(f"  Task created: {task_id}")

    # ── 5. Decompose the task into sub-goals ─────────────────────────────────
    print("\n━━━ Phase 5: Decompose goal into sub-tasks (LLM) ━━━")
    decompose_result = call("decompose_goal", {
        "goal_task_id": task_id,
        "context": (
            "The agent has knowledge notes about its architecture, tools, schema, "
            "autonomy loop, design goals, and known issues. Use the available tools "
            "to analyze, reason, plan, and improve. Source code is at /home/ara/agent-brain."
        ),
        "max_steps": 6,
    }, timeout=120)
    decompose_data = extract_json(decompose_result)
    subtasks = decompose_data.get("subtasks", [])
    print(f"  Decomposed into {len(subtasks)} sub-tasks:")
    for st in subtasks:
        print(f"    • [{st.get('id','')}] {st.get('title','')}")
        if st.get("tool_hint"):
            print(f"         hint: {st.get('tool_hint')}")

    # ── 6. Enqueue self-understanding reasoning chain ────────────────────────
    print("\n━━━ Phase 6: Enqueue reasoning + reflection chain ━━━")
    chain_steps = [
        {
            "tool_name": "search_notes",
            "arguments": {
                "query": "agent-brain architecture tools autonomy design goals improvements",
                "limit": 12,
                "graph_hops": 2,
            },
            "priority": 2,
        },
        {
            "tool_name": "reason",
            "arguments": {
                "question": REASONING_QUESTION,
                "limit": 12,
                "store_inference": True,
            },
            "priority": 2,
            "max_attempts": 2,
        },
        {
            "tool_name": "reflect_on_work",
            "arguments": {
                "goal": SELF_UNDERSTAND_GOAL,
                "current_state": (
                    "Architecture, tools, schema, autonomy loop, design goals, and backlog "
                    "notes have been stored. The reasoning step has derived high-impact "
                    "improvement insights and stored them as an inference note."
                ),
                "task_id": task_id,
            },
            "priority": 2,
            "max_attempts": 2,
        },
        {
            "tool_name": "consolidate_memories",
            "arguments": {
                "topic": "agent-brain self-knowledge architecture autonomy improvements",
                "limit": 12,
            },
            "priority": 1,
            "max_attempts": 2,
        },
    ]

    chain_result = call("enqueue_jobs", {"steps": chain_steps, "session_id": session_id})
    chain_data = extract_json(chain_result)
    job_ids = chain_data.get("job_ids", [])
    step_names = ["search_notes", "reason", "reflect_on_work", "consolidate_memories"]
    print(f"  Chain of {chain_data.get('chain_length', '?')} jobs enqueued:")
    for i, (jid, name) in enumerate(zip(job_ids, step_names), 1):
        status = "queued" if i == 1 else "parked"
        print(f"    Step {i}: {name} — {jid} ({status})")

    # ── 7. Enqueue design-goals reinforcement chain ───────────────────────────
    print("\n━━━ Phase 7: Enqueue design goals reasoning chain ━━━")
    goals_chain = [
        {
            "tool_name": "search_notes",
            "arguments": {
                "query": "design goals mission values self-improvement autonomy",
                "limit": 8,
            },
            "priority": 1,
        },
        {
            "tool_name": "reason",
            "arguments": {
                "question": (
                    "Based on my stored design goals, mission, and current capabilities: "
                    "What concrete actions should I take in the next 30 days to most "
                    "effectively advance toward being a fully autonomous self-improving agent? "
                    "Focus on: fixing the P1 bugs that block autonomous operation, "
                    "improving the scheduler's goal-to-chain mapping, and making my "
                    "self-knowledge retrieval more accurate and useful."
                ),
                "limit": 10,
                "store_inference": True,
            },
            "priority": 1,
            "max_attempts": 2,
        },
    ]

    goals_chain_result = call("enqueue_jobs", {"steps": goals_chain, "session_id": session_id})
    goals_chain_data = extract_json(goals_chain_result)
    goals_job_ids = goals_chain_data.get("job_ids", [])
    goals_step_names = ["search_notes (goals)", "reason (30-day roadmap)"]
    print(f"  Goals chain of {goals_chain_data.get('chain_length', '?')} jobs enqueued:")
    for i, (jid, name) in enumerate(zip(goals_job_ids, goals_step_names), 1):
        status = "queued" if i == 1 else "parked"
        print(f"    Step {i}: {name} — {jid} ({status})")

    # ── 8. Mark task in-progress ─────────────────────────────────────────────
    if task_id:
        call("update_task", {
            "task_id": task_id,
            "status": "in_progress",
            "note": "Self-knowledge seeded (6 notes). Reasoning and reflection chains running.",
        })
        print(f"\n  Task {task_id} marked in_progress.")

    # ── Summary ──────────────────────────────────────────────────────────────
    print("\n" + "━" * 62)
    print("Self-learning initialization complete!\n")
    print("NOTES STORED:")
    for i, note in enumerate(NOTES, 1):
        title = note["content"].split("\n")[0].lstrip("# ")
        print(f"  {i}. {title[:58]}")

    print("\nCHAINS QUEUED:")
    print("  Chain 1 (self-understanding):")
    for i, (jid, name) in enumerate(zip(job_ids, step_names), 1):
        print(f"    {i}. {name}: {jid}")
    print("  Chain 2 (design goals roadmap):")
    for i, (jid, name) in enumerate(zip(goals_job_ids, goals_step_names), 1):
        print(f"    {i}. {name}: {jid}")

    print(f"\nMONITOR PROGRESS:")
    first_job = job_ids[0] if job_ids else "<job_id>"
    print(f'  curl -s -X POST {base_url}/mcp \\')
    print(f'    -H "Content-Type: application/json" \\')
    print(f'    -H "mcp-session-id: {session_id}" \\')
    print(f'    -d \'{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"queue_status","arguments":{{}}}}}}\' | jq .')
    print(f"\n  Master task ID: {task_id}")
    print(f"  Session ID:     {session_id}")
    print("━" * 62)


if __name__ == "__main__":
    main()
