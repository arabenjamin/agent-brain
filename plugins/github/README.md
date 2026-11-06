# github

GitHub repo watches over the REST API: ci-watch tells the owner when a CI run newly fails, and when a failing branch recovers. Watches agent-brain, gizmatron and gizmo-server.

Built before the template and linked to it on 2026-11-06; `.cruft.json`
records the template commit.
`scripts/plugin_template.sh update github` brings a later
template change in as a diff to review.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description, what it requires |
| `workflows/ci-watch.yaml` | hourly (Brain Ops): the latest GitHub Actions run per workflow and branch of the watched repos, through the core's `github` API context; a new failure opens an action item, a recovery sends a note |

Seeds go in the directories the brain reads (`workflows/`, `contexts/`,
`views/`, …); `scripts/check_schemas.sh` lists the kinds it knows. A data
plugin has no Rust: its workflows use the brain's own tools.

## Enable and check

On by default (`PLUGINS_ENABLED` in `docker-compose.yml`); loads only with `GITHUB_TOKEN` set.
