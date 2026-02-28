#!/usr/bin/env python3
"""Drive the brain through a self-analysis session.

Steps:
  1. Search what it currently believes about itself
  2. Store a fresh ground-truth note covering the actual 64-tool/12-skill state
  3. Use reason() to have it compare old vs new understanding
  4. Consolidate all self-knowledge notes into a single summary
  5. Ask for its roadmap assessment
"""

import json
import sys
import textwrap
import requests

BASE = "http://localhost:3001/mcp"

HEADERS_BASE = {
    "Content-Type": "application/json",
    "mcp-protocol-version": "2024-11-05",
}

_req_id = 0


def _id():
    global _req_id
    _req_id += 1
    return _req_id


def init_session():
    r = requests.post(BASE, headers=HEADERS_BASE, json={
        "jsonrpc": "2.0", "id": _id(), "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "self_reflect", "version": "1.0"},
        },
    })
    r.raise_for_status()
    session_id = r.headers.get("mcp-session-id", "")
    if not session_id:
        print("ERROR: no session id returned")
        sys.exit(1)

    headers = {**HEADERS_BASE, "mcp-session-id": session_id}
    requests.post(BASE, headers=headers, json={
        "jsonrpc": "2.0", "method": "notifications/initialized",
    })
    print(f"Session: {session_id}\n")
    return headers


def call(headers, tool, args):
    r = requests.post(BASE, headers=headers, json={
        "jsonrpc": "2.0", "id": _id(), "method": "tools/call",
        "params": {"name": tool, "arguments": args},
    })
    r.raise_for_status()
    body = r.json()
    if "error" in body:
        return f"[ERROR] {body['error']}"
    result = body.get("result", {})
    content = result.get("content", [])
    if not content:
        return "[empty]"
    text = content[0].get("text", "")
    try:
        parsed = json.loads(text)
        return json.dumps(parsed, indent=2)
    except Exception:
        return text


def section(title):
    bar = "─" * 70
    print(f"\n{bar}")
    print(f"  {title}")
    print(f"{bar}")


def show(label, text, width=100):
    print(f"\n[{label}]")
    for line in text.splitlines():
        print(textwrap.fill(line, width=width) if len(line) > width else line)


# ─── Main ──────────────────────────────────────────────────────────────────────

headers = init_session()

# ── Step 1: what does the brain currently think it is? ──────────────────────
section("Step 1 — Current self-knowledge (searching existing notes)")

result = call(headers, "search_notes", {"query": "capabilities tools skills agent brain", "limit": 5})
show("search_notes: capabilities/tools/skills", result)

result2 = call(headers, "search_notes", {"query": "self knowledge understanding what I can do", "limit": 5})
show("search_notes: self-knowledge/understanding", result2)

# ── Step 2: store fresh ground-truth note ───────────────────────────────────
section("Step 2 — Storing updated ground-truth self-knowledge note")

ground_truth = """
Agent Brain — Current Capability State (as of 2026-02-27)

I am an autonomous MCP server implemented in Rust. I run as a persistent background
process backed by Neo4j for long-term memory and DuckDB for telemetry. Any
MCP-compatible client can connect to me via stdio or HTTP/SSE transport.

SKILLS AND TOOL COUNT: 64 static tools across 12 skills, plus N runtime-defined
tools via DynamicSkill.

1. ApiSkill (14 tools): ingest_openapi, graph_query_endpoint, execute_http_request,
   get_api_context, list_loaded_apis, clear_api_context, discover_openapi,
   build_openapi_from_docs, build_openapi_from_repo, export_openapi, diff_api_spec,
   configure_api_credential, list_api_credentials, delete_api_credential.
   Capable of self-healing API documentation when requests fail.

2. KnowledgeSkill (10 tools): store_note, search_notes, find_related_notes,
   prune_old_notes, consolidate_memories, review_due_notes, search_by_entity,
   reason, audit_action, explain_reasoning.
   Uses hybrid BM25+vector RRF search with spaced-repetition scheduling.

3. TaskSkill (6 tools): create_task, reflect_on_work, decompose_goal, update_task,
   list_tasks, record_outcome.

4. AgentSkill (8 tools): enqueue_agent, enqueue_chain, queue_status, get_job_result,
   cancel_job, retry_job, set_worker_config, drain_queue.
   Priority job queue (0-3) backed by Neo4j; per-provider semaphores
   (Ollama:3, Anthropic:2, Gemini:5); parked/chaining job lifecycle.

5. AdminSkill (4 tools): delete_api, purge_duplicate_endpoints,
   purge_orphaned_schemas, reset_graph.

6. ModelSkill (5 tools): list_models, use_model, register_model, select_model,
   get_model_stats.
   Supports runtime switching between Ollama, Anthropic, and Gemini providers.

7. SchedulerSkill (5 tools): start_scheduler, stop_scheduler, get_scheduler_status,
   configure_scheduler, run_scheduler_tick.
   Background Tokio task that autonomously dispatches created tasks as job chains
   every N seconds (default 300). Keyword-based goal→chain mapping. Error budget
   auto-pauses on repeated failures.

8. DynamicSkill (4 + runtime tools): define_tool, execute_procedure,
   list_dynamic_tools, remove_dynamic_tool.
   New MCP tools can be defined at runtime backed by stored procedure pipelines;
   they persist across restarts via Neo4j.

9. WorkingMemorySkill (3 tools): push_context, get_context, summarise_session.

10. ProcedureSkill (2 tools): store_procedure, search_procedures.

11. SleepSkill (2 tools): digest_experiences, analyze_gaps.
    Reads DuckDB telemetry (interactions + knowledge_gaps tables) to export
    training data and identify capability gaps. Requires TELEMETRY_DB_PATH env var.

12. SearchSkill (1 tool): search_web (SerpApi / Brave / Google Custom Search).

MEMORY ARCHITECTURE:
- Neo4j stores: Notes (with embeddings), Tasks (with SUBTASK_OF edges), AgentJobs,
  Procedures, DynamicTools, WorkingMemory, ModelSpecs, ApiCredentials,
  Resources/Endpoints/Schemas/HealingEvents.
- Notes support: chunking (>1500 chars), entity extraction, RELATES_TO similarity
  edges (cosine >= 0.75), DERIVED_FROM for inferences, SUMMARIZED_BY for
  consolidations, spaced-repetition review scheduling.
- DuckDB stores: interactions (every tool call), knowledge_gaps (failed searches).

DEPLOYMENT: Docker Compose stack (Neo4j + MCP server). HTTP transport on :3000.
Supports Bearer token auth (MCP_API_KEY). Session lifecycle: initialize →
notifications/initialized → tool calls.

KNOWN GAPS:
- graph_query_endpoint uses CONTAINS matching; semantic/vector search not yet wired
- DynamicSkill tools unavailable on stdio path after restart (async load issue)
- Per-provider semaphores not resizable at runtime via set_worker_config
- No SSE push notifications for completed job results (callers must poll)
- No CI/CD pipeline yet; no self-hosted runner for autonomous redeployment
- No git hook to trigger self-knowledge update on code changes
""".strip()

store_result = call(headers, "store_note", {
    "content": ground_truth,
    "note_type": "semantic",
    "source_context": "self_reflect.py ground-truth update 2026-02-27",
})
show("store_note result", store_result)

# ── Step 3: reason — compare old vs new understanding ────────────────────────
section("Step 3 — Reasoning: compare current understanding vs ground truth")

reason_result = call(headers, "reason", {
    "question": (
        "Based on the stored notes about my capabilities, what is my current "
        "accurate self-understanding? What gaps exist between what I previously "
        "believed about myself and what I actually am? What has changed most "
        "significantly?"
    ),
    "limit": 8,
    "store_inference": True,
})
show("reason: self-comparison", reason_result)

# ── Step 4: consolidate self-knowledge ───────────────────────────────────────
section("Step 4 — Consolidating all self-knowledge notes")

consolidate_result = call(headers, "consolidate_memories", {
    "topic": "agent brain capabilities skills tools self-knowledge",
    "limit": 10,
})
show("consolidate_memories result", consolidate_result)

# ── Step 5: roadmap ───────────────────────────────────────────────────────────
section("Step 5 — Roadmap: what should I build next?")

roadmap_result = call(headers, "reason", {
    "question": (
        "Given my current capabilities, known gaps, and the goal of becoming a "
        "fully autonomous self-improving agent, what should my development roadmap "
        "look like? What are the highest-leverage capabilities I am missing? "
        "Prioritise by impact on autonomy and self-improvement ability."
    ),
    "limit": 10,
    "store_inference": True,
})
show("reason: roadmap", roadmap_result)

section("Done")
print("\nAll notes persisted in Neo4j.\n")
