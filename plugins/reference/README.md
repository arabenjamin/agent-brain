# reference

Reference links (link_entities, the entity-linking workflow), entity kinds from Wikidata classes (kinds.yaml, type questions), and Wikipedia background (wiki_background), with their HBI routes.

Built before the template and linked to it on 2026-11-06; `.cruft.json`
records the template commit.
`scripts/plugin_template.sh update reference` brings a later
template change in as a diff to review.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description |
| `kinds.yaml` | the fine entity kinds, each with its Wikidata root classes and the coarse type it implies |
| `crate/src/lib.rs` | the `Plugin` impl (`Reference`) |
| `crate/src/skill.rs` | `link_entities`, `wiki_background` |
| `crate/src/wiki.rs` | reference links: which Wikidata item and Wikipedia article an entity is, as proposals the person confirms |
| `crate/src/background.rs` | `wiki_background`: Wikipedia as the first step of researching a question |
| `crate/src/kinds.rs` | an entity's fine `kind`, derived from its linked item |
| `crate/src/geo.rs` | a linked item's coordinates (Wikidata P625) as a native point |
| `crate/src/db.rs` | the graph as the plugin uses it, and the core's merge candidates |
| `crate/src/routes.rs` | the HTTP routes under `/api/plugins/reference/` |
| `schemas/tools/` | the two tools' input schemas |
| `workflows/entity-linking.yaml` | hourly `link_entities` |

Graph access is in `db.rs` (with the queries in each module).

## Enable and check

On by default (`PLUGINS_ENABLED` in `docker-compose.yml`); `wiki_background` needs `WIKIMEDIA_CONTACT`. Then
`cargo test -p agent-brain-plugin-reference --lib`, `scripts/check_schemas.sh`
and `scripts/check_plugin_deps.sh`, and rebuild the image.
