//! `geocode`: a place name to a point (MAP_PLAN Phase 2).
//!
//! What the brain already knows comes first, so a lookup costs nothing and
//! sends nothing when it can:
//!
//! 1. **The graph:** an entity of that name or alias linked to a Wikidata
//!    item with a point, then any stored Wikidata item of that label with a
//!    point (lookups store every candidate they see).
//! 2. **The cache:** `(:Place {key})`, every earlier Nominatim answer,
//!    misses included (asked again after [`MISS_TTL_DAYS`]).
//! 3. **Nominatim** (OpenStreetMap), on its usage policy: at most one request
//!    a second across the process, an identifying User-Agent with a contact
//!    (`NOMINATIM_CONTACT`, else `WIKIMEDIA_CONTACT`; none = no Nominatim),
//!    and every answer cached. Through the net policy.
//!
//! **Public names only** (owner, MAP_PLAN §5.2): the name goes to
//! OpenStreetMap as a search query goes to a search engine with
//! `search_web`. A private place (a home, a trip's stop) is never geocoded;
//! it is placed by hand on the map. Not finding a place is an answer, not an
//! error: it names what was tried.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::{info, warn};

use agent_brain_plugin_api::graph::query;
use agent_brain_plugin_api::protocol::net_policy;
use agent_brain_plugin_api::{Graph, Skill, ToolCallResult, ToolDefinition, parse_args};

const NOMINATIM: &str = "https://nominatim.openstreetmap.org/search";
/// A miss is asked again after this many days (a new place, an OSM edit).
const MISS_TTL_DAYS: i64 = 30;
const MAX_CANDIDATES: usize = 3;
/// Nominatim's policy is an absolute maximum of one request per second.
const MIN_GAP: Duration = Duration::from_millis(1100);

/// The last Nominatim request, process-wide (every caller shares the limit).
static LAST_CALL: Mutex<Option<Instant>> = Mutex::const_new(None);

pub struct GeocodeSkill {
    graph: Arc<dyn Graph>,
    contact: Option<String>,
    http: reqwest::Client,
}

/// The cache key: lower case, inner whitespace collapsed.
pub fn key_of(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// One Nominatim result, as the API returns it (coordinates are strings).
#[derive(Debug, Clone, Deserialize)]
struct Hit {
    lat: String,
    lon: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    /// `class` in Nominatim's `json` format, `category` in `jsonv2`.
    #[serde(default, alias = "category")]
    class: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    osm_type: Option<String>,
    #[serde(default)]
    osm_id: Option<i64>,
    /// south, north, west, east, as strings.
    #[serde(default)]
    boundingbox: Option<Vec<String>>,
}

/// Nominatim's answer as places: `{name, display_name, lat, lon, kind, osm,
/// bbox: [south, north, west, east]}`, best first, at most [`MAX_CANDIDATES`].
pub fn parse_nominatim(body: &str) -> Result<Vec<Value>, String> {
    let hits: Vec<Hit> = serde_json::from_str(body)
        .map_err(|e| format!("Nominatim answered something else: {e}"))?;
    Ok(hits
        .into_iter()
        .filter_map(|h| {
            let lat: f64 = h.lat.parse().ok().filter(|v: &f64| v.abs() <= 90.0)?;
            let lon: f64 = h.lon.parse().ok().filter(|v: &f64| v.abs() <= 180.0)?;
            let bbox: Option<Vec<f64>> = h
                .boundingbox
                .map(|b| b.iter().filter_map(|x| x.parse().ok()).collect())
                .filter(|b: &Vec<f64>| b.len() == 4);
            Some(json!({
                "name": h.name.filter(|n| !n.is_empty()).or_else(|| h.display_name.clone()),
                "display_name": h.display_name,
                "lat": lat,
                "lon": lon,
                "kind": match (h.class, h.kind) {
                    (Some(c), Some(k)) => Some(format!("{c}:{k}")),
                    (c, k) => c.or(k),
                },
                "osm": match (h.osm_type, h.osm_id) {
                    (Some(t), Some(i)) => Some(format!("{t}/{i}")),
                    _ => None,
                },
                "bbox": bbox,
            }))
        })
        .take(MAX_CANDIDATES)
        .collect())
}

impl GeocodeSkill {
    pub fn new(graph: Arc<dyn Graph>, env: impl Fn(&str) -> Option<String>) -> Self {
        let contact = env("NOMINATIM_CONTACT").or_else(|| env("WIKIMEDIA_CONTACT"));
        let ua = match &contact {
            Some(c) => format!("agent-brain/{} ({c})", env!("CARGO_PKG_VERSION")),
            None => format!("agent-brain/{}", env!("CARGO_PKG_VERSION")),
        };
        Self {
            graph,
            contact,
            http: net_policy::guard(reqwest::Client::builder())
                .user_agent(ua)
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
        }
    }

    fn def() -> ToolDefinition {
        ToolDefinition {
            name: "geocode".to_string(),
            description: "Where a PUBLIC place is: a place name (a city, a strait, a port, a \
                          base, a country) to a point {lat, lon} and the area it covers. Uses \
                          the brain's own located entities first, then its cache, then \
                          OpenStreetMap's Nominatim (the name is sent there, as a search query \
                          is sent with search_web). NEVER geocode a private address or a \
                          person's home: private places are placed by hand on the map. Then \
                          `lookup` places_near / notes_near with the lat and lon for what the \
                          brain knows around it."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "place": {
                        "type": "string",
                        "description": "The place's name, as specific as you can (\"Strait of Hormuz\", \"Port of Aden\", \"Sevastopol\")."
                    }
                },
                "required": ["place"]
            }),
        }
    }

    /// Tier 1: a located entity, then any stored Wikidata item of that label.
    async fn in_graph(&self, key: &str) -> anyhow::Result<Option<Value>> {
        let rows = self
            .graph
            .execute(
                query(
                    "MATCH (e:Entity)-[l:REFERS_TO]->(w:WikiItem) \
                     WHERE l.status IN ['confirmed', 'proposed'] AND w.location IS NOT NULL \
                       AND (toLower(e.name) = $key OR EXISTS { \
                         MATCH (a:EntityAlias)-[:ALIAS_OF]->(e) WHERE toLower(a.name) = $key }) \
                     RETURN e.name AS entity, e.kind AS kind, w.qid AS qid, w.label AS label, \
                            w.location.latitude AS lat, w.location.longitude AS lon, \
                            l.status AS link \
                     ORDER BY CASE l.status WHEN 'confirmed' THEN 0 ELSE 1 END LIMIT 1 \
                     UNION \
                     MATCH (w:WikiItem) WHERE toLower(w.label) = $key AND w.location IS NOT NULL \
                     RETURN null AS entity, null AS kind, w.qid AS qid, w.label AS label, \
                            w.location.latitude AS lat, w.location.longitude AS lon, \
                            null AS link \
                     LIMIT 1",
                )
                .param("key", key),
            )
            .await?;
        // The entity row first, when there is one.
        let best = rows
            .iter()
            .find(|r| r.get::<String>("entity").is_ok())
            .or_else(|| rows.first());
        Ok(best.and_then(|r| {
            Some(json!({
                "name": r.get::<String>("label").ok(),
                "lat": r.get::<f64>("lat").ok()?,
                "lon": r.get::<f64>("lon").ok()?,
                "entity": r.get::<String>("entity").ok(),
                "kind": r.get::<String>("kind").ok(),
                "qid": r.get::<String>("qid").ok(),
                "wikidata_url": r.get::<String>("qid").ok().map(|q| format!("https://www.wikidata.org/wiki/{q}")),
                "link": r.get::<String>("link").ok(),
            }))
        }))
    }

    /// Tier 2: an earlier Nominatim answer. `Some(None)` = a fresh miss.
    async fn in_cache(&self, key: &str) -> anyhow::Result<Option<Option<Vec<Value>>>> {
        let rows = self
            .graph
            .execute(
                query(
                    "MATCH (p:Place {key: $key}) \
                     WHERE p.found OR p.fetched_at > datetime() - duration({days: $ttl}) \
                     RETURN p.found AS found, p.candidates_json AS c",
                )
                .param("key", key)
                .param("ttl", MISS_TTL_DAYS),
            )
            .await?;
        let Some(r) = rows.first() else {
            return Ok(None);
        };
        if !r.get::<bool>("found").unwrap_or(false) {
            return Ok(Some(None));
        }
        let c: Vec<Value> = r
            .get::<String>("c")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Ok(Some(Some(c)))
    }

    async fn remember(&self, key: &str, place: &str, cands: &[Value]) -> anyhow::Result<()> {
        let best = cands.first();
        self.graph
            .run(
                query(
                    "MERGE (p:Place {key: $key}) \
                     SET p.query = $query, p.found = $found, p.source = 'nominatim', \
                         p.name = $name, p.display_name = $display, p.kind = $kind, p.osm = $osm, \
                         p.location = CASE WHEN $lat IS NULL THEN null \
                                           ELSE point({latitude: $lat, longitude: $lon}) END, \
                         p.bbox = $bbox, p.candidates_json = $c, p.fetched_at = datetime()",
                )
                .param("key", key)
                .param("query", place)
                .param("found", best.is_some())
                .param("name", best.and_then(|b| b["name"].as_str()))
                .param("display", best.and_then(|b| b["display_name"].as_str()))
                .param("kind", best.and_then(|b| b["kind"].as_str()))
                .param("osm", best.and_then(|b| b["osm"].as_str()))
                .param("lat", best.and_then(|b| b["lat"].as_f64()))
                .param("lon", best.and_then(|b| b["lon"].as_f64()))
                .param(
                    "bbox",
                    best.map(|b| b["bbox"].clone()).unwrap_or(Value::Null),
                )
                .param("c", serde_json::to_string(cands).unwrap_or_default()),
            )
            .await
    }

    /// Tier 3: one Nominatim search, at most one a second process-wide.
    async fn nominatim(&self, place: &str) -> Result<Vec<Value>, String> {
        {
            let mut last = LAST_CALL.lock().await;
            if let Some(t) = *last {
                let since = t.elapsed();
                if since < MIN_GAP {
                    tokio::time::sleep(MIN_GAP - since).await;
                }
            }
            *last = Some(Instant::now());
        }
        let resp = self
            .http
            .get(NOMINATIM)
            .query(&[
                ("q", place),
                ("format", "jsonv2"),
                ("limit", "3"),
                ("accept-language", "en"),
            ])
            .send()
            .await
            .map_err(|e| match net_policy::refusal_in(&e) {
                Some(why) => format!("Nominatim not asked: {why}"),
                None => format!("Nominatim request failed: {e}"),
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "Nominatim answered HTTP {}: {}",
                status.as_u16(),
                body.chars().take(200).collect::<String>()
            ));
        }
        parse_nominatim(&body)
    }

    async fn handle(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            place: String,
        }
        let input: Input = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let place = input.place.trim();
        let key = key_of(place);
        if key.is_empty() {
            return ToolCallResult::error("`place` is empty: name the place to find.".to_string());
        }
        let mut tried = vec![];

        match self.in_graph(&key).await {
            Ok(Some(p)) => {
                return ToolCallResult::success_json(json!({
                    "query": place, "found": true, "source": "graph", "place": p,
                    "note": "From the brain's own knowledge (a Wikidata coordinate); nothing was sent out.",
                }));
            }
            Ok(None) => {
                tried.push("the brain's located entities and Wikidata items: none of that name")
            }
            Err(e) => {
                warn!(error = %e, "geocode: graph lookup failed");
                tried.push("the graph (the lookup failed)");
            }
        }
        match self.in_cache(&key).await {
            Ok(Some(Some(c))) if !c.is_empty() => return found(place, "cache", c),
            Ok(Some(_)) => {
                return ToolCallResult::success_json(json!({
                    "query": place, "found": false, "source": "cache",
                    "note": format!("Nominatim found nothing for this name within the last {MISS_TTL_DAYS} days (cached miss). Try a more specific or official name."),
                }));
            }
            Ok(None) => {}
            Err(e) => warn!(error = %e, "geocode: cache lookup failed"),
        }
        if self.contact.is_none() {
            tried.push("Nominatim: not asked (set NOMINATIM_CONTACT or WIKIMEDIA_CONTACT: its policy requires a contact)");
            return not_found(place, &tried);
        }
        match self.nominatim(place).await {
            Ok(c) => {
                if let Err(e) = self.remember(&key, place, &c).await {
                    warn!(error = %e, "geocode: answer not cached");
                }
                info!(place, found = !c.is_empty(), "geocode: asked Nominatim");
                if c.is_empty() {
                    tried.push("Nominatim (OpenStreetMap): no match");
                    not_found(place, &tried)
                } else {
                    found(place, "nominatim", c)
                }
            }
            Err(e) => {
                ToolCallResult::error(format!("geocode: {e}. Tried first: {}.", tried.join("; ")))
            }
        }
    }
}

fn found(place: &str, source: &str, cands: Vec<Value>) -> ToolCallResult {
    let note = if cands.len() > 1 {
        format!(
            "{} candidates; the first is OpenStreetMap's best match. If it is the wrong one, ask again with a more specific name (add the country or region).",
            cands.len()
        )
    } else {
        "One match.".to_string()
    };
    ToolCallResult::success_json(json!({
        "query": place, "found": true, "source": source,
        "place": cands[0], "candidates": cands, "note": note,
    }))
}

fn not_found(place: &str, tried: &[&str]) -> ToolCallResult {
    ToolCallResult::success_json(json!({
        "query": place, "found": false,
        "tried": tried,
        "note": "Not found. Try the place's official or English name, or a larger place around it.",
    }))
}

#[async_trait]
impl Skill for GeocodeSkill {
    fn name(&self) -> &str {
        "Maps"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        (tool_name == "geocode").then_some(())?;
        Some(self.handle(arguments).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_ignores_case_and_spacing() {
        assert_eq!(key_of("  Strait   of\tHormuz "), "strait of hormuz");
    }

    #[test]
    fn nominatim_hits_become_places_best_first() {
        let body = r#"[
          {"lat":"26.5667","lon":"56.25","name":"Strait of Hormuz","display_name":"Strait of Hormuz, Iran",
           "class":"natural","type":"strait","osm_type":"relation","osm_id":123,
           "boundingbox":["25.9","27.2","55.6","57.0"]},
          {"lat":"bad","lon":"1"},
          {"lat":"27.1","lon":"56.3","name":"","display_name":"Hormuz Island, Iran","category":"place","type":"island"},
          {"lat":"1","lon":"2"},
          {"lat":"3","lon":"4"}]"#;
        let p = parse_nominatim(body).unwrap();
        assert_eq!(p.len(), 3, "an unreadable hit is skipped, at most 3 kept");
        assert_eq!(p[0]["name"], "Strait of Hormuz");
        assert_eq!(p[0]["lat"], 26.5667);
        assert_eq!(p[0]["kind"], "natural:strait");
        assert_eq!(p[0]["osm"], "relation/123");
        assert_eq!(p[0]["bbox"], json!([25.9, 27.2, 55.6, 57.0]));
        // An empty name falls back to the display name.
        assert_eq!(p[1]["name"], "Hormuz Island, Iran");
        // jsonv2 calls the class `category`.
        assert_eq!(p[1]["kind"], "place:island");
        assert!(
            parse_nominatim("{\"error\":\"x\"}")
                .unwrap_err()
                .contains("something else")
        );
        assert_eq!(parse_nominatim("[]").unwrap().len(), 0);
    }
}
