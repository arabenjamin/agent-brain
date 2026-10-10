#!/usr/bin/env bash
# Query the brain's developer notes (`dev_docs` tool, services/dev_notes.rs)
# from a shell — for coding agents whose MCP client only lists the front-door
# profile's tools. Needs the brain up; if it is down, grep devdocs/devnotes.jsonl.
#
#   scripts/dev_docs.sh search "why do schedule steps need provider_hint" [subsystem] [path]
#   scripts/dev_docs.sh get engineering-history/web-search
#   scripts/dev_docs.sh write rule|history|decision|plan "<subsystem>" "<title>" "<content>" [path,...] [id]
#   (content "-" reads stdin; an existing id replaces that note)
#
# BRAIN_URL (default http://localhost:3001), MCP_API_KEY if the brain requires one.
set -euo pipefail
URL="${BRAIN_URL:-http://localhost:3001}/mcp"
H=(-H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream')
[ -n "${MCP_API_KEY:-}" ] && H+=(-H "Authorization: Bearer $MCP_API_KEY")

action="${1:-}"
case "$action" in
  search) args=$(jq -n --arg q "${2:?query}" --arg s "${3:-}" --arg p "${4:-}" \
            '{action:"search",query:$q} + (if $s!="" then {subsystem:$s} else {} end)
                                     + (if $p!="" then {path:$p} else {} end)') ;;
  get)    args=$(jq -n --arg id "${2:?id}" '{action:"get",id:$id}') ;;
  write)  c="${5:?content}"; [ "$c" = "-" ] && c=$(cat)
          args=$(jq -n --arg k "${2:?kind}" --arg s "${3:?subsystem}" --arg t "${4:?title}" \
            --arg c "$c" --arg p "${6:-}" --arg id "${7:-}" \
            '{action:"write",kind:$k,subsystem:$s,title:$t,content:$c}
             + (if $p!="" then {paths:($p|split(","))} else {} end)
             + (if $id!="" then {id:$id} else {} end)') ;;
  *) sed -n '2,10p' "$0"; exit 2 ;;
esac

sid=$(curl -s -D - -o /dev/null -X POST "$URL" "${H[@]}" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"dev_docs.sh","version":"1"}}}' \
  | awk -F': ' 'tolower($1)=="mcp-session-id"{print $2}' | tr -d '\r')
[ -n "$sid" ] || { echo "brain not reachable at $URL — grep devdocs/devnotes.jsonl instead" >&2; exit 1; }
curl -s -o /dev/null -X POST "$URL" "${H[@]}" -H "mcp-session-id: $sid" \
  -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'
out=$(curl -s -X POST "$URL" "${H[@]}" -H "mcp-session-id: $sid" \
  -d "$(jq -n --argjson a "$args" '{jsonrpc:"2.0",id:2,method:"tools/call",params:{name:"dev_docs",arguments:$a}}')" \
  | jq -r '.result.content[0].text // .error.message // .')
echo "$out" | jq . 2>/dev/null || echo "$out"
