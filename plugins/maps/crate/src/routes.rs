//! The maps plugin's HTTP routes, served by the host under
//! `/api/plugins/maps/` (MAP_PLAN Phase 3, the HBI's Map panel).
//!
//! - `GET    /layers` — every loaded bundle's layers, and the files that
//!   did not parse (said, not dropped).
//! - `GET    /layers/{name}?since=&until=` — one layer's points as GeoJSON,
//!   with what was cut or skipped; `since`/`until` (RFC 3339) only for a
//!   timed layer.
//! - `GET    /places` — the places added by hand.
//! - `POST   /places` `{name, lat, lon, note?}` — add one, or move it (a
//!   move without `note` keeps the old one; `note: ""` clears it).
//! - `DELETE /places/{name}` — remove one (hand-placed only: the geocode
//!   cache is never touched here).
//!
//! - `GET    /areas` — the watch areas (Phase 4) as GeoJSON outlines, each
//!   with its last day's coverage per source.
//! - `GET    /areas/positions?minutes=60` — the latest position of each ship
//!   and aircraft seen in the areas in that window (at most 2,000).
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
    routing::{delete, get},
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
        .route("/areas", get(handle_areas))
        .route("/areas/positions", get(handle_positions))
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
    match layers::features(s.graph.as_ref(), &layer, since, until).await {
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

async fn handle_areas(State(s): St) -> Response {
    let (areas, problems) = s.tracks.areas();
    let store = s.tracks.store.clone();
    let since = tracks::now() - 86_400;
    let mut features = Vec::new();
    for a in areas {
        let mut cov = serde_json::Map::new();
        for (source, what, on) in [("ais", "ships", a.ships), ("adsb", "aircraft", a.aircraft)] {
            if !on {
                continue;
            }
            let c = match &store {
                Ok(st) => {
                    let name = a.name.clone();
                    st.blocking(move |st| st.coverage(source, &name, since))
                        .await
                        .ok()
                }
                Err(_) => None,
            };
            let (_, up, reports) = c.unwrap_or((0, 0, 0));
            let (state, says) = judge(what, 86_400, up, reports);
            cov.insert(
                what.into(),
                json!({"state": state, "says": says, "reports": reports}),
            );
        }
        let [w, so, e, n] = a.bbox;
        features.push(json!({
            "type": "Feature",
            "geometry": {"type": "Polygon", "coordinates": [[[w, so], [e, so], [e, n], [w, n], [w, so]]]},
            "properties": {
                "name": a.name, "title": a.title, "description": a.description,
                "ships": a.ships, "aircraft": a.aircraft, "coverage": cov,
            },
        }));
    }
    Json(json!({
        "type": "FeatureCollection", "features": features, "problems": problems,
        "collectors": s.tracks.status.snapshot(),
        "store": s.tracks.store.as_ref().err(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct Minutes {
    minutes: Option<i64>,
}

async fn handle_positions(State(s): St, Query(q): Query<Minutes>) -> Response {
    let minutes = q.minutes.unwrap_or(60);
    if !(1..=1440).contains(&minutes) {
        return error(StatusCode::BAD_REQUEST, "minutes must be 1 to 1440");
    }
    let store = match &s.tracks.store {
        Ok(st) => st.clone(),
        Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, e),
    };
    let since = tracks::now() - minutes * 60;
    let rows = store
        .blocking(move |st| st.latest(None, None, since, MAX_POSITIONS + 1))
        .await;
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let truncated = rows.len() > MAX_POSITIONS;
    let features: Vec<Value> = rows
        .into_iter()
        .take(MAX_POSITIONS)
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
        .collect();
    let count = features.len();
    Json(json!({
        "type": "FeatureCollection", "features": features, "count": count, "minutes": minutes,
        "truncated": truncated,
        "note": if truncated { Some(format!("Showing the {MAX_POSITIONS} most recently seen.")) } else { None },
    }))
    .into_response()
}

async fn handle_tracks_status(State(s): St) -> Response {
    let (areas, problems) = s.tracks.areas();
    Json(json!({
        "collectors": s.tracks.status.snapshot(),
        "areas": areas.iter().map(|a| &a.name).collect::<Vec<_>>(),
        "problems": problems,
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
