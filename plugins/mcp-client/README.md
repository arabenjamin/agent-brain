# mcp-client

Call tools on external MCP servers: mcp_list_tools lists the registered servers or one server's tools, and mcp_call runs one. Servers are :McpServer nodes seeded from mcp-servers/*.yaml (Exa, Tavily); auth secrets stay in env vars.

Moved out of core on 2026-11-06 (CODEBASE_SPLIT_PLAN Phase 3). Rules:
`scripts/dev_docs.sh get subsystem-rules/architecture-subsystem-rules-outbound-mcp-client`.

Stamped from `templates/plugin`; `.cruft.json` records the template commit.
`scripts/plugin_template.sh update mcp-client` brings a later
template change in as a diff to review. This README is the plugin's own: an
update never touches it.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description, what it requires |
| `crate/src/lib.rs` | the `Plugin` impl (`McpClient`) |
| `crate/src/skill.rs` | the tools: mcp_list_tools, mcp_call; resolving a server from its `:McpServer` node (or a bare keyless URL), and checking a call's arguments against the remote tool's schema with the host's `ArgCheck` |
| `crate/src/client.rs` | JSON-RPC over HTTP (plain or SSE-framed), the session handshake, a server's key from its env var at call time |
| `mcp-servers/*.yaml` | server seeds the host writes as `:McpServer` on first load (graph-owned after) |
| `schemas/tools/<tool>.json` | each tool's input schema, loaded by `tool_schema!` |

Any loaded bundle may ship `mcp-servers/`: the private `personal` bundle seeds
aidevboard and worklittle. home_assistant is in the graph only, with no seed
file (`dev_docs get mcp-client/home-assistant-mcp-server-is-graph-only-no-seed-file`).

## Enable and check

On by default (`PLUGINS_ENABLED` in `docker-compose.yml`). Then
`cargo test -p agent-brain-plugin-mcp-client --lib`, `scripts/check_schemas.sh`
and `scripts/check_plugin_deps.sh`, and rebuild the image.
