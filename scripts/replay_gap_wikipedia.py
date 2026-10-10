"""HBI plan Phase 21 replay (kept so a v2 can be measured the same way): 15 past gaps, the production gap chain (A) vs the same
chain with wiki_background first (B). Both arms get the same ALREADY KNOWN
section (search_notes with earlier gap syntheses removed, which would
otherwise hand back the old answer), then run through the real queue with the
chain's own steps, providers and distillation. Nothing is stored.

usage: python3 -I scripts/replay_gap_wikipedia.py DIR enqueue|collect
DIR must hold gaps15.json (the frozen sample is eval/gap_wikipedia_replay.json;
copy it there); the state and results are written to DIR.
"""
import json, subprocess, sys, time, urllib.request, copy, os
import yaml

S = sys.argv[1]
MODE = sys.argv[2]
REPO = "/home/ara/agent-brain"
BASE = "http://localhost:3001/mcp"


def env(name):
    return subprocess.run(["docker", "compose", "exec", "-T", "agent-brain", "printenv", name],
                          cwd=REPO, capture_output=True, text=True).stdout.strip()


KEY = env("MCP_API_KEY")
PW, USER = env("NEO4J_PASSWORD"), env("NEO4J_USER") or "neo4j"
HDR = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
if KEY:
    HDR["Authorization"] = f"Bearer {KEY}"


def post(body, sid=None):
    h = dict(HDR)
    if sid:
        h["Mcp-Session-Id"] = sid
    req = urllib.request.Request(BASE, data=json.dumps(body).encode(), headers=h)
    with urllib.request.urlopen(req, timeout=600) as r:
        return r.headers.get("mcp-session-id"), r.read().decode()


def session():
    sid, _ = post({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "replay21", "version": "1"}}})
    post({"jsonrpc": "2.0", "method": "notifications/initialized"}, sid)
    return sid


def call(sid, name, args):
    _, raw = post({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": name, "arguments": args}}, sid)
    if raw.startswith(("event", "data")):
        raw = raw.split("data:", 1)[1]
    r = json.loads(raw)["result"]
    return r["content"][0]["text"], r.get("isError", False)


def cypher(q):
    out = subprocess.run(["docker", "compose", "exec", "-T", "neo4j", "cypher-shell", "-u", USER, "-p", PW,
                          "--format", "plain", q], cwd=REPO, capture_output=True, text=True)
    return out.stdout


chain = yaml.safe_load(open(f"{REPO}/chains/fill-knowledge-gap.yaml"))["steps"]
by_desc = {s.get("description"): s for s in chain}
SEARCH = by_desc["Gap fill: search the web for external evidence"]
BANK_WEB = by_desc["Gap fill: bank web results"]
REASSEMBLE = by_desc["Gap fill: reassemble known + web evidence"]
REASON = by_desc["Gap fill: answer the gap from known + web evidence"]
BANK_KNOWN = by_desc["Gap fill: bank existing knowledge"]

B_INTRO_OLD = 'The CONTEXT contains two sections: "## ALREADY KNOWN" (notes the brain\nhas stored) and "## WEB SEARCH RESULTS" (external evidence gathered for\nthis gap).'
B_INTRO_NEW = ('The CONTEXT contains three sections: "## ALREADY KNOWN" (notes the brain\nhas stored), '
               '"## BACKGROUND — Wikipedia" (encyclopedic background, may be out\nof date) and '
               '"## WEB SEARCH RESULTS" (external evidence gathered for\nthis gap).')


def subst(step, goal, sid):
    s = json.loads(json.dumps(step).replace("{{goal_topic}}", goal.replace('"', '\\"'))
                   .replace("gap-{{task_id}}", sid))
    return s


def steps_for(arm, goal, known, sid):
    out = []
    bk = subst(BANK_KNOWN, goal, sid)
    bk["arguments"]["content"] = "## ALREADY KNOWN — existing notes from the graph\n" + known
    out.append(bk)
    search = subst(SEARCH, goal, sid)
    if arm == "B":
        out.append({"tool_name": "wiki_background",
                    "arguments": {"question": goal, "session_id": sid},
                    "priority": 1, "max_attempts": 2, "provider_hint": "ollama",
                    "description": "Gap fill: Wikipedia background"})
        search["arguments"]["query"] = "{{_prev.query}}"
    out += [search, subst(BANK_WEB, goal, sid), subst(REASSEMBLE, goal, sid)]
    reason = subst(REASON, goal, sid)
    if arm == "B":
        q = reason["arguments"]["question"]
        assert B_INTRO_OLD in q, "chain reason prompt changed"
        reason["arguments"]["question"] = q.replace(B_INTRO_OLD, B_INTRO_NEW)
    out.append(reason)
    return out


gaps = json.load(open(f"{S}/gaps15.json"))
state_p = f"{S}/replay21_state.json"

if MODE == "enqueue":
    sid = session()
    state = json.load(open(state_p)) if os.path.exists(state_p) else {}
    for i, g in enumerate(gaps):
        if str(i) in state:
            continue
        raw, err = call(sid, "search_notes", {"query": g["goal"], "limit": 10})
        try:
            v = json.loads(raw)
            notes = v.get("results") or v.get("notes") or []
            kept = [n for n in notes if "## Gap synthesis" not in str(n.get("content", ""))
                    and n.get("source_context") != "fill_knowledge_gap"]
            v_key = "results" if "results" in v else "notes"
            v[v_key] = kept
            dropped = len(notes) - len(kept)
            known = json.dumps(v, ensure_ascii=False)
        except Exception:
            known, dropped = raw, 0
        entry = {"goal": g["goal"], "dropped_syntheses": dropped}
        for arm in ("A", "B"):
            wm = f"replay21-{arm}-{i}"
            ids_raw, err = call(sid, "enqueue_jobs", {"steps": steps_for(arm, g["goal"], known, wm)})
            entry[arm] = {"session": wm, "enqueue": json.loads(ids_raw) if not err else ids_raw}
        state[str(i)] = entry
        json.dump(state, open(state_p, "w"), indent=1)
        print(i, "enqueued; dropped syntheses:", dropped, flush=True)

elif MODE == "collect":
    state = json.load(open(state_p))
    done = 0
    for i, e in sorted(state.items(), key=lambda x: int(x[0])):
        for arm in ("A", "B"):
            ids = e[arm]["enqueue"]["ids"]
            idl = json.dumps(ids)
            out = cypher(f"UNWIND range(0, size({idl})-1) AS k MATCH (j:AgentJob {{id: {idl}[k]}}) "
                         f"RETURN k, j.tool_name, j.status, left(coalesce(j.result, j.error, ''), 20000) AS r ORDER BY k")
            e[arm]["jobs"] = out
        json.dump(state, open(state_p, "w"), indent=1)
    print("collected")
