# harness

The brain improves its own sub-agents from evidence (Harness²): probe the agent, write candidates, probe them, compose, record the lesson; the owner adopts.

Built before the template and linked to it on 2026-11-06; `.cruft.json`
records the template commit.
`scripts/plugin_template.sh update harness` brings a later
template change in as a diff to review.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description, what it requires |
| `workflows/harness-improve.yaml` | `{agent, task, why}`: probe the agent, write an invariant and a domain candidate, probe both, compose, probe the result, record the lesson; never adopts by itself |
| `workflows/harness-candidates.yaml` | weekly: starts one improvement for the sub-agent with two or more bad runs |
| `contexts/harness-editor.yaml`, `harness-composer.yaml`, `harness-reflector.yaml` | the agents those steps run |

See `project-docs/HARNESS2_PLAN.md`.

Seeds go in the directories the brain reads (`workflows/`, `contexts/`,
`views/`, …); `scripts/check_schemas.sh` lists the kinds it knows. A data
plugin has no Rust: its workflows use the brain's own tools.

## Enable and check

On by default (`PLUGINS_ENABLED` in `docker-compose.yml`).
