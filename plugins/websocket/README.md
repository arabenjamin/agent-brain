# websocket

WebSocket client (ws_connect, ws_send, ws_receive, ws_close): hold a live connection to a ws:// or wss:// endpoint across tool calls, for streaming feeds and push APIs that http_request cannot read.

Built before the template and linked to it on 2026-11-06; `.cruft.json`
records the template commit.
`scripts/plugin_template.sh update websocket` brings a later
template change in as a diff to review.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description, what it requires |
| `crate/src/lib.rs` | the `Plugin` impl (`Websocket`) |
| `crate/src/skill.rs` | the tools: ws_close, ws_connect, ws_receive, ws_send; the open connections, the net-policy check before a connect, and the size limits |
| `schemas/tools/<tool>.json` | each tool's input schema, loaded by `tool_schema!`; `ws_receive`'s is built in Rust (its description carries the default timeout) |

Seeds go in the directories the brain reads (`workflows/`, `contexts/`,
`views/`, …); `scripts/check_schemas.sh` lists the kinds it knows. Graph
access goes in `crate/src/store.rs`.

## Enable and check

On by default (`PLUGINS_ENABLED` in `docker-compose.yml`). Then
`cargo test -p agent-brain-plugin-websocket --lib`, `scripts/check_schemas.sh`
and `scripts/check_plugin_deps.sh`, and rebuild the image.
