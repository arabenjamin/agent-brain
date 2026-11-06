# osint-data

ACLED conflict-event data (acled_events): live queries, summarised, with ACLED's attribution.

Built before the template and linked to it on 2026-11-06; `.cruft.json`
records the template commit.
`scripts/plugin_template.sh update osint-data` brings a later
template change in as a diff to review.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description, the ACLED credentials it requires |
| `crate/src/lib.rs` | the `Plugin` impl (`OsintData`) |
| `crate/src/skill.rs` | `acled_events` |
| `crate/src/client.rs` | the ACLED API client, and the licence and storage rules (queried live, never mirrored) |
| `schemas/tools/acled_events.json` | its input schema |

No seeds and no graph labels of its own.

## Enable and check

On by default (`PLUGINS_ENABLED` in `docker-compose.yml`); loads only with `ACLED_EMAIL` and `ACLED_PASSWORD` set. Then
`cargo test -p agent-brain-plugin-osint-data --lib`, `scripts/check_schemas.sh`
and `scripts/check_plugin_deps.sh`, and rebuild the image.
