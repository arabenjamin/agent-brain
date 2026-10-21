//! The maps plugin's HTTP routes, served by the host under
//! `/api/plugins/maps/` (MAP_PLAN Phase 3, the HBI's Map panel).
//!
//! - `GET    /layers` — every loaded bundle's layers, and the files that
//!   did not parse (said, not dropped).
//! - `GET    /layers/{name}?since=&until=&zoom=&west=&south=&east=&north=`
//!   — one layer's points as GeoJSON, with what was cut or skipped;
//!   `since`/`until` (RFC 3339) only for a timed layer; the view's box only
//!   reaches a `view` layer; under its `minzoom` the answer is "zoom in",
//!   with no query (Phase 5b).
//! - `GET    /places` — the places added by hand.
//! - `POST   /places` `{name, lat, lon, note?}` — add one, or move it (a
//!   move without `note` keeps the old one; `note: ""` clears it).
//! - `DELETE /places/{name}` — remove one (hand-placed only: the geocode
//!   cache is never touched here).
//!
//! - `GET    /areas?minutes=60` — the watch areas (graph, Phase 5a) as
//!   GeoJSON outlines: on or off, from a file or drawn, the last day's
//!   coverage per source and how many were seen in `minutes`.
//! - `POST   /areas` `{title, bbox, ships, aircraft, description?, program?}`
//!   — watch a new area (drawn on the map).
//! - `PUT    /areas/{name}` `{enabled?}` and/or the fields above — switch it
//!   on or off (any area), or change a drawn one.
//! - `DELETE /areas/{name}` — a drawn area (a file's is switched off).
//! - `GET    /areas/positions?minutes=60&west=&south=&east=&north=` — the
//!   latest position of each ship and aircraft seen in that window, inside
//!   the view when one is given (at most 2,000, a cut said).
//! - `POST   /aircraft/snapshot` `{bbox}` — one OpenSky look at the view, not
//!   stored (Phase 5d: at most 100 square degrees, one a minute).
//! - `GET    /tracks/status` — what the collectors are doing.
//!
//! Places added by hand are private (§5.2): they live only in the graph,
//! and geocode never reads them (their key is `hand:<name>`).

use std::sync::Arc;

use agent_brain_plugin_api::axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use agent_brain_plugin_api::graph::query;
use agent_brain_plugin_api::{Graph, PluginContext};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::geocode::key_of;
use crate::layers::{self, Layer};
use crate::tracks::{self, Tracks, tool::judge};

const MAX_NAME: usize = 80;
const MAX_NOTE: usize = 500;

pub struct RouteState {
    pub graph: Arc<dyn Graph>,
    pub ctx: PluginContext,
    pub tracks: Tracks,
}

type St = State<Arc<RouteState>>;

pub fn router(state: Arc<RouteState>) -> Router {
    Router::new()
        .route("/layers", get(handle_layers))
        .route("/layers/{name}", get(handle_layer))
        .route("/places", get(handle_places).post(handle_add_place))
        .route("/places/{name}", delete(handle_delete_place))
        .route("/areas", get(handle_areas).post(handle_add_area))
        .route("/areas/positions", get(handle_positions))
        .route(
            "/areas/{name}",
            put(handle_change_area).delete(handle_delete_area),
        )
        .route("/aircraft/snapshot", post(handle_snapshot))
        .route("/tracks/status", get(handle_tracks_status))
        .with_state(state)
}

fn error(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "error": msg.to_string() }))).into_response()
}

/// Every loaded bundle's layers, `(plugin, layer)`, and the files that failed.
pub fn load_all(ctx: &PluginContext) -> (Vec<(String, Layer)>, Vec<Value>) {
    let mut ok: Vec<(String, Layer)> = Vec::new();
    let mut bad = Vec::new();
    for (plugin, path) in ctx.files("layers") {
        match Layer::load(&path) {
            Ok(l) if ok.iter().any(|(_, o)| o.name == l.name) => bad.push(json!({
                "plugin": plugin, "file": path.display().to_string(),
                "error": format!("a layer named '{}' is already loaded", l.name),
            })),
            Ok(l) => ok.push((plugin, l)),
            Err(e) => bad.push(json!({
                "plugin": plugin, "file": path.display().to_string(), "error": e,
            })),
        }
    }
    (ok, bad)
}

async fn handle_layers(State(s): St) -> Response {
    let (layers, problems) = load_all(&s.ctx);
    let list: Vec<Value> = layers
        .iter()
        .map(|(plugin, l)| {
            json!({
                "name": l.name, "title": l.title, "description": l.description,
                "color": l.color, "timed": l.timed, "private": l.private, "on": l.on,
                "view": l.view, "minzoom": l.minzoom, "cluster": l.cluster,
                "owner": l.owner.as_ref().map(|o| json!({
                    "program": o.program, "topic": o.topic, "workflow": o.workflow,
                })),
                "plugin": plugin,
            })
        })
        .collect();
    Json(json!({"layers": list, "problems": problems})).into_response()
}

#[derive(Deserialize)]
struct Window {
    since: Option<String>,
    until: Option<String>,
    /// The map's zoom: below a layer's `minzoom` it is not queried.
    zoom: Option<f64>,
    // Not `#[serde(flatten)] View`: flattened query fields arrive as strings
    // and a number fails to parse.
    west: Option<f64>,
    south: Option<f64>,
    east: Option<f64>,
    north: Option<f64>,
}

fn instant(v: &Option<String>, what: &str) -> Result<Option<String>, String> {
    match v.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|d| Some(d.with_timezone(&Utc).to_rfc3339()))
            .map_err(|_| format!("{what} must be an RFC 3339 time, e.g. 2026-10-21T00:00:00Z")),
    }
}

async fn handle_layer(State(s): St, Path(name): Path<String>, Query(w): Query<Window>) -> Response {
    let (layers, _) = load_all(&s.ctx);
    let Some((_, layer)) = layers.into_iter().find(|(_, l)| l.name == name) else {
        return error(StatusCode::NOT_FOUND, format!("no layer named '{name}'"));
    };
    let (since, until) = match (instant(&w.since, "since"), instant(&w.until, "until")) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return error(StatusCode::BAD_REQUEST, e),
    };
    if !layer.timed && (since.is_some() || until.is_some()) {
        return error(
            StatusCode::BAD_REQUEST,
            format!("layer '{name}' has no time: since and until do not apply"),
        );
    }
    let v = View {
        west: w.west,
        south: w.south,
        east: w.east,
        north: w.north,
    };
    let view = match v.bbox() {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    if let (Some(min), Some(z)) = (layer.minzoom, w.zoom)
        && z < min
    {
        return Json(json!({
            "type": "FeatureCollection", "layer": name, "features": [], "count": 0,
            "skipped": 0, "truncated": false, "hidden": true,
            "note": format!("Zoom in to see this layer (from zoom {min})."),
        }))
        .into_response();
    }
    match layers::features(s.graph.as_ref(), &layer, since, until, view).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("layer '{name}': {e}"),
        ),
    }
}

async fn handle_places(State(s): St) -> Response {
    let rows = s
        .graph
        .query(
            "MATCH (p:Place {source: 'hand'})
             RETURN p.name AS name, p.location.y AS lat, p.location.x AS lon, p.note AS note,
                    toString(p.created_at) AS created_at, toString(p.updated_at) AS updated_at
             ORDER BY toLower(p.name)",
            json!({}),
        )
        .await;
    match rows {
        Ok(r) => Json(json!({"places": r, "count": r.len()})).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
struct NewPlace {
    name: String,
    lat: f64,
    lon: f64,
    #[serde(default)]
    note: Option<String>,
}

/// A hand-placed place, checked: `(name, key, note)`.
fn check_place(p: &NewPlace) -> Result<(String, String, Option<String>), String> {
    let name = p.name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Err(format!(
            "a place needs a name of 1 to {MAX_NAME} characters"
        ));
    }
    if !(-90.0..=90.0).contains(&p.lat) || !(-180.0..=180.0).contains(&p.lon) {
        return Err("lat must be -90 to 90 and lon -180 to 180".into());
    }
    let note = p
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string);
    if note.as_ref().is_some_and(|n| n.chars().count() > MAX_NOTE) {
        return Err(format!("a note is at most {MAX_NOTE} characters"));
    }
    Ok((name.clone(), hand_key(&name), note))
}

fn hand_key(name: &str) -> String {
    format!("hand:{}", key_of(name))
}

async fn handle_add_place(State(s): St, Json(p): Json<NewPlace>) -> Response {
    let (name, key, note) = match check_place(&p) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    let rows = s
        .graph
        .execute(
            query(
                "OPTIONAL MATCH (old:Place {key: $key})
                 WITH old IS NOT NULL AS existed
                 MERGE (p:Place {key: $key})
                 ON CREATE SET p.created_at = datetime()
                 SET p.source = 'hand', p.private = true, p.name = $name,
                     p.note = CASE WHEN $keep_note THEN p.note ELSE $note END,
                     p.location = point({latitude: $lat, longitude: $lon}),
                     p.updated_at = datetime()
                 RETURN existed, p.note AS note",
            )
            .param("key", &key)
            .param("name", &name)
            .param("note", &note)
            .param("keep_note", p.note.is_none())
            .param("lat", p.lat)
            .param("lon", p.lon),
        )
        .await;
    match rows {
        Ok(r) => {
            let row = r.first();
            let existed = row
                .and_then(|r| r.get::<bool>("existed").ok())
                .unwrap_or(false);
            let kept = row.and_then(|r| r.get::<String>("note").ok());
            Json(json!({
                "place": {"name": name, "lat": p.lat, "lon": p.lon, "note": kept},
                "action": if existed { "moved" } else { "added" },
            }))
            .into_response()
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn handle_delete_place(State(s): St, Path(name): Path<String>) -> Response {
    let rows = s
        .graph
        .execute(
            query(
                "MATCH (p:Place {key: $key, source: 'hand'})
                 WITH p, p.name AS name DELETE p RETURN name",
            )
            .param("key", hand_key(&name)),
        )
        .await;
    match rows {
        Ok(r) if r.is_empty() => error(
            StatusCode::NOT_FOUND,
            format!("no place named '{name}' was added by hand"),
        ),
        Ok(_) => Json(json!({"deleted": name})).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

const MAX_POSITIONS: usize = 2000;

/// A view or a box from the query string: all four or none.
#[derive(Default)]
struct View {
    west: Option<f64>,
    south: Option<f64>,
    east: Option<f64>,
    north: Option<f64>,
}

impl View {
    fn bbox(&self) -> Result<Option<[f64; 4]>, String> {
        match (self.west, self.south, self.east, self.north) {
            (None, None, None, None) => Ok(None),
            (Some(w), Some(s), Some(e), Some(n)) if w < e && s < n => Ok(Some([w, s, e, n])),
            _ => Err(
                "a view is west, south, east and north, with west < east and south < north".into(),
            ),
        }
    }
}

#[derive(Deserialize)]
struct AreasQuery {
    /// Counts of ships and aircraft seen in this many minutes (default 60).
    minutes: Option<i64>,
}

async fn handle_areas(State(s): St, Query(q): Query<AreasQuery>) -> Response {
    let all = match s.tracks.all_areas().await {
        Ok(a) => a,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let minutes = q.minutes.unwrap_or(60).clamp(1, 1440);
    let store = s.tracks.store.clone();
    let day = tracks::now() - 86_400;
    let recent = tracks::now() - minutes * 60;
    let counts = match &store {
        Ok(st) => st
            .blocking(move |st| st.area_counts(recent))
            .await
            .unwrap_or_default(),
        Err(_) => vec![],
    };
    let mut features = Vec::new();
    for w in all {
        let a = &w.area;
        let mut cov = serde_json::Map::new();
        for (source, what, on) in [("ais", "ships", a.ships), ("adsb", "aircraft", a.aircraft)] {
            if !on {
                continue;
            }
            let c = match &store {
                Ok(st) => {
                    let name = a.name.clone();
                    st.blocking(move |st| st.coverage(source, &name, day))
                        .await
                        .ok()
                }
                Err(_) => None,
            };
            let (_, up, reports) = c.unwrap_or((0, 0, 0));
            let (state, says) = judge(what, 86_400, up, reports);
            let seen = counts
                .iter()
                .find(|(ar, so, _)| *ar == a.name && *so == source)
                .map(|c| c.2)
                .unwrap_or(0);
            cov.insert(
                what.into(),
                json!({"state": state, "says": says, "reports": reports, "seen": seen}),
            );
        }
        let [wl, so, e, n] = a.bbox;
        features.push(json!({
            "type": "Feature",
            "geometry": {"type": "Polygon", "coordinates": [[[wl, so], [e, so], [e, n], [wl, n], [wl, so]]]},
            "properties": {
                "name": a.name, "title": a.title, "description": a.description,
                "ships": a.ships, "aircraft": a.aircraft, "coverage": cov,
                "on": w.enabled, "from": w.managed_by, "bbox": a.bbox,
                "center": [(wl + e) / 2.0, (so + n) / 2.0],
                "sq_deg": a.sq_deg(),
            },
        }));
    }
    Json(json!({
        "type": "FeatureCollection", "features": features, "problems": s.tracks.problems(),
        "collectors": s.tracks.status.snapshot(), "minutes": minutes,
        "store": s.tracks.store.as_ref().err(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct AreaInput {
    title: String,
    bbox: [f64; 4],
    #[serde(default)]
    ships: bool,
    #[serde(default)]
    aircraft: bool,
    #[serde(default)]
    description: String,
    #[serde(default)]
    program: Option<String>,
}

impl AreaInput {
    fn area(self, name: String) -> tracks::Area {
        tracks::Area {
            name,
            title: self.title.trim().to_string(),
            description: self.description.trim().to_string(),
            owner: self
                .program
                .filter(|p| !p.trim().is_empty())
                .map(|p| crate::layers::Owner {
                    program: Some(p),
                    topic: None,
                    workflow: None,
                }),
            bbox: self.bbox,
            ships: self.ships,
            aircraft: self.aircraft,
        }
    }
}

async fn handle_add_area(State(s): St, Json(input): Json<AreaInput>) -> Response {
    let name = tracks::areas::slug(&input.title);
    if name.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "an area needs a title with a letter or digit in it",
        );
    }
    let g = match s.tracks.graph_handle() {
        Ok(g) => g,
        Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, e),
    };
    let area = input.area(name.clone());
    match tracks::areas::create(g.as_ref(), &area).await {
        Ok(()) => {
            s.tracks.changed();
            Json(json!({"created": name})).into_response()
        }
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

/// A change to an area: only the fields given change.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AreaChange {
    enabled: Option<bool>,
    title: Option<String>,
    bbox: Option<[f64; 4]>,
    ships: Option<bool>,
    aircraft: Option<bool>,
    description: Option<String>,
}

impl AreaChange {
    fn edits(&self) -> bool {
        self.title.is_some()
            || self.bbox.is_some()
            || self.ships.is_some()
            || self.aircraft.is_some()
            || self.description.is_some()
    }
}

async fn handle_change_area(
    State(s): St,
    Path(name): Path<String>,
    Json(c): Json<AreaChange>,
) -> Response {
    let g = match s.tracks.graph_handle() {
        Ok(g) => g,
        Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, e),
    };
    if c.edits() {
        let all = match s.tracks.all_areas().await {
            Ok(a) => a,
            Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
        };
        let Some(mut a) = all
            .into_iter()
            .find(|w| w.area.name == name)
            .map(|w| w.area)
        else {
            return error(StatusCode::NOT_FOUND, format!("no area named '{name}'"));
        };
        if let Some(t) = &c.title {
            a.title = t.trim().to_string();
        }
        if let Some(b) = c.bbox {
            a.bbox = b;
        }
        if let Some(v) = c.ships {
            a.ships = v;
        }
        if let Some(v) = c.aircraft {
            a.aircraft = v;
        }
        if let Some(d) = &c.description {
            a.description = d.trim().to_string();
        }
        if let Err(e) = tracks::areas::update(g.as_ref(), &a).await {
            return error(StatusCode::BAD_REQUEST, e);
        }
    }
    if let Some(on) = c.enabled
        && let Err(e) = tracks::areas::set_enabled(g.as_ref(), &name, on).await
    {
        return error(StatusCode::NOT_FOUND, e);
    }
    s.tracks.changed();
    Json(json!({"changed": name})).into_response()
}

async fn handle_delete_area(State(s): St, Path(name): Path<String>) -> Response {
    let g = match s.tracks.graph_handle() {
        Ok(g) => g,
        Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, e),
    };
    match tracks::areas::delete(g.as_ref(), &name).await {
        Ok(()) => {
            s.tracks.changed();
            Json(json!({"deleted": name})).into_response()
        }
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize)]
struct PositionsQuery {
    minutes: Option<i64>,
    west: Option<f64>,
    south: Option<f64>,
    east: Option<f64>,
    north: Option<f64>,
}

fn positions_json(rows: Vec<Value>) -> Vec<Value> {
    rows.into_iter()
        .map(|r| {
            let (lat, lon) = (r["lat"].clone(), r["lon"].clone());
            let mut p = r;
            if let Some(o) = p.as_object_mut() {
                o.remove("lat");
                o.remove("lon");
                o.retain(|_, v| !v.is_null());
            }
            json!({"type": "Feature", "geometry": {"type": "Point", "coordinates": [lon, lat]}, "properties": p})
        })
        .collect()
}

async fn handle_positions(State(s): St, Query(q): Query<PositionsQuery>) -> Response {
    let minutes = q.minutes.unwrap_or(60);
    if !(1..=1440).contains(&minutes) {
        return error(StatusCode::BAD_REQUEST, "minutes must be 1 to 1440");
    }
    let v = View {
        west: q.west,
        south: q.south,
        east: q.east,
        north: q.north,
    };
    let bbox = match v.bbox() {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    let store = match &s.tracks.store {
        Ok(st) => st.clone(),
        Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, e),
    };
    let since = tracks::now() - minutes * 60;
    let rows = store
        .blocking(move |st| st.latest(None, None, since, bbox, MAX_POSITIONS + 1))
        .await;
    let mut rows = match rows {
        Ok(r) => r,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let truncated = rows.len() > MAX_POSITIONS;
    rows.truncate(MAX_POSITIONS);
    let features = positions_json(rows);
    let count = features.len();
    Json(json!({
        "type": "FeatureCollection", "features": features, "count": count, "minutes": minutes,
        "truncated": truncated,
        "note": if truncated { Some(format!("Showing the {MAX_POSITIONS} most recently seen in this view: zoom in for the rest.")) } else { None },
    }))
    .into_response()
}

#[derive(Deserialize)]
struct SnapshotInput {
    bbox: [f64; 4],
}

/// One OpenSky snapshot of the map's view (Phase 5d): shown, not stored.
async fn handle_snapshot(State(s): St, Json(input): Json<SnapshotInput>) -> Response {
    match s.tracks.opensky.snapshot(&s.tracks, input.bbox).await {
        Ok((positions, credits)) => {
            let at = tracks::now();
            let rows: Vec<Value> = positions
                .into_iter()
                .map(|p| {
                    json!({"source": p.source, "id": p.id, "lat": p.lat, "lon": p.lon,
                           "speed": p.speed, "course": p.course, "altitude": p.altitude,
                           "at": p.at, "name": p.name, "class": p.class, "snapshot": true})
                })
                .collect();
            let features = positions_json(rows);
            let count = features.len();
            Json(json!({
                "type": "FeatureCollection", "features": features, "count": count, "at": at,
                "credits_left": credits,
                "note": if count == 0 { Some("OpenSky shows no aircraft here now: either none are flying or no receiver covers this view (zero is not quiet).") } else { None },
            }))
            .into_response()
        }
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

async fn handle_tracks_status(State(s): St) -> Response {
    let areas = s.tracks.all_areas().await;
    Json(json!({
        "collectors": s.tracks.status.snapshot(),
        "areas": areas.as_ref().map(|a| a.iter().map(|w| json!({"name": w.area.name, "on": w.enabled, "from": w.managed_by})).collect::<Vec<_>>()).ok(),
        "areas_error": areas.as_ref().err(),
        "problems": s.tracks.problems(),
        "store": s.tracks.store.as_ref().err(),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(name: &str, lat: f64, lon: f64, note: Option<&str>) -> NewPlace {
        NewPlace {
            name: name.into(),
            lat,
            lon,
            note: note.map(Into::into),
        }
    }

    #[test]
    fn a_hand_place_is_checked_and_keyed_apart_from_the_geocode_cache() {
        let (name, key, note) =
            check_place(&place("  Hotel   Kyoto ", 35.0, 135.7, Some(" "))).unwrap();
        assert_eq!(name, "Hotel Kyoto");
        assert_eq!(key, "hand:hotel kyoto");
        assert_eq!(note, None);
        assert!(check_place(&place("", 0.0, 0.0, None)).is_err());
        assert!(check_place(&place("x", 91.0, 0.0, None)).is_err());
        assert!(check_place(&place("x", 0.0, 181.0, None)).is_err());
        assert!(check_place(&place(&"x".repeat(81), 0.0, 0.0, None)).is_err());
        assert!(check_place(&place("x", 0.0, 0.0, Some(&"y".repeat(501)))).is_err());
    }

    #[test]
    fn a_window_is_an_rfc3339_time_or_nothing() {
        assert_eq!(instant(&None, "since"), Ok(None));
        assert_eq!(instant(&Some(" ".into()), "since"), Ok(None));
        assert_eq!(
            instant(&Some("2026-10-21T08:00:00-04:00".into()), "since"),
            Ok(Some("2026-10-21T12:00:00+00:00".into()))
        );
        assert!(instant(&Some("last week".into()), "since").is_err());
    }
}
