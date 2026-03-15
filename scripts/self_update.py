#!/usr/bin/env python3
"""
self_update.py — Refresh agent-brain's self-knowledge after a code change.

Called by the post-commit git hook. Runs analyze_own_structure, then stores
a note summarising what changed in the commit so the brain stays up-to-date.

Usage:
    python3 scripts/self_update.py [--base-url http://localhost:3001]
"""

import json
import subprocess
import sys
import argparse
import urllib.request
import urllib.error

# ---------------------------------------------------------------------------
# MCP helpers (minimal — mirrors self_learn.py)
# ---------------------------------------------------------------------------

def mcp_request(method, params, session_id=None, base_url="http://localhost:3001", timeout=30):
    headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
    if session_id:
        headers["mcp-session-id"] = session_id
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(f"{base_url}/mcp", data=body, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            new_session = resp.headers.get("mcp-session-id")
            raw = resp.read().decode()
            # SSE envelope — strip "data: " prefix if present
            for line in raw.splitlines():
                if line.startswith("data:"):
                    raw = line[5:].strip()
                    break
            return json.loads(raw) if raw else {}, new_session
    except urllib.error.URLError as e:
        return None, None

def tool_call(name, arguments, session_id, base_url="http://localhost:3001"):
    result, _ = mcp_request("tools/call", {"name": name, "arguments": arguments},
                            session_id=session_id, base_url=base_url)
    return result

# ---------------------------------------------------------------------------
# Git helpers
# ---------------------------------------------------------------------------

def git(*args):
    try:
        return subprocess.check_output(["git"] + list(args), text=True).strip()
    except subprocess.CalledProcessError:
        return ""

def get_commit_summary():
    sha   = git("rev-parse", "--short", "HEAD")
    msg   = git("log", "-1", "--pretty=%s")
    files = git("diff-tree", "--no-commit-id", "-r", "--name-only", "HEAD")
    return sha, msg, files

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", default="http://localhost:3001")
    args = parser.parse_args()
    base_url = args.base_url

    sha, msg, files = get_commit_summary()
    print(f"[self_update] commit {sha}: {msg}")

    # Initialize session
    resp, session_id = mcp_request("initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "self_update", "version": "1.0"},
    }, base_url=base_url, timeout=10)

    if resp is None:
        print("[self_update] Brain not reachable — skipping self-knowledge refresh")
        sys.exit(0)

    mcp_request("notifications/initialized", {}, session_id=session_id,
                base_url=base_url, timeout=10)
    print(f"[self_update] session {session_id}")

    # Refresh tool/structure count
    structure = tool_call("analyze_own_structure", {"store_as_note": True},
                          session_id, base_url)
    total_tools = 0
    if structure and "result" in structure:
        try:
            data = json.loads(structure["result"][0]["text"])
            total_tools = data.get("total_tools", 0)
        except (KeyError, IndexError, json.JSONDecodeError):
            
            pass
    print(f"[self_update] tools at runtime: {total_tools}")

    # Store a note about the commit
    changed = files[:800] if files else "(no src changes)"
    note = (
        f"Code change committed: {sha}\n"
        f"Message: {msg}\n"
        f"Changed files:\n{changed}\n\n"
        f"Runtime tool count: {total_tools}"
    )
    tool_call("store_note", {
        "content": note,
        "note_type": "episodic",
        "tags": ["code-change", "self-knowledge"],
        "source_context": f"git-hook post-commit {sha}",
    }, session_id, base_url)

    print("[self_update] self-knowledge refreshed")

if __name__ == "__main__":
    main()
