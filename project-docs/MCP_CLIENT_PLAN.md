# Outbound MCP Client — the brain as a client of other MCP servers

Status: **build in progress (2026-09-09).** Lets the brain call tools on ANY
external MCP server — job boards (aidevboard, worklittle) first, but the
subsystem is general: register a server, list its tools, call them, from chat or
a chain/schedule. Supersedes Track 2's "MCP client" note in
`VALIDATION_AND_APPLY_PLAN.md` (that track's apply *workflow* still applies; this
is the transport it rides on).

## Why general, not aidevboard-specific

Two job MCP servers already exist and differ in transport/auth, which is the
whole argument for generality:
- **aidevboard** — `https://aidevboard.com/mcp`, auth `X-API-Key: $AIDEVEBOARD_KEY`
  (email-verified free key). Full search→analyze→compile→approve→submit apply flow.
- **worklittle** — `https://mcp.worklittle.com/` (from `worklittle.com/llms.txt`),
  keyless for `search_jobs` and friends; API key only for employer/write tools.

A general client means adding the next server (another job board, a docs server,
a data API that ships MCP) is a YAML file, not a code change.

## Model — mirror the ApiContext pattern

- **`(:McpServer)` node**, seeded ON CREATE from `mcp-servers/*.yaml`
  (`MCP_SERVERS_DIR`, default `./mcp-servers`), graph-owned after first creation
  (like SourceLists) so runtime edits persist; missing dir non-fatal. Fields:
  `name` (unique), `url`, `description`, `enabled`, and the same auth triple
  `http_request` uses — `auth_scheme` (`none`|`bearer`|`header`), `auth_param`
  (header name for `header`), `auth_env_var` (the env var NAME holding the
  secret; the value is injected at call time and NEVER stored in graph or YAML).
- Seeds: `mcp-servers/aidevboard.yaml`, `mcp-servers/worklittle.yaml`.

## Transport — `services/mcp_client.rs`

`McpClient` speaks MCP JSON-RPC over HTTP, in the brain process (has egress),
never the sandbox. Best-effort handshake: `initialize` → capture any
`Mcp-Session-Id` → `notifications/initialized` → the requested op, threading the
session id. Handshake failure is non-fatal (both known servers answer
`tools/list` without it). Handles BOTH plain-JSON and SSE-framed
(`event:`/`data:`) response bodies (`extract_json_body`, unit-tested). Auth
injected from the env var named on the config. Methods: `list_tools`, `call_tool`.

## Tools (passthrough) — `skills/mcp_client.rs`

Minimal, general, works in chat AND chains/schedules immediately:
- `mcp_list_servers` — registered `:McpServer` nodes (name, url, enabled, desc).
- `mcp_list_tools {server}` — remote `tools/list` (names + descriptions + schemas).
- `mcp_call {server, tool, arguments}` — remote `tools/call`; the text content is
  returned under `answer` so `{{_prev}}` flows in a chain. `server` resolves to a
  `:McpServer` by name, or is used directly if it is a bare `https://` URL
  (keyless). A remote `isError` returns a *successful* tool call carrying the
  error text (like `execute_code`), so it never burns a chain retry.

Passthrough (one generic call tool) rather than federating every remote tool as
a native brain tool: minimal, robust to a server being down, and the registry is
built once at startup. Federation (auto-registering `server__tool`) is a later
option if it proves worth it.

## Human-in-the-loop safety

The dangerous remote actions (aidevboard `apply_to_job`) are gated **server-side**
— they require a human-approved artifact hash + `principal_authorized`, which the
brain cannot fabricate — so a generic `mcp_call` passthrough cannot submit an
application on its own. The behavioural rule ("never claim an application was
submitted unless the tool returned success") lives in `contexts/general.yaml`.

## Build phases
- **P1 (this pass):** node model + seeder + `mcp-servers/{aidevboard,worklittle}.yaml`;
  `McpClient` transport (+ parser unit tests); `McpClientSkill` 3 tools; wire into
  `brain_core` + compose `MCP_SERVERS_DIR`. Verifiable live after a rebuild.
- **P2 — built (2026-09-09, uncommitted; ACTIVATES ON REBUILD).**
  - Enabler: `mcp_call` now also returns `structured_json` (the structured payload
    as a STRING) so a chain can pass a remote tool's structured output cleanly via
    `{{_prev.structured_json}}` into an execute_code/push_context arg.
  - Reply-to-apply: `contexts/general.yaml` JOB APPLICATION RULE rewritten around
    the real MCP toolchain — search_notes(résumé) → `analyze_application_readiness`
    → `compile_job_specific_resume` → **STOP, show artifact verbatim, ask approval,
    create_task** → only on explicit human approval `approve_compiled_application`
    + `apply_to_job`. `principal_authorized` never set by the model on its own;
    never claim submitted unless `apply_to_job` returned success. `mcp_list_servers`/
    `mcp_list_tools`/`mcp_call` added to the general allowlist.
  - Daily two-board search: `schedules/ai-dev-board-job-search.yaml` — 9 steps,
    aidevboard `match_jobs` + worklittle `search_jobs` banked via push_context,
    reassembled with neo4j_query, merged/deduped/ranked by execute_code (links
    verbatim; JSON-preferred, raw-text fallback so a board is never dropped), into
    the fresh `jobsearch-<date>` chat. `enabled:false`, `managed_by:yaml`, and
    **SHADOWED** by the live runtime node until ownership is transferred.
  - **Everything P2 references `mcp_call`, unregistered on the running brain**, so
    all of it activates only after a rebuild. Post-rebuild TODO, in order:
    (1) smoke-test `mcp_call aidevboard match_jobs` + `worklittle search_jobs` to
    capture their real structuredContent shapes; (2) confirm/adjust the
    execute_code merge parser against worklittle's actual JSON; (3) transfer the
    live schedule to this YAML (delete the runtime node or SET managed_by='yaml')
    and enable; (4) dry-run reply-to-apply to a role and confirm it STOPS at compile.
- **P3 (maybe):** tool federation; per-server rate/quota tracking; a discovery
  helper that reads a server's `llms.txt`/`mcp.json`.
