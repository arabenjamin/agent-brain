# Map plan

Give the brain a sense of place: located data, spatial questions workflows
can ask, layers, and a self-hosted map to look at. A map is only as useful as
the located data behind it, so the data comes first and the map last. Planned
2026-10-21 from todo d203bad4 ("a map in the entity drawer"), widened by the
owner to workflows: track flights and shipping for the NIA, plan a trip,
track UFO reports. **NIA first.** Tiles: **Protomaps**, self-hosted (owner).

## 1. Measured (2026-10-21)

- **No coordinates anywhere in the graph.** 542 entities are `location`;
  Wikidata typed some of them (country 38, municipality 16, state or province
  13, region 12, county 6, geographic feature 5, facility 2).
- **Wikidata has the coordinates; linking is the limit.** Of the 351 linked
  entities, 95 are locations and **93 have a coordinate (P625)** (the two
  without are untyped); organisations 17 of 94 (headquarters), events 2 of 10,
  people 0. Only 95 of 542 locations are linked so far, and **linking has
  stopped**: the `entity-linking` workflow (gpt-oss cloud) has been off since
  2026-10-11, when the free quota ran out. More points need linking back on
  (a model choice, §5.3), or a drawer "Look up" per entity. 3 items carry
  more than one coordinate.
- **The fetch already exists.** The reference plugin reads each linked item's
  claims through the Wikimedia gate (`wbgetentities`, 50 per request) for its
  kind walk; P625 is one more claim in the same response.
- **Other sources in reach:** ACLED's API returns latitude/longitude, which the
  osint-data plugin does not request yet. The websocket plugin is back
  (2026-10-20), which ship positions need (aisstream.io streams AIS over a
  WebSocket).
- **Spatial is native:** Neo4j `point()` with `point.distance` and
  `point.withinBBox`; DuckDB has a spatial extension for high-volume tracks.
- **Protomaps tile sizes** (daily build 2026-10-21, 139 GB planet,
  `pmtiles extract --dry-run`): world to zoom 6 = 45 MB, 7 = 189 MB,
  8 = 559 MB. Michigan bbox to zoom 12 = 128 MB, 14 = 561 MB, 15 = 1.2 GB.

## 2. Shape: four layers, the map last

1. **Located data.** One shape for "a thing at a place, at a time": a Neo4j
   `point` (WGS 84), an optional time, a source. Static places are entity
   points (from Wikidata). Events and observations (an ACLED event, a UFO
   report, a trip stop, "ship X entered the strait") are located notes and
   items, linked to their entities. **High-volume tracks** (flight and ship
   positions) go to DuckDB with a retention window; only the events they
   produce reach the graph.
2. **Spatial primitives for workflows.** A workflow does not look at a map; it
   asks questions: `geocode` (a place name to a point, cached), and
   within / near / entered ("inside this box", "within 50 km of X", "entered
   since the last check"). The geofence is the NIA tripwire.
3. **Layers.** A named, query-backed set of located things ("NIA: Hormuz
   shipping", "Trip: Japan", "UFO reports, Michigan"), declared in a plugin's
   `layers/*.yaml` (no hardcoded data) and owned by a program, topic or
   workflow. A layer stores its query, not its points, so it is always live.
4. **The map.** MapLibre GL in the HBI over Protomaps vector tiles: a map in
   the entity drawer (Phase 1), a Map panel with layer toggles and a time
   slider (Phase 3).

### Tiles: Protomaps, self-hosted

A PMTiles archive is one file read by HTTP range requests, so the HBI's own
nginx serves it: no tile server, no third party sees where we look, and the
map works offline. `scripts/fetch_map_tiles.sh` extracts from the daily
build with `go-pmtiles` in Docker (nothing on the host) into `./maps/`
(gitignored, mounted read-only into the HBI at `/maps/`), with the basemap's
fonts and sprites beside it (from `protomaps/basemaps-assets`), so nothing is
fetched from GitHub at view time. Default: **world to zoom 7 (189 MB) and
Michigan to zoom 14 (561 MB)**; regions are a list (`maps/regions.yaml`),
so an NIA area of interest is one more line. Refreshed by re-running; the
build is daily, a refresh every few months is plenty.

### Privacy

- Tiles are local: viewing never leaves the network.
- **Geocoding sends the place name out.** Public places (NIA, news, UFO
  reports) are public work and may use Nominatim (1 request/s, a contact in
  the User-Agent). A trip's addresses are private: they geocode only against
  a self-hosted geocoder, or not at all (§5). The `data_class` rule applies
  to tools as it does to models.
- Flight and ship positions are public broadcasts; what we choose to watch
  can reveal an interest, so the watch lists stay in the NIA program's own
  files, not in public queries beyond the source API.

## 3. Phases

**Phase 1: entity points and the drawer map.** *Built 2026-10-21 (§4).*

- The reference plugin reads P625 with the claims it already fetches and
  stores `w.location` (a Neo4j point) and `w.coords_at` on the `:WikiItem`;
  a backfill fills the items stored before, in batches, through the same
  Wikimedia gate. An item with several coordinates keeps the first
  (preferred-rank first) and says how many it had.
- `GET /api/plugins/reference/entities/{id}/wiki` returns the link's
  `location: {lat, lon}`.
- `scripts/fetch_map_tiles.sh`, the `/maps/` mount, and an `EntityMap` in the
  drawer: a MapLibre map centred on the point, zoomed by kind (country,
  state, municipality, facility), with a marker; "no coordinates" says why
  (not linked, or the item has none).

**Phase 2: located data and `geocode`.** A `location` point on notes and
items; `geocode` (Nominatim for public work, cached as `(:Place)`, refused for
private work until §5's decision); ACLED requests latitude/longitude and its
events carry a point; claims about a linked place inherit its point. Spatial
helpers in the views (`near`, `within`).

**Phase 3: layers and the Map panel.** The `layers/` seed kind (query, style,
owner); `lookup`-backed layer queries; the HBI Map panel with layer toggles,
entity points, ACLED events, and a time slider for anything with a time.

**Phase 4: NIA tracks and tripwires.** First live sources, measured before
building (rate limits, coverage, volume per day): ships through aisstream.io
over the websocket plugin, flights through OpenSky's REST API. Positions to
DuckDB (`tracks`, retention window); a geofence workflow per area of interest
that writes an event note when a watched class of vessel or aircraft enters
or leaves, and an item when the owner asked to be told. Areas are layers.

**Later.** Trip planning (routing, the private-data story first); UFO reports
(NUFORC's public data); boundaries (country and state polygons) for
"within a country".

## 4. Phase 1 as built (2026-10-21)

- **Coordinates.** `wiki.rs` reads P625 from the claims every lookup already
  fetches (`coord_of`: preferred rank, else first normal; never deprecated;
  Earth only, since Wikidata also places craters on Mars) and stores
  `location` (a Neo4j point), `coord_count` and `coords_at` on the
  `:WikiItem`. `geo.rs` backfills older items 50 per request through the
  Wikimedia gate, on the plugin's own idle loop (every 30 min, skipped while
  the brain is busy, deployed brain only), not the entity-linking pass,
  which is off and needs a model. The first pass read all 351 linked items:
  93 of 95 locations, 18 organisations, 3 concepts, 2 events, 1 product have
  a point, as measured. The drawer also reads one unread item on demand.
- **Points in generic queries.** The first native point exposed a silent
  failure: neo4rs could not turn a point into `serde_json::Value` (its field
  names answered a string request with a number), and `neo4j_query`, the
  canned views and the plugin graph shim nulled the whole row. Fixed in the
  vendored neo4rs (third patch, `types/serde/cenum.rs`, with a regression
  test); a point now reads `{sr_id: 4326, x: lon, y: lat}`. Spatial Cypher
  works today: "within 300 km of Detroit" returns Roseville, Macomb County,
  MSU, Cleveland, Ohio.
- **Tiles.** `scripts/fetch_map_tiles.sh` (go-pmtiles v1.31.2 in Docker,
  build 20261021): `world.pmtiles` 181 MB to zoom 7, `region-michigan.pmtiles`
  535 MB to zoom 14, fonts and sprites (basemaps-assets pinned) 15 MB, and a
  `manifest.json` the HBI reads for its sources. The HBI's nginx serves
  `/maps/` read-only with range requests (206).
- **The drawer map.** `EntityMap` (shown for places, and for anything else
  with a point) over `PointMap`: MapLibre GL 6.13 (lazy chunk; its tile
  worker bundled as an ES-module worker, which a real browser caught when the
  tests could not), the Protomaps dark flavor, a region's detail drawn over
  the world from zoom 7, zoom by kind, a marker, OSM attribution. When there
  is no map it says why: not linked, not read yet, Wikidata has none, or no
  tiles on the server. Checked in headless Chromium: Detroit (Michigan
  detail, z10) and Yemen (world, z3.5) render with no console errors and no
  failed requests.

## 5. Decisions for the owner

1. **Default regions.** World to zoom 7 plus Michigan to zoom 14 (~750 MB).
   More detail is per region; which NIA areas first (Strait of Hormuz,
   Taiwan Strait, Black Sea, Red Sea and Bab-el-Mandeb)?
2. **Private geocoding.** For trips: self-host a geocoder (Nominatim or
   Photon on a regional extract, GBs of disk and an import), or geocode only
   public places and enter private stops by hand on the map.
3. **Linking again.** `entity-linking` is the only thing that grows the set
   of entities with a point. It was paused for the free gpt-oss quota; turn
   it back on (on what model), or leave linking to the drawer's on-demand
   "Look up".
4. **Phase 4 sources.** aisstream.io needs a free key; OpenSky allows
   anonymous use at low limits and more with a free account. Measure both
   on the NIA's areas before choosing.
