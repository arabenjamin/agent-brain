# alias-judge

Alias verdicts: judges each entity merge candidate (same / related / different, with probabilities) on idle time, so the merge review sorts the likely ones first. The person still decides every merge.

Built before the template and linked to it on 2026-11-06; `.cruft.json`
records the template commit.
`scripts/plugin_template.sh update alias-judge` brings a later
template change in as a diff to review.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description |
| `crate/src/lib.rs` | the `Plugin` impl (`AliasJudge`): no tools; `start` spawns the judge |
| `crate/src/judge.rs` | the idle-time loop: the local decision model's verdict on each entity merge candidate, stored as `ALIAS_VERDICT` for the core's review list |

No tools and no seeds: the plugin is its background loop. Graph access is in `judge.rs`.

## Enable and check

On by default (`PLUGINS_ENABLED` in `docker-compose.yml`). Then
`cargo test -p agent-brain-plugin-alias-judge --lib`, `scripts/check_schemas.sh`
and `scripts/check_plugin_deps.sh`, and rebuild the image.
