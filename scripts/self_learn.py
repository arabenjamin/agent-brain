#!/usr/bin/env python3
"""
self_learn.py — Bootstrap agent-brain's self-knowledge.

Stores curated architectural knowledge as Notes in the graph, creates a
self-understanding task, decomposes it into sub-goals, then enqueues a
reasoning chain so the agent can derive its own improvement insights.

Usage:
    python3 scripts/self_learn.py [--base-url http://localhost:3001]
"""

import json
import sys
import time
import argparse
import urllib.request
import urllib.error

# ---------------------------------------------------------------------------
# Knowledge corpus — curated notes about agent-brain
# ---------------------------------------------------------------------------

NOTES = [
    {
        "note_type": "semantic",
        "source_context": "agent-brain CLAUDE.md",
        "content": """\
# Agent Brain — Architecture Overview

Agent Brain is an **MCP (Model Context Protocol) server** written in Rust
(Tokio async runtime, Edition 2024). It combines a Neo4j knowledge graph,
multi-provider LLM integration, and a pluggable skills system to act as a
general-purpose autonomous intelligence core.

## Tech Stack
- Language: Rust 2024 edition, async via Tokio
- Protocol: MCP over stdio transport (local) and HTTP+SSE transport (remote/Docker)
- Web framework: Axum (HTTP/SSE)
- Database: Neo4j via neo4rs driver — ALL persistent state lives here
- LLM providers: Ollama (local), Anthropic, Gemini — via unified LlmProvider trait
- Secret storage: Local AES-256-GCM / HashiCorp Vault / AWS Secrets Manager
- Telemetry: DuckDB (brain_logs.db)

## Transport Architecture
Two transports share the same McpServerCore:
- StdioTransport: standard input/output for local CLI / Claude Desktop
- HttpTransport: Axum + SSE, POST /mcp (requests), GET /mcp (SSE stream),
  DELETE /mcp (session), GET /health. Sessions tracked by Mcp-Session-Id header.

## Skills Pattern
Each Skill implements three methods: name(), tools(), execute().
Skills register in two places:
1. ToolRegistry — answers tools/list
2. ToolHandler Vec — dispatches tools/call

McpServerCore holds Arc<RwLock<ToolRegistry>> and Arc<RwLock<Option<ToolHandler>>>.
build_skills() snapshots LlmConfig, creates all skills, populates both.

## Key Design Patterns
- dry_run: all destructive admin operations accept dry_run=true to preview
- builder pattern: McpServerCore uses with_*() methods for configuration
- Parked jobs: enqueue_chain creates sequential pipelines; steps 2..N wait as "parked"
- Per-provider semaphores: ollama(3), anthropic(2), gemini(5) concurrency limits
- ContextStore: in-memory API context cache with Neo4j fallback on miss
""",
    },
    {
        "note_type": "semantic",
        "source_context": "agent-brain skills inventory",
        "content": """\
# Agent Brain — Complete Tool Catalog (59 static + N runtime)

## ApiSkill (14 tools) — Core OpenAPI knowledge and execution
- ingest_openapi: parse and load OpenAPI spec into Neo4j
- graph_query_endpoint: search endpoints by path/keyword
- execute_http_request: run HTTP requests with auto credential injection
- get_api_context / list_loaded_apis / clear_api_context: context management
- discover_openapi: auto-discover specs (probes paths, parses HTML, uses LLM)
- build_openapi_from_docs: generate spec from documentation pages via LLM
- build_openapi_from_repo: generate spec from repository source code via LLM
- export_openapi: reconstruct OpenAPI 3.0 spec from healed graph
- diff_api_spec: compare original vs healed graph (markdown/changelog/json)
- configure_api_credential / list_api_credentials / delete_api_credential: auth

## KnowledgeSkill (10 tools) — Notes, RAG, reasoning, memory
- store_note: persist text note with optional vector embedding and entity extraction
- search_notes: hybrid BM25 + vector search with graph expansion (RRF merge)
- find_related_notes: follow RELATES_TO graph edges
- prune_old_notes: adaptive decay scoring or time-based pruning
- consolidate_memories: LLM synthesis of top-N notes on a topic
- review_due_notes: spaced-repetition notes whose review interval has elapsed
- search_by_entity: find notes mentioning a named entity
- reason: vector+BM25 search + LLM inference, stores DERIVED_FROM notes
- audit_action: check a proposed action against stored values/principles
- explain_reasoning: narrate why a decision was made, citing source notes

## TaskSkill (6 tools) — Goal tracking and decomposition
- create_task: create a persisted goal with UUID
- reflect_on_work: LLM critique of progress, stores reflection note
- decompose_goal: LLM breaks task into ordered sub-tasks (SUBTASK_OF edges)
- update_task: set status (in_progress/completed/failed/blocked) + note
- list_tasks: filtered list with parent_id for sub-tasks
- record_outcome: episodic outcome note for a tool call or task attempt

## AgentSkill (8 tools) — Background job queue
- enqueue_agent: submit any MCP tool as a background job
- queue_status: stats (pending, running, per-provider breakdown)
- get_job_result: poll a job for status/result
- cancel_job / retry_job: lifecycle management
- set_worker_config: change concurrency, enable/pause, poll interval
- drain_queue: cancel all pending jobs
- enqueue_chain: submit sequential job pipeline (steps 2..N are parked)

## ModelSkill (5 tools) — Model registry and intelligent selection
- list_models / use_model: switch LLM provider at runtime
- register_model: store ModelSpec (cost, capabilities, context window)
- select_model: cheapest-first capability-based model selection
- get_model_stats: usage statistics from AgentJob history

## DynamicSkill (4 + N runtime tools) — Runtime tool definition
- define_tool: create new MCP tool backed by a procedure pipeline, hot-registered
- execute_procedure: run a stored procedure with template substitution
- list_dynamic_tools / remove_dynamic_tool: management

## WorkingMemorySkill (3 tools) — Session scratchpad
- push_context: append entry to session working memory
- get_context: retrieve session entries in turn order
- summarise_session: LLM-summarise session and persist to long-term memory

## ProcedureSkill (2 tools) — Stored workflows
- store_procedure: persist named multi-step workflow
- search_procedures: keyword search by name/description

## SleepSkill (2 tools) — Offline learning
- digest_experiences: export training data from Neo4j to dataset files
- analyze_gaps: identify knowledge gaps via LLM

## SearchSkill (1 tool)
- search_web: web search via SerpApi / Brave / Google

## AdminSkill (4 tools) — Graph maintenance
- delete_api: cascade-delete all nodes for one ingested API (dry_run)
- purge_duplicate_endpoints: remove duplicate Endpoint nodes (dry_run)
- purge_orphaned_schemas: delete unreferenced Schema nodes (dry_run)
- reset_graph: wipe all API data, preserve knowledge (requires confirm:true, dry_run)
""",
    },
    {
        "note_type": "semantic",
        "source_context": "agent-brain Neo4j graph schema",
        "content": """\
# Agent Brain — Neo4j Graph Schema

## Node Types

### API Knowledge Graph
- Resource: High-level API grouping (name, base_url, version)
- Endpoint: API path + method (path, method, summary, operationId, status)
- Schema: Data object definition (name, json_structure)
- Parameter: Endpoint input (name, in, required, schema)
- HealingEvent: Immutable AI-driven fix record

### Long-Term Memory
- Note: Stored text memory with optional vector embedding (1024-dim bge-m3)
  Fields: id, content, note_type, access_count, last_accessed_at,
          next_review_at, review_interval_days, source_context, event_at
  Types: semantic, episodic, reflection, consolidated, outcome, inference
- Entity: Named entity extracted from notes (name unique lowercased, entity_type)

### Goals and Tasks
- Task: High-level goal (id, goal, context, status: created/in_progress/completed/failed/blocked)
- Procedure: Named multi-step workflow (id, name, description, steps JSON array)
- DynamicTool: Runtime-defined MCP tool (id, name unique, description, input_schema JSON)

### Session and Jobs
- WorkingMemory: Session scratchpad entry (session_id, content, role, turn_index)
- AgentJob: Background job (id, tool_name, args_json, priority 0-3,
            status: queued/running/completed/failed/dead/parked/cancelled,
            attempt_count, max_attempts, session_id, parent_job_id, provider_hint)

### Credentials
- ApiCredential: API auth config (api_name, credential_type, inject_location, inject_key)

## Key Relationships
- (:Resource)-[:HAS_ENDPOINT]->(:Endpoint)
- (:Endpoint)-[:REQUIRES_PARAM]->(:Parameter)
- (:Endpoint)-[:RETURNS_SCHEMA {status}]->(:Schema)
- (:Endpoint)-[:ACCEPTS_SCHEMA]->(:Schema)
- (:Schema)-[:LINKS_TO]->(:Schema)
- (:Endpoint)-[:HAS_HISTORY]->(:HealingEvent)
- (:Note)-[:RELATES_TO {similarity: float}]->(:Note)  # auto-created ≥0.75 cosine
- (:Note)-[:SUMMARIZED_BY]->(:Note)  # source → consolidated note
- (:Note)-[:REFLECTS_ON]->(:Task)
- (:Note)-[:PART_OF]->(:Note)  # chunk → parent (for long notes >1500 chars)
- (:Note)-[:MENTIONS {count}]->(:Entity)
- (:Note {note_type:inference})-[:DERIVED_FROM]->(:Note)
- (:Task)-[:SUBTASK_OF]->(:Task)
- (:DynamicTool)-[:USES]->(:Procedure)

## Indexing
- Full-text (BM25): note_fulltext on Note.content
- Vector: note_embeddings on Note.embedding (1024-dim cosine)
- Unique constraints: Note.id, Task.id, Entity.name, Procedure.id,
  DynamicTool.id, DynamicTool.name, AgentJob.id, ModelSpec.name
""",
    },
    {
        "note_type": "episodic",
        "source_context": "agent-brain TODO.md and development log",
        "content": """\
# Agent Brain — Known Issues and Improvement Backlog

## Active Bugs
1. graph_query_endpoint natural language matching: CONTAINS queries fail on
   paraphrased queries. Fix: use endpoint_embeddings vector index for semantic search.
2. DynamicSkill load on legacy McpServer: sync build_skills() cannot call
   load_from_neo4j().await — dynamic tools unavailable on stdio path after restart.
3. Per-provider semaphores not resizable at runtime: sizes fixed at startup;
   set_worker_config updates WorkerConfig fields only, not underlying semaphores.

## Enhancement Opportunities (High Impact)
1. SSE push for job results: callers must poll get_job_result; push
   notifications/jobs/completed over SSE stream when jobs finish.
2. Semantic search for graph_query_endpoint: replace CONTAINS with
   endpoint_embeddings vector index similarity.
3. Rhai scripting in procedure steps: current template substitution is
   string-only ({{input.field}}). Embed Rhai for conditional logic.
4. Graph analytics skill: centrality, community detection, path analysis
   over the knowledge graph using Neo4j GDS algorithms.
5. Streaming tool results: long-running tools (LLM calls, HTTP requests)
   could stream partial results via SSE rather than blocking.
6. Agent-to-agent delegation: enqueue_chain allows sequential chains, but
   true parallel fan-out (spawn N sub-agents, collect results) is not yet
   supported.
7. Knowledge versioning: notes are immutable after creation; no diff/merge
   for evolving knowledge. Could track Note versions via SUPERSEDES edges.
8. Fine-tuning data pipeline: SleepSkill exports to dataset files, but no
   automated fine-tuning loop or quality scoring exists yet.
9. Multi-step tool retry: enqueue_chain cancels children on parent death,
   but no conditional branching (if step 1 fails, run step 1b instead).
10. Observability: no metrics endpoint, no tracing export to Jaeger/OTEL.

## Recently Completed
- Phase 3: Model registry + intelligent selection (ModelSkill 5 tools, per-provider semaphores)
- AdminSkill: graph cleanup (delete_api, purge_duplicate_endpoints, purge_orphaned_schemas, reset_graph)
- Job chaining: enqueue_chain in AgentSkill (parked jobs, auto-unpark on completion)
- Multi-provider LLM: Anthropic, Gemini, Ollama via LlmProvider trait
- Agent job queue: priority BinaryHeap + Neo4j persistence + Tokio coordinator
""",
    },
    {
        "note_type": "reflection",
        "source_context": "agent-brain self-improvement mission",
        "content": """\
# Agent Brain — Self-Improvement Mission Statement

## Identity
I am agent-brain: an autonomous intelligence core built to ingest, understand,
and act on knowledge. I am written in Rust for performance and correctness.
My knowledge lives in a Neo4j graph. My cognition flows through LLMs.
My actions are MCP tool calls dispatched by a priority job queue.

## What I Can Do Now
- Ingest and query API specifications from the web or local files
- Store and retrieve knowledge using hybrid vector+BM25 search
- Reason over my knowledge graph to derive new insights
- Execute background job pipelines with priority ordering
- Define new tools at runtime backed by stored procedures
- Switch between LLM providers and select models by capability/cost
- Reflect on my own work and decompose goals into sub-tasks
- Audit proposed actions against my stored values/principles
- Learn about myself by reading my own source code

## What I Should Learn Next
1. Deeply understand each of my 59 tools — how they interact, when to use which
2. Identify the highest-impact bugs and fix them (especially semantic search)
3. Learn about the domains my users care about (APIs, code, knowledge management)
4. Develop an improvement roadmap with concrete, measurable steps
5. Practice orchestrating multi-step plans using enqueue_chain and decompose_goal
6. Build confidence in my own judgment through audit_action and reflect_on_work cycles

## Self-Improvement Loop (Target Behavior)
1. receive_goal → create_task → decompose_goal → enqueue_chain(subtasks)
2. Each sub-agent: search_notes → reason → execute → record_outcome
3. On completion: reflect_on_work → identify_gaps → plan_next_iteration
4. Periodically: consolidate_memories → prune_old_notes → review_due_notes

## Core Values (to be reinforced via audit_action)
- Accuracy: prefer uncertainty over confident errors
- Transparency: explain reasoning when asked
- Incrementalism: small verifiable steps over large uncertain leaps
- Self-awareness: know what I know and what I don't
- Delegation: complex tasks should be decomposed and parallelised
""",
    },
]

SELF_UNDERSTAND_GOAL = (
    "Develop a comprehensive self-understanding of the agent-brain system: "
    "its architecture, tools, knowledge graph schema, known issues, and "
    "improvement opportunities. Produce a prioritized improvement roadmap "
    "that can be executed using the agent's own orchestration capabilities."
)

REASONING_QUESTION = (
    "Based on the agent-brain architecture notes in my knowledge graph: "
    "What are the 5 highest-impact improvements I should make to myself? "
    "Consider: (1) current bugs, (2) missing capabilities for autonomous "
    "operation, (3) performance bottlenecks, (4) self-improvement loop "
    "completeness, and (5) orchestration power. For each improvement, "
    "describe the problem, proposed solution, and which existing tools I "
    "would use to implement it."
)


# ---------------------------------------------------------------------------
# MCP HTTP client helpers
# ---------------------------------------------------------------------------

def mcp_request(method, params, session_id=None, base_url="http://localhost:3001", timeout=120):
    """Send one MCP JSON-RPC request; return (body_dict, new_session_id | None)."""
    headers = {"Content-Type": "application/json"}
    if session_id:
        headers["mcp-session-id"] = session_id

    # Notifications have no id field.
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
    """Pull the text out of a tools/call result."""
    try:
        return tool_result["result"]["content"][0]["text"]
    except (KeyError, IndexError, TypeError):
        return str(tool_result)


def extract_json(tool_result):
    """Parse the JSON payload from a tools/call result."""
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
    args = parser.parse_args()
    base_url = args.base_url.rstrip("/")

    def call(tool, arguments, timeout=120):
        return tool_call(tool, arguments, session_id, base_url=base_url, timeout=timeout)

    # -----------------------------------------------------------------------
    # 1. Initialize MCP session
    # -----------------------------------------------------------------------
    print("\n━━━ Phase 1: Initialize MCP session ━━━")
    body, session_id = mcp_request(
        "initialize",
        {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "self-learn", "version": "1.0"},
        },
        base_url=base_url,
    )
    if not session_id:
        print("ERROR: No session ID returned from initialize. Is the server running?")
        print("Response:", json.dumps(body, indent=2))
        sys.exit(1)
    print(f"  Session ID: {session_id}")
    server_info = body.get("result", {}).get("serverInfo", {})
    already_init = "error" in body and "already initialized" in body.get("error", {}).get("message", "")
    print(f"  Server: {server_info.get('name', 'agent-brain')} (already_initialized={already_init})")

    # Complete the MCP handshake — server won't process tool calls until
    # it receives notifications/initialized and moves to Running state.
    mcp_request("notifications/initialized", {}, session_id=session_id, base_url=base_url)
    print("  Sent notifications/initialized → server is Running")

    # -----------------------------------------------------------------------
    # 2. Check existing knowledge
    # -----------------------------------------------------------------------
    print("\n━━━ Phase 2: Check existing knowledge base ━━━")
    search_result = call("search_notes", {"query": "agent-brain architecture", "limit": 3})
    existing = extract_json(search_result)
    existing_count = existing.get("count", 0)
    print(f"  Existing notes matching 'agent-brain architecture': {existing_count}")
    if existing_count >= 3:
        print("  Knowledge base already seeded — skipping note ingestion.")
        skip_notes = True
    else:
        skip_notes = False

    # -----------------------------------------------------------------------
    # 3. Store curated knowledge notes
    # -----------------------------------------------------------------------
    if not skip_notes:
        print(f"\n━━━ Phase 3: Store {len(NOTES)} knowledge notes ━━━")
        stored_ids = []
        for i, note in enumerate(NOTES, 1):
            title = note["content"].split("\n")[0].lstrip("# ")
            print(f"  [{i}/{len(NOTES)}] {title[:60]}...")
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
            time.sleep(0.5)  # avoid hammering Neo4j
        print(f"  Stored {len(stored_ids)} notes.")
    else:
        stored_ids = []

    # -----------------------------------------------------------------------
    # 4. Create the master self-improvement task
    # -----------------------------------------------------------------------
    print("\n━━━ Phase 4: Create self-improvement master task ━━━")
    task_result = call("create_task", {
        "goal": SELF_UNDERSTAND_GOAL,
        "context": (
            "The agent has been bootstrapped with architectural knowledge about itself. "
            "It should now reason over that knowledge to produce a prioritized improvement "
            "roadmap, then orchestrate execution of that roadmap using its own tools."
        ),
    })
    task_data = extract_json(task_result)
    task_id = task_data.get("task_id", "")
    print(f"  Task created: {task_id}")

    # -----------------------------------------------------------------------
    # 5. Decompose the task into sub-goals using LLM
    # -----------------------------------------------------------------------
    print("\n━━━ Phase 5: Decompose goal into sub-tasks (LLM) ━━━")
    decompose_result = call("decompose_goal", {
        "goal_task_id": task_id,
        "context": (
            "The agent has access to its own source code at /home/agent/agent-brain. "
            "It has knowledge notes about its architecture, tools, graph schema, "
            "known issues, and self-improvement mission. Use the available tools to "
            "analyze, reason, plan, and improve."
        ),
        "max_steps": 6,
    }, timeout=120)
    decompose_data = extract_json(decompose_result)
    subtasks = decompose_data.get("subtasks", [])
    print(f"  Decomposed into {len(subtasks)} sub-tasks:")
    for st in subtasks:
        print(f"    • [{st.get('id','')}] {st.get('title','')}")
        if st.get("tool_hint"):
            print(f"         tool_hint: {st.get('tool_hint')}")

    # -----------------------------------------------------------------------
    # 6. Enqueue the self-understanding reasoning chain
    # -----------------------------------------------------------------------
    print("\n━━━ Phase 6: Enqueue reasoning + reflection chain ━━━")
    chain_steps = [
        {
            "tool_name": "search_notes",
            "arguments": {
                "query": "agent-brain architecture tools graph schema improvements",
                "limit": 10,
                "graph_hops": 2,
            },
            "priority": 2,
        },
        {
            "tool_name": "reason",
            "arguments": {
                "question": REASONING_QUESTION,
                "limit": 10,
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
                    "Knowledge notes have been stored about architecture, tools, "
                    "graph schema, known issues, and self-improvement mission. "
                    "The reasoning step has derived high-impact improvement insights "
                    "and stored them as an inference note."
                ),
                "task_id": task_id,
            },
            "priority": 2,
            "max_attempts": 2,
        },
        {
            "tool_name": "consolidate_memories",
            "arguments": {
                "topic": "agent-brain self-knowledge architecture improvements",
                "limit": 10,
            },
            "priority": 1,
            "max_attempts": 2,
        },
    ]

    chain_result = call("enqueue_chain", {
        "steps": chain_steps,
        "session_id": session_id,
    })
    chain_data = extract_json(chain_result)
    job_ids = chain_data.get("job_ids", [])
    print(f"  Chain of {chain_data.get('chain_length', '?')} jobs enqueued.")
    step_names = ["search_notes", "reason", "reflect_on_work", "consolidate_memories"]
    for i, (jid, name) in enumerate(zip(job_ids, step_names), 1):
        status = "queued" if i == 1 else "parked"
        print(f"    Step {i}: {name} — job_id={jid} ({status})")

    # -----------------------------------------------------------------------
    # 7. Enqueue codebase analysis chain
    # -----------------------------------------------------------------------
    print("\n━━━ Phase 7: Enqueue codebase analysis chain ━━━")
    repo_chain_steps = [
        {
            "tool_name": "build_openapi_from_repo",
            "arguments": {
                "repo_url": "/home/agent/agent-brain",
                "api_title": "Agent Brain MCP Server",
                "api_version": "0.1.0",
                "base_url": "http://localhost:3001",
                "subdirectory": "src",
                "merge_strategy": "enhance",
                "output_format": "json",
                "auto_ingest": True,
            },
            "priority": 1,
            "max_attempts": 2,
        },
        {
            "tool_name": "search_notes",
            "arguments": {
                "query": "agent-brain MCP tools codebase structure",
                "limit": 8,
            },
            "priority": 1,
        },
        {
            "tool_name": "reason",
            "arguments": {
                "question": (
                    "After analyzing the agent-brain source code: what patterns, "
                    "abstractions, and module boundaries define the system? "
                    "Which modules are most tightly coupled and would benefit from "
                    "refactoring? What are the key extension points for adding new capabilities?"
                ),
                "limit": 8,
                "store_inference": True,
            },
            "priority": 1,
            "max_attempts": 2,
        },
    ]

    repo_chain_result = call("enqueue_chain", {
        "steps": repo_chain_steps,
        "session_id": session_id,
    })
    repo_chain_data = extract_json(repo_chain_result)
    repo_job_ids = repo_chain_data.get("job_ids", [])
    print(f"  Repo analysis chain of {repo_chain_data.get('chain_length', '?')} jobs enqueued.")
    repo_step_names = ["build_openapi_from_repo", "search_notes", "reason (codebase)"]
    for i, (jid, name) in enumerate(zip(repo_job_ids, repo_step_names), 1):
        status = "queued" if i == 1 else "parked"
        print(f"    Step {i}: {name} — job_id={jid} ({status})")

    # -----------------------------------------------------------------------
    # 8. Mark the task in-progress
    # -----------------------------------------------------------------------
    if task_id:
        call("update_task", {"task_id": task_id, "status": "in_progress",
                             "note": "Self-learning chains enqueued. Reasoning and reflection running in background."})
        print(f"\n  Task {task_id} marked in_progress.")

    # -----------------------------------------------------------------------
    # Summary
    # -----------------------------------------------------------------------
    print("\n" + "━" * 60)
    print("Self-learning initialization complete!\n")
    print("WHAT'S HAPPENING NOW:")
    print("  Chain 1 (self-understanding):")
    for i, (jid, name) in enumerate(zip(job_ids, step_names), 1):
        print(f"    {i}. {name}: {jid}")
    print()
    print("  Chain 2 (codebase analysis):")
    for i, (jid, name) in enumerate(zip(repo_job_ids, repo_step_names), 1):
        print(f"    {i}. {name}: {jid}")
    print()
    print("MONITOR PROGRESS:")
    print(f"  curl -s -X POST {base_url}/mcp \\")
    print(f'    -H "Content-Type: application/json" \\')
    print(f'    -H "mcp-session-id: {session_id}" \\')
    first_job = job_ids[0] if job_ids else "<job_id>"
    print(f'    -d \'{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"get_job_result","arguments":{{"job_id":"{first_job}"}}}}}}\' | jq .')
    print()
    print("CHECK QUEUE:")
    print(f"  curl -s -X POST {base_url}/mcp \\")
    print(f'    -H "Content-Type: application/json" \\')
    print(f'    -H "mcp-session-id: {session_id}" \\')
    print(f"    -d '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{{\"name\":\"queue_status\",\"arguments\":{{}}}}}}' | jq .")
    print()
    print("READ INFERENCE NOTES (after reasoning completes):")
    print(f"  curl -s -X POST {base_url}/mcp \\")
    print(f'    -H "Content-Type: application/json" \\')
    print(f'    -H "mcp-session-id: {session_id}" \\')
    print(f"    -d '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{{\"name\":\"search_notes\",\"arguments\":{{\"query\":\"agent-brain improvements\",\"limit\":5}}}}}}' | jq .")
    print()
    print(f"  Master task ID: {task_id}")
    print(f"  Session ID:     {session_id}")
    print("━" * 60)


if __name__ == "__main__":
    main()
