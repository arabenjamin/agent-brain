# research

Deep research (deep_research): a cited multi-source report from Parallel's Task API, capped by a monthly spend ledger (:ResearchRun), and the deep-research workflow.

Built before the template and linked to it on 2026-11-06; `.cruft.json`
records the template commit.
`scripts/plugin_template.sh update research` brings a later
template change in as a diff to review.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description, the `PARALLEL_API_KEY` it requires |
| `parallel.yaml` | the processors `deep_research` may use, each one's price and wait, and the monthly cap |
| `crate/src/lib.rs` | the `Plugin` impl (`Research`): `start` applies the graph schema and spawns the collector |
| `crate/src/skill.rs` | `deep_research`; its schema is built in Rust from `parallel.yaml`'s processors |
| `crate/src/client.rs` | the Parallel Task API client |
| `crate/src/config.rs` | reads `parallel.yaml` |
| `crate/src/runs.rs` | `(:ResearchRun)`: the spend ledger, the resume point and the background collector's state |
| `schemas/graph/schema.cypher` | `ResearchRun`'s constraint and index |
| `workflows/deep-research.yaml`, `contexts/researcher-deep-research.yaml` | the workflow and its agent |

Graph access is in `runs.rs`.

## Enable and check

On by default (`PLUGINS_ENABLED` in `docker-compose.yml`); loads only with `PARALLEL_API_KEY` set. Paid per run, capped monthly. Then
`cargo test -p agent-brain-plugin-research --lib`, `scripts/check_schemas.sh`
and `scripts/check_plugin_deps.sh`, and rebuild the image.
