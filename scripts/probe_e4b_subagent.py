#!/usr/bin/env python3
"""Probe: can Gemma-4-E4B (llama.cpp sidecar) actually drive the sub-agent tool loop?

Runs `run_agent(agent="knowledge-local", task=...)` over the MCP HTTP endpoint.
knowledge-local is pinned to the `gemma4-e4b` catalog entry (provider `llamacpp`),
so the sub-agent loop resolves to LlmProviderType::LlamaCpp @ LLAMACPP_URL and the
unmetered clamp keeps it. The task is a STRUCTURAL question that can only be
answered by CALLING TOOLS (recent_tasks + list_apis) — so the tool_trace tells us
whether E4B emits usable tool-calls over the OpenAI-compat wire across rounds.

Prerequisites: the brain must be REBUILT with this change set and running
(SUBAGENT_ENABLED=true, HTTP transport — run_agent is HTTP-path only), and the
llamacpp sidecar up. Usage:

    python3 scripts/probe_e4b_subagent.py [base_url]

Reads the result's tool_trace (ground truth of what ran) — a sub-agent's prose can
claim actions it never took; the trace cannot. A PASS = E4B called the deterministic
tools and returned a grounded answer. A FAIL with an empty trace = E4B did not
tool-call (the risk flagged before wiring) → keep knowledge lookups on cloud.
"""
import json
import sys
import urllib.request

BASE_URL = sys.argv[1] if len(sys.argv) > 1 else "http://localhost:3001"
TASK = (
    "Two things, using tools (do not answer from memory): "
    "1) the last 5 tasks that ran — use recent_tasks. "
    "2) what APIs we have in the graph — use list_apis. "
    "Report both concisely with the real values."
)
EXPECT_TOOLS = {"recent_tasks", "list_apis"}


def mcp(method, params, session_id=None):
    headers = {"Content-Type": "application/json"}
    if session_id:
        headers["mcp-session-id"] = session_id
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(f"{BASE_URL}/mcp", data=body, headers=headers, method="POST")
    with urllib.request.urlopen(req, timeout=300) as resp:
        sid = resp.headers.get("mcp-session-id")
        text = resp.read().decode()
    # HTTP transport may answer as SSE ("data: {...}") or plain JSON.
    if text.startswith("data:"):
        text = "\n".join(l[5:].strip() for l in text.splitlines() if l.startswith("data:"))
    try:
        return json.loads(text), sid
    except json.JSONDecodeError:
        return {"_raw": text}, sid


def main():
    print(f"→ MCP endpoint: {BASE_URL}/mcp")
    _, sid = mcp("initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "e4b-probe", "version": "1"},
    })
    if not sid:
        print("ERROR: no session id — is the brain running on HTTP?")
        sys.exit(1)
    mcp("notifications/initialized", {}, sid)
    print(f"→ session {sid}; calling run_agent(knowledge-local)…\n")

    body, _ = mcp("tools/call", {
        "name": "run_agent",
        "arguments": {"agent": "knowledge-local", "task": TASK},
    }, sid)

    result = body.get("result", body)
    content = result.get("content") if isinstance(result, dict) else None
    if not content:
        print("UNEXPECTED RESULT:", json.dumps(body, indent=2)[:2000])
        sys.exit(2)
    text = content[0].get("text", "")
    if result.get("isError"):
        print("run_agent returned an ERROR (config/infra — e.g. agent not spawnable):\n", text)
        sys.exit(2)

    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        print("run_agent answer (non-JSON):\n", text)
        sys.exit(2)

    trace = payload.get("tool_trace", [])
    ran = {t["tool"] for t in trace}
    print("=" * 70)
    print("ANSWER:\n", payload.get("answer", "(none)"))
    print("-" * 70)
    print("ok:", payload.get("ok"), "| retrieved:", payload.get("retrieved"))
    print("tool_trace:", json.dumps(trace))
    print("=" * 70)

    if not trace:
        print("\n❌ FAIL — E4B ran NO tools. It did not tool-call over the loop.")
        print("   Keep knowledge lookups on cloud; E4B can't drive the sub-agent loop.")
        sys.exit(1)
    missing = EXPECT_TOOLS - ran
    if missing:
        print(f"\n⚠ PARTIAL — E4B tool-called but missed {missing}. Ran: {ran}.")
        print("   It drives the loop but picked the wrong/incomplete tools.")
        sys.exit(1)
    print(f"\n✅ PASS — E4B called the deterministic tools ({ran}) and answered.")
    print("   Safe to consider pinning knowledge-worker to gemma4-e4b.")


if __name__ == "__main__":
    main()
