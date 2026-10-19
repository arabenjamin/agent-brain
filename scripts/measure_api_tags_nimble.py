#!/usr/bin/env python3
"""API_USE_PLAN Phase 0: measure nimble's API tags against the hand tags.

For each API in eval/api_tags_golden.yaml (registered ones from the graph,
the to-register ones from a short description written here), nimble is asked:
  - kind:   one `choice` over the nine kinds, in the file's order;
  - topics: one `noul` (0..1 yes) per topic of the (:Topic) list.
A kind is nimble's when its probability is >= 0.7, else "can't tell".
A topic is yes at >= 0.7, no at <= 0.3, else "can't tell". "Can't tell"
counts as untagged, never as wrong (plan §3 Phase 0).

Bodies are written with insertion-ordered dicts: criteria order is part of
the prompt (the alias judge's lesson, HBI §11.10).

Usage: python3 scripts/measure_api_tags_nimble.py [--out results.json]
Needs the brain on BRAIN_URL (default http://localhost:3001) and host Ollama
with `nimble` on OLLAMA_URL (default http://localhost:11434).
"""
import argparse
import json
import os
import sys
import urllib.request

import yaml

BRAIN = os.environ.get("BRAIN_URL", "http://localhost:3001")
OLLAMA = os.environ.get("OLLAMA_URL", "http://localhost:11434")
MODEL = os.environ.get("NIMBLE_MODEL", "nimble")
SURE = 0.7
NO = 0.3
MAX_ENDPOINTS = 15

# Not registered yet (Phase 2); described as api-integrator would, from the docs.
UNREGISTERED = {
    "cisa_kev": "CISA Known Exploited Vulnerabilities (KEV) catalog: JSON feed of vulnerabilities "
    "CISA has evidence are exploited in the wild, each with CVE id, vendor, product, date added, "
    "required action and due date.",
    "weather_gov": "US National Weather Service API (api.weather.gov): forecasts, hourly forecasts, "
    "observations and alerts for a gridpoint or station.",
}


def post(url, body, headers=None):
    req = urllib.request.Request(url, data=body.encode(), headers={"Content-Type": "application/json", **(headers or {})})
    with urllib.request.urlopen(req, timeout=300) as r:
        return r.read().decode(), dict(r.headers)


def cypher(q):
    init = json.dumps({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
        "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "measure", "version": "0"}}})
    _, h = post(f"{BRAIN}/mcp", init, {"Accept": "application/json, text/event-stream"})
    sid = {k.lower(): v for k, v in h.items()}["mcp-session-id"]
    post(f"{BRAIN}/mcp", json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}), {"mcp-session-id": sid})
    call = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                       "params": {"name": "neo4j_query", "arguments": {"cypher": q}}})
    text, _ = post(f"{BRAIN}/mcp", call, {"Accept": "application/json, text/event-stream", "mcp-session-id": sid})
    for line in text.splitlines():
        line = line.removeprefix("data:").strip()
        if line.startswith("{"):
            return json.loads(json.loads(line)["result"]["content"][0]["text"])["rows"]
    raise RuntimeError(text[:500])


def ask(state, question):
    body = json.dumps({"model": MODEL, "keep_alive": "5m", "state": state, "questions": {"q": question}})
    text, _ = post(f"{OLLAMA}/v1/systemone", body)
    return json.loads(text)["answers"]["q"]


def api_text(name, desc, endpoints):
    lines = [f"API: {name}", desc.strip()]
    if endpoints:
        lines.append("Endpoints:")
        lines += [f"- {e}" for e in endpoints[:MAX_ENDPOINTS]]
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--golden", default="eval/api_tags_golden.yaml")
    ap.add_argument("--out")
    args = ap.parse_args()
    gold = yaml.safe_load(open(args.golden))
    kinds = gold["kinds"]  # ordered as in the file
    topics = cypher("MATCH (t:Topic) RETURN t.name AS name, t.label AS label, t.description AS d ORDER BY name")
    regs = {r["name"]: r for r in cypher(
        "MATCH (a:ApiContext) OPTIONAL MATCH (a)-[:EXPOSES]->(e:ApiEndpoint) "
        "WITH a, e ORDER BY e.path RETURN a.name AS name, a.description AS d, "
        "[x IN collect(e) WHERE x IS NOT NULL | x.method + ' ' + x.path + ': ' + coalesce(x.summary, '')] AS eps")}

    kind_q = {"type": "choice",
              "instructions": "What kind of data does this API provide? Pick the one kind that fits it best.",
              "criteria": dict(kinds)}
    results = []
    for a in gold["apis"]:
        if a.get("match") is False:
            continue
        name = a["name"]
        if name in regs:
            text = api_text(name, regs[name]["d"] or "", regs[name]["eps"])
        else:
            text = api_text(name, UNREGISTERED[name], [])
        state = {"api": text}
        k = ask(state, kind_q)
        row = {"api": name, "registered": name in regs, "gold_kind": a["kind"], "gold_topics": a["topics"],
               "kind": k["choice"], "p_kind": k["probabilities"][k["choice"]], "topics": {}}
        for t in topics:
            q = {"type": "noul", "instructions":
                 f"Is this API's data mainly about this topic, so that a workflow watching the topic would read it?\n"
                 f"Topic: {t['label']}. {t['d']}"}
            row["topics"][t["name"]] = ask(state, q)["noul"]
        results.append(row)
        print(f"{name:26} kind={row['kind']:18} p={row['p_kind']:.2f}  gold={a['kind']}", file=sys.stderr)

    # Score.
    k_right = k_wrong = k_unsure = 0
    t_right = t_wrong = t_unsure = 0
    wrong = []
    for r in results:
        if r["p_kind"] < SURE:
            k_unsure += 1
        elif r["kind"] == r["gold_kind"]:
            k_right += 1
        else:
            k_wrong += 1
            wrong.append(f"kind {r['api']}: {r['kind']} ({r['p_kind']:.2f}), hand {r['gold_kind']}")
        for t, p in r["topics"].items():
            truth = t in r["gold_topics"]
            if NO < p < SURE:
                t_unsure += 1
            elif (p >= SURE) == truth:
                t_right += 1
            else:
                t_wrong += 1
                wrong.append(f"topic {r['api']} / {t}: {p:.2f}, hand {'yes' if truth else 'no'}")
    summary = {"apis": len(results),
               "kind": {"right": k_right, "wrong": k_wrong, "cant_tell": k_unsure},
               "topics": {"right": t_right, "wrong": t_wrong, "cant_tell": t_unsure},
               "wrong": wrong}
    print(json.dumps(summary, indent=1))
    if args.out:
        json.dump({"summary": summary, "results": results}, open(args.out, "w"), indent=1)


if __name__ == "__main__":
    main()
