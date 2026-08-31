#!/usr/bin/env bash
# pause_cloud_schedules.sh — take the cloud-dependent schedules out of rotation,
# and put them back.
#
# WHY THIS EXISTS
#
# When Ollama Cloud is unreachable — quota exhausted, 403, outage — a step that
# asked for a cloud model does not fail. `SharedLlm::generate` classifies the
# error as "the provider could not answer" and retries on local `gemma4:latest`,
# which is the right default for a chain that would otherwise die. It is the
# wrong default for a report: the weaker model answers, the chain succeeds, and
# a thinner analysis is stored as `source_record` or `semantic` and read back
# later as if nothing had happened. Nothing errors, so nothing tells you.
#
# The schedules touched here are exactly those whose stored steps carry a cloud
# step, plus the three that spawn Tasks routing to a cloud step in `chains/`.
# Everything else already ran entirely on the local model and is unaffected by
# an outage, so pausing it would cost availability and buy nothing.
#
# READING A STEP'S ROUTING — the rule is narrower than it looks. In queue.rs:
#
#     let use_local = job.provider_hint.as_deref() == Some("ollama");
#
# so a step is local ONLY when `provider_hint` is exactly `"ollama"`. An
# OMITTED hint is `None`, which is not local — it falls through to the active
# config, and that is a cloud model (`OLLAMA_MODEL=gemma4:31b-cloud`). Auditing
# with "cloud if provider_hint is present and != ollama" therefore reads every
# chat-authored schedule as local, because chat-authored steps rarely set the
# field at all. That mistake was made here on 2026-08-26 and missed seven
# schedules on the first pass.
#
# Safe to run against a live brain:
#   - Touches `enabled`, `paused_reason`, `paused_at` and nothing else. Steps,
#     description, interval, and next_run_at are left exactly as they are.
#   - Survives a restart. `sync_yaml_scheduled_task` force-syncs steps,
#     description, and interval from `schedules/*.yaml`, but never `enabled` —
#     so a yaml-owned schedule stays paused across a rebuild.
#   - `resume` only re-enables what THIS script paused (it matches on
#     `paused_reason`), so a schedule disabled for some other reason — a broken
#     definition, a superseded experiment — is not swept back on by accident.
#
# Usage:
#   scripts/pause_cloud_schedules.sh pause  "ollama-cloud quota exhausted; resume 2026-08-30"
#   scripts/pause_cloud_schedules.sh status
#   scripts/pause_cloud_schedules.sh resume

set -euo pipefail

BRAIN_URL="${BRAIN_URL:-http://localhost:3001}"
SID="${BRAIN_SID:-$(uuidgen 2>/dev/null || echo pause-cloud-schedules)}"
MARKER="cloud-paused:"

# Matched on name prefix. Derive this list from the GRAPH, not from schedules/ —
# runtime-owned schedules have no file, and they are the ones most likely to be
# cloud-routed by omission. `status` prints the current membership; to re-derive:
#
#   MATCH (s:ScheduledTask) WHERE s.enabled RETURN s.name, s.steps
#   -- then flag any step whose provider_hint is not exactly "ollama"
#
TARGETS=(
  # --- own a cloud step ---
  "Daily news aggregation"              # schedules/daily-news.yaml
  "Off-Grid Networking Monitor"         # schedules/off-grid-networking-monitor.yaml
  "Bi-weekly SLM benchmark watch"       # schedules/slm-benchmark-watch.yaml
  "Monthly tech dependency synthesis"   # schedules/tech-dependency-synthesis.yaml
  "Weekly hardware tripwire"            # schedules/hardware-tripwire.yaml
  # --- spawn Tasks that route to a cloud step ---
  "Daily news analysis"                 # -> 'fill knowledge gap:'  chains/fill-knowledge-gap.yaml
  "Media watch"                         # -> 'watch video:'         chains/video-learning.yaml
  "Brain exercise"                      # -> research goals         chains/learn.yaml
)

cypher() {
  python3 - "$BRAIN_URL" "$SID" "$1" <<'PY'
import json, sys, urllib.request
url, sid, cypher = sys.argv[1], sys.argv[2], sys.argv[3]
body = json.dumps({
    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
    "params": {"name": "neo4j_query", "arguments": {
        "cypher": cypher, "readonly": cypher.lstrip().upper().startswith("MATCH")
                  and " SET " not in cypher.upper(), "limit": 50}},
}).encode()
req = urllib.request.Request(url + "/mcp", body,
                             {"Content-Type": "application/json", "mcp-session-id": sid})
try:
    r = json.load(urllib.request.urlopen(req, timeout=30))
except Exception as e:
    sys.exit(f"brain unreachable at {url}: {e}")
if "result" not in r:
    sys.exit(f"tool call failed: {json.dumps(r)[:400]}")
text = r["result"]["content"][0]["text"]
if r["result"].get("isError"):
    sys.exit(f"query failed: {text[:400]}")
rows = json.loads(text).get("rows", [])
for row in rows:
    state = "OFF" if row.get("en") is False else "ON "
    why = row.get("why") or ""
    print(f"  {state}  {row.get('name','')[:64]:<64} {why}")
print(f"  ({len(rows)} matched)")
PY
}

# Build a WHERE clause from TARGETS.
where=""
for t in "${TARGETS[@]}"; do
  [ -n "$where" ] && where="$where OR "
  where="$where s.name STARTS WITH \"$t\""
done

case "${1:-status}" in
  pause)
    reason="${2:-cloud provider unavailable}"
    echo "Pausing cloud-dependent schedules — $reason"
    cypher "MATCH (s:ScheduledTask) WHERE ($where) AND s.enabled
            SET s.enabled = false,
                s.paused_reason = \"$MARKER $reason\",
                s.paused_at = datetime()
            RETURN s.name AS name, s.enabled AS en, s.paused_reason AS why"
    ;;
  resume)
    echo "Resuming schedules paused by this script"
    cypher "MATCH (s:ScheduledTask)
            WHERE s.enabled = false AND s.paused_reason STARTS WITH \"$MARKER\"
            SET s.enabled = true, s.paused_reason = null, s.paused_at = null
            RETURN s.name AS name, s.enabled AS en"
    ;;
  status)
    echo "Cloud-dependent schedules:"
    cypher "MATCH (s:ScheduledTask) WHERE $where
            RETURN s.name AS name, s.enabled AS en, s.paused_reason AS why
            ORDER BY s.name"
    ;;
  *)
    echo "usage: $0 {pause [reason]|resume|status}" >&2
    exit 2
    ;;
esac
