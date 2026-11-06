# apis

Register and understand outside APIs: ingest_api reads an OpenAPI spec, a docs page or a source tree into an ApiContext and its endpoints; define_api_context registers one by hand and probes its GET endpoints; list_apis and list_endpoints answer what is registered; the api-integrator agent. http_request, in core, calls them.

Moved out of core on 2026-11-06 (CODEBASE_SPLIT_PLAN Phase 3). The core keeps
`http_request`, which calls an API through its stored `(:ApiContext)`, the
`apis/*.yaml` seeds, and the `api-retire-check` workflow, which reads the graph
only. Off, the brain still calls every registered API; it can't register or
list them.

Stamped from `templates/plugin`; `.cruft.json` records the template commit.
`scripts/plugin_template.sh update apis` brings a later
template change in as a diff to review. This README is the plugin's own: an
update never touches it.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description, what it requires |
| `crate/src/lib.rs` | the `Plugin` impl (`Apis`) |
| `crate/src/skill.rs` | the skill and its dispatch; `list_apis`, `list_endpoints` |
| `crate/src/ingest.rs` | `ingest_api`: spec parsing, finding a docs page's linked spec, a source tree's spec file or route files, the model fallbacks, writing the context and endpoints |
| `crate/src/context.rs` | `define_api_context`: one API by hand, each GET endpoint probed through the core's `http_request` (`ctx.http`) |
| `crate/src/tags.rs` | an API's kind and topics: checked against the graph's closed lists, set by the caller or proposed by the local decision model at 0.7 or above |
| `contexts/api-integrator.yaml` | the agent that does API work (the developer plugin adds when to draft a plugin) |
| `schemas/tools/<tool>.json` | each tool's input schema, loaded by `tool_schema!` |

Rules: `scripts/dev_docs.sh search "ingest_api"` and `dev_docs search "API tags"`.

## Enable and check

On by default (`PLUGINS_ENABLED` in `docker-compose.yml`). Then
`cargo test -p agent-brain-plugin-apis --lib`, `scripts/check_schemas.sh`
and `scripts/check_plugin_deps.sh`, and rebuild the image.
