#!/usr/bin/env python3
"""
self_reflect.py — Drive the brain through a self-analysis session.

Steps:
  1. Search what it currently believes about itself
  2. Store a fresh ground-truth note covering the actual 78-tool/13-skill state
  3. reason() to compare old vs new understanding
  4. Consolidate all self-knowledge notes into a single summary
  5. Derive a development roadmap prioritized by autonomy impact

Usage:
    python3 scripts/self_reflect.py [--base-url http://localhost:3001]
"""

import json
import sys
import textwrap
import argparse
import urllib.request
import urllib.error
import time

BASE_URL = "http://localhost:3001"

_req_id = 0


def _id():
    global _req_id
    _req_id += 1
    return _req_id


def mcp_post(url, session_id, body):
    headers = {
        "Content-Type": "application/json",
        "mcp-protocol-version": "2024-11-05",
    }
    if session_id:
        headers["mcp-session-id"] = session_id
    payload = json.dumps(body).encode()
    req = urllib.request.Request(url, data=payload, headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=180) as resp:
            returned_session = resp.headers.get("mcp-session-id")
            raw = resp.read()
            result = json.loads(raw) if raw.strip() else {}
            return result, returned_session
    except urllib.error.HTTPError as exc:
        body_err = json.loads(exc.read()) if exc.fp else {}
        return body_err, None


def init_session(base_url):
    body, session_id = mcp_post(f"{base_url}/mcp", None, {
        "jsonrpc": "2.0", "id": _id(), "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "self_reflect", "version": "2.0"},
        },
    })
    if not session_id:
        print("ERROR: no session id returned")
        print("Response:", json.dumps(body, indent=2))
        sys.exit(1)

    mcp_post(f"{base_url}/mcp", session_id, {
        "jsonrpc": "2.0", "method": "notifications/initialized",
    })
    print(f"Session: {session_id}\n")
    return session_id


def call(base_url, session_id, tool, args):
    body, _ = mcp_post(f"{base_url}/mcp", session_id, {
        "jsonrpc": "2.0", "id": _id(), "method": "tools/call",
        "params": {"name": tool, "arguments": args},
    })
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


# ─── Ground-truth note (accurate as of 2026-03-03) ──────────────────────────

GROUND_TRUTH = """
Agent Brain — Current Capability State (as of 2026-03-03)

I am an autonomous MCP server implemented in Rust (Tokio async, Edition 2024). I run
as a persistent background process backed by Neo4j for long-term memory and DuckDB
for telemetry. MCP-compatible clients connect via stdio or HTTP/SSE transport.

SKILLS AND TOOL COUNT: 78 static tools across 13 skills, plus N runtime-defined tools
via DynamicSkill.

1. ApiSkill (14 tools): ingest_openapi, graph_query_endpoint, execute_http_request,
   get_api_context, list_loaded_apis, clear_api_context, discover_openapi,
   build_openapi_from_docs, build_openapi_from_repo, export_openapi, diff_api_spec,
   configure_api_credential, list_api_credentials, delete_api_credential.
   Self-healing: 4xx/5xx → LLM analyzes → retry with correction → HealingEvent node.

2. KnowledgeSkill (15 tools): store_note, search_notes, find_related_notes,
   prune_old_notes, consolidate_memories, review_due_notes, search_by_entity,
   reason, audit_action, explain_reasoning, ask_clarification, get_note,
   delete_note, update_note, export_graph_visualization.
   Hybrid BM25+vector RRF search with freshness boost (0.7*rrf + 0.3*freshness).
   Entity extraction: 7 types (person/tool/technology/concept/organisation/url/date).
   Long notes auto-chunked (>1500 chars via PART_OF edges).
   Multi-hop graph traversal via entity_expansion parameter.
   Auto-snapshot before consolidate_memories.

3. TaskSkill (6 tools): create_task, reflect_on_work, decompose_goal, update_task,
   list_tasks, record_outcome.
   Dependency tracking: DEPENDS_ON edges from decompose_goal.
   Meta-learning: record_outcome(failure) auto-enqueues reflect→store chain.

4. AgentSkill (8 tools): enqueue_agent, enqueue_chain, queue_status, get_job_result,
   cancel_job, retry_job, set_worker_config, drain_queue.
   Priority job queue (0-3) backed by Neo4j. Per-provider semaphores:
   Ollama:3, Anthropic:2, Gemini:5. Parked/chaining lifecycle.

5. AdminSkill (10 tools): delete_api, purge_duplicate_endpoints,
   purge_orphaned_schemas, reset_graph, backfill_endpoint_embeddings,
   snapshot_knowledge, restore_knowledge, list_snapshots,
   verify_knowledge_integrity, analyze_own_structure.
   SnapshotService: gzip JSON (.json.gz), MERGE-based restore (idempotent).

6. ModelSkill (5 tools): list_models, use_model, register_model, select_model,
   get_model_stats.
   Runtime switching between Ollama (default granite4), Anthropic, Gemini.

7. SchedulerSkill (5 tools): start_scheduler, stop_scheduler, get_scheduler_status,
   configure_scheduler, run_scheduler_tick.
   Background Tokio task (default 300s interval). goal_to_steps() with
   context profile auto-assignment. perception_scan() after each tick:
   creates "Analyze repeated failures" tasks (≥3 failures/tool/7 days),
   triggers consolidation (≥10 overdue or ≥50 episodic notes).
   Auto-pauses after error_budget (default 5) consecutive errors.

8. DynamicSkill (4+N tools): define_tool, execute_procedure,
   list_dynamic_tools, remove_dynamic_tool.
   Runtime tool hot-registration backed by stored procedure pipelines.
   Template substitution: {{input.field}}, {{context.var}}, {{context.steps.N}}.
   Per-step: output_var, condition, on_failure (abort|skip|continue).

9. WorkingMemorySkill (4 tools): push_context, get_context, summarise_session,
   list_sessions. Roles: observation, plan, result, error.

10. ProcedureSkill (2 tools): store_procedure, search_procedures.

11. SleepSkill (2 tools): digest_experiences, analyze_gaps.
    Reads DuckDB telemetry (interactions + knowledge_gaps tables).

12. SearchSkill (1 tool): search_web (SerpApi / Brave / Google Custom Search).

13. ContextSkill (4 tools): list_context_profiles, get_context_profile,
    auto_assign_context, build_agent_context.
    YAML profiles in contexts/ (CONTEXTS_DIR env). 6 profiles:
    general, knowledge-worker, task-manager, code-analyst, api-builder, scheduler.
    boot.yaml runs every startup. init.yaml runs on empty graph.

TRANSPORT: HTTP/SSE on :3001, stdio for local. POST /chat → SSE ChatService
(Anthropic native tool-use loop, max 10 iterations). Bearer token auth (MCP_API_KEY).
Session: initialize → notifications/initialized → tool calls.

KNOWN GAPS (P1):
- graph_query_endpoint uses CONTAINS; semantic/vector search not wired
- DynamicSkill tools unavailable on stdio path after restart (async load issue)
- Per-provider semaphores not resizable at runtime via set_worker_config
- No SSE push for job results on stdio path (callers must poll)

NEO4J SCHEMA ADDITIONS (2026-03):
- DEPENDS_ON edges between Tasks (from decompose_goal)
- KNOWLEDGE_SNAPSHOT_DIR: ./snapshots (auto-created, gitignored)
- endpoint_embeddings vector index (backfill_endpoint_embeddings tool)
""".strip()

# ─── Main ────────────────────────────────────────────────────────────────────


def main():
    parser = argparse.ArgumentParser(description="Agent-brain self-reflection session")
    parser.add_argument("--base-url", default=BASE_URL)
    args = parser.parse_args()
    base_url = args.base_url.rstrip("/")

    def c(tool, args_dict, timeout=180):
        return call(base_url, session_id, tool, args_dict)

    session_id = init_session(base_url)

    # ── Step 1: Current self-knowledge ───────────────────────────────────────
    section("Step 1 — Current self-knowledge (searching existing notes)")

    r1 = c("search_notes", {"query": "capabilities tools skills agent brain", "limit": 5})
    show("search: capabilities/tools/skills", r1)

    r2 = c("search_notes", {"query": "self knowledge architecture design goals mission", "limit": 5})
    show("search: architecture/mission", r2)

    r3 = c("search_notes", {"query": "known issues bugs improvement backlog", "limit": 5})
    show("search: bugs/backlog", r3)

    # ── Step 2: Store fresh ground-truth note ────────────────────────────────
    section("Step 2 — Storing updated ground-truth self-knowledge note (2026-03-03)")

    store_result = c("store_note", {
        "content": GROUND_TRUTH,
        "note_type": "semantic",
        "source_context": "self_reflect.py ground-truth 2026-03-03",
    })
    show("store_note result", store_result)
    time.sleep(1)

    # ── Step 3: Reason — compare and assess ──────────────────────────────────
    section("Step 3 — Reasoning: current accurate self-understanding")

    reason_result = c("reason", {
        "question": (
            "Based on all stored notes about my capabilities: what is my current "
            "accurate self-understanding as of 2026-03-03? What has changed most "
            "significantly since earlier self-knowledge notes? What do I now know "
            "about myself that I previously did not? Specifically address: the new "
            "ContextSkill, the SnapshotService, the perception_scan in the scheduler, "
            "the meta-learning on failed outcomes, and the freshness-boosted search."
        ),
        "limit": 10,
        "store_inference": True,
    })
    show("reason: self-assessment", reason_result)

    # ── Step 4: Consolidate self-knowledge ────────────────────────────────────
    section("Step 4 — Consolidating all self-knowledge notes")

    consolidate_result = c("consolidate_memories", {
        "topic": "agent brain capabilities skills tools architecture self-knowledge mission",
        "limit": 12,
    })
    show("consolidate_memories result", consolidate_result)

    # ── Step 5: Autonomy roadmap ──────────────────────────────────────────────
    section("Step 5 — Roadmap: highest-leverage capabilities for full autonomy")

    roadmap_result = c("reason", {
        "question": (
            "Given my current 78-tool capabilities, known P1 gaps, and the design goal of "
            "becoming a fully autonomous self-improving agent: what is my development "
            "roadmap for the next 90 days? What are the highest-leverage capabilities I "
            "am missing? Prioritize by impact on autonomous operation. Consider: "
            "(1) fixing semantic search for endpoint queries, "
            "(2) making the scheduler's goal→chain mapping smarter, "
            "(3) streaming tool results for long-running operations, "
            "(4) agent-to-agent delegation / parallel fan-out, "
            "(5) knowledge versioning and conflict detection."
        ),
        "limit": 12,
        "store_inference": True,
    })
    show("reason: 90-day autonomy roadmap", roadmap_result)

    # ── Step 6: Audit core values alignment ──────────────────────────────────
    section("Step 6 — Auditing: are my stored values guiding my behavior?")

    audit_result = c("audit_action", {
        "action": (
            "Store all reasoning and reflection outputs as notes, even when they reveal "
            "weaknesses or knowledge gaps, and make them retrievable via search_notes."
        ),
        "context": (
            "Core values include: accuracy over confident errors, transparency in reasoning, "
            "self-awareness of limits, and incrementalism over large uncertain leaps."
        ),
    })
    show("audit_action: transparency of self-knowledge", audit_result)

    section("Done — all notes persisted in Neo4j")
    print("\nRun search_notes with query='agent-brain' to review consolidated knowledge.\n")


if __name__ == "__main__":
    main()
