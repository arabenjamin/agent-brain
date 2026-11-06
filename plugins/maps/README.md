# maps

Places and spatial questions: geocode (a place name to a point: the brain's own located entities first, then a cache, then OpenStreetMap's Nominatim, public names only), the views places_near and notes_near, map layers (layers/*.yaml, read-only queries the HBI's Map panel draws) with places added by hand, and ship (aisstream.io) and aircraft (OpenSky) tracks over watch areas (areas/*.yaml) with area_traffic, every count with its coverage.

Built before the template and linked to it on 2026-11-06; `.cruft.json`
records the template commit.
`scripts/plugin_template.sh update maps` brings a later
template change in as a diff to review.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description |
| `crate/src/lib.rs` | the `Plugin` impl (`Maps`): two skills, the routes, the `layers` and `areas` seed kinds; `start` applies the graph schema, loads the layers, seeds the watch areas and spawns the collectors |
| `crate/src/geocode.rs` | `geocode`: the brain's located entities first, then the `(:Place)` cache, then OpenStreetMap's Nominatim (one request a second, `NOMINATIM_CONTACT`) |
| `crate/src/layers.rs` | map layers (`layers/*.yaml` in any bundle): named read-only queries that return located rows, always live |
| `crate/src/routes.rs` | the Map panel's HTTP routes under `/api/plugins/maps/` |
| `crate/src/tracks/mod.rs` | ship and aircraft tracks over the watch areas, collected in the background |
| `crate/src/tracks/areas.rs` | watch areas (`areas/*.yaml` in any bundle; this one ships none, they are private) |
| `crate/src/tracks/ais.rs` | ships: one aisstream.io WebSocket (`AISSTREAM_API_KEY`, sent only there, never logged) |
| `crate/src/tracks/opensky.rs` | aircraft: OpenSky's `states/all` (`OPENSKY_CLIENT_ID`, `OPENSKY_CLIENT_SECRET`) |
| `crate/src/tracks/store.rs` | positions in their own DuckDB file (`TRACKS_DB_PATH`), never the graph |
| `crate/src/tracks/tool.rs` | `area_traffic`: what the collectors saw in an area, every count with its coverage |
| `layers/` | the shipped layers: `places`, `mentioned`, `located-notes`, `geocoded`, `my-places` |
| `views/places_near.yaml`, `notes_near.yaml` | the `lookup` views of what the brain knows, and what its notes say, within a radius |
| `schemas/tools/` | `geocode` and `area_traffic` input schemas |
| `schemas/graph/schema.cypher` | `Place` and `WatchArea` constraints |
| `schemas/duckdb/tracks.sql` | the tracks database's tables |
| `schemas/seeds/` | the generated shapes of `layers` and `areas` files |

Graph access is in each module (`geocode.rs`, `layers.rs`, `tracks/`); the tracks are DuckDB, and only what they add up to reaches the graph. See `project-docs/MAP_PLAN.md`.

## Enable and check

On by default (`PLUGINS_ENABLED` in `docker-compose.yml`). The ship and aircraft collectors run only in the deployed brain, with their keys set and a watch area to watch; otherwise each reports itself off. Then
`cargo test -p agent-brain-plugin-maps --lib`, `scripts/check_schemas.sh`
and `scripts/check_plugin_deps.sh`, and rebuild the image.
