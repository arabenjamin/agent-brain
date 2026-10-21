//! Watch areas (MAP_PLAN Phase 4): `areas/*.yaml` in any loaded bundle. An
//! area is a box the collectors watch for ships (aisstream) and aircraft
//! (OpenSky). What the owner watches can reveal an interest, so the NIA's
//! areas live in its own private bundle; the maps plugin ships none.

use serde::Deserialize;

use crate::layers::Owner;

/// OpenSky charges 1 credit for a box under 25 square degrees, 2 up to 100,
/// more above (measured 2026-10-21): an aircraft area stays at 100 or under.
pub const MAX_AIRCRAFT_SQ_DEG: f64 = 100.0;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Area {
    /// Equals the file name: lowercase letters, digits and hyphens.
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub owner: Option<Owner>,
    /// `[min_lon, min_lat, max_lon, max_lat]`, WGS 84.
    pub bbox: [f64; 4],
    /// Collect ship positions here (aisstream.io).
    #[serde(default)]
    pub ships: bool,
    /// Collect aircraft positions here (OpenSky).
    #[serde(default)]
    pub aircraft: bool,
}

impl Area {
    pub fn parse(text: &str) -> Result<Self, String> {
        let a: Area = serde_yaml::from_str(text).map_err(|e| format!("YAML: {e}"))?;
        a.check()?;
        Ok(a)
    }

    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
        let a = Self::parse(&text)?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if a.name != stem {
            return Err(format!(
                "name '{}' must equal the file name '{stem}'",
                a.name
            ));
        }
        Ok(a)
    }

    pub fn check(&self) -> Result<(), String> {
        if self.name.is_empty()
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(format!(
                "name '{}' must be lowercase letters, digits and hyphens",
                self.name
            ));
        }
        if self.title.trim().is_empty() {
            return Err("an area needs a title".into());
        }
        let [w, s, e, n] = self.bbox;
        if !(-180.0..=180.0).contains(&w)
            || !(-180.0..=180.0).contains(&e)
            || !(-90.0..=90.0).contains(&s)
            || !(-90.0..=90.0).contains(&n)
        {
            return Err("bbox is [min_lon, min_lat, max_lon, max_lat] in degrees".into());
        }
        if w >= e || s >= n {
            return Err(
                "bbox must have min_lon < max_lon and min_lat < max_lat (an area across the antimeridian is two areas)"
                    .into(),
            );
        }
        if !self.ships && !self.aircraft {
            return Err("an area watches ships, aircraft or both".into());
        }
        if self.aircraft && self.sq_deg() > MAX_AIRCRAFT_SQ_DEG {
            return Err(format!(
                "an aircraft area is at most {MAX_AIRCRAFT_SQ_DEG} square degrees (this is {:.0}): OpenSky charges more above it",
                self.sq_deg()
            ));
        }
        if let Some(o) = &self.owner {
            let set = [&o.program, &o.topic, &o.workflow]
                .iter()
                .filter(|v| v.as_deref().is_some_and(|s| !s.trim().is_empty()))
                .count();
            if set != 1 {
                return Err("owner names one of program, topic or workflow".into());
            }
        }
        Ok(())
    }

    pub fn sq_deg(&self) -> f64 {
        let [w, s, e, n] = self.bbox;
        (e - w) * (n - s)
    }

    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        let [w, s, e, n] = self.bbox;
        (s..=n).contains(&lat) && (w..=e).contains(&lon)
    }
}

/// Every loaded bundle's areas, and the files that did not load.
pub fn load_all(files: Vec<(String, std::path::PathBuf)>) -> (Vec<Area>, Vec<String>) {
    let mut ok: Vec<Area> = Vec::new();
    let mut bad = Vec::new();
    for (plugin, path) in files {
        match Area::load(&path) {
            Ok(a) if ok.iter().any(|o| o.name == a.name) => bad.push(format!(
                "{plugin}: {}: an area named '{}' is already loaded",
                path.display(),
                a.name
            )),
            Ok(a) => ok.push(a),
            Err(e) => bad.push(format!("{plugin}: {}: {e}", path.display())),
        }
    }
    (ok, bad)
}

/// The first area containing a point.
pub fn area_of(areas: &[Area], lat: f64, lon: f64) -> Option<&Area> {
    areas.iter().find(|a| a.contains(lat, lon))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(extra: &str) -> Result<Area, String> {
        Area::parse(&format!("name: hormuz\ntitle: Strait of Hormuz\n{extra}"))
    }

    #[test]
    fn an_area_is_a_valid_box_that_watches_something() {
        let a = area("bbox: [54.0, 24.0, 58.5, 27.5]\nships: true\naircraft: true\n").unwrap();
        assert!(a.contains(26.5, 56.3));
        assert!(!a.contains(12.0, 43.0));
        assert!((a.sq_deg() - 15.75).abs() < 1e-9);
        assert!(
            area("bbox: [54.0, 24.0, 58.5, 27.5]\n")
                .unwrap_err()
                .contains("watches")
        );
        assert!(
            area("bbox: [58.5, 24.0, 54.0, 27.5]\nships: true\n")
                .unwrap_err()
                .contains("min_lon")
        );
        assert!(area("bbox: [54.0, 24.0, 58.5, 97.5]\nships: true\n").is_err());
        // Ships may watch a big box; aircraft may not (OpenSky credits).
        assert!(area("bbox: [27.5, 30.0, 42.0, 47.0]\nships: true\n").is_ok());
        let e = area("bbox: [27.5, 30.0, 42.0, 47.0]\naircraft: true\n").unwrap_err();
        assert!(e.contains("square degrees"), "{e}");
    }
}

// ── In the graph (MAP_PLAN Phase 5a) ────────────────────────────────────────
//
// `(:WatchArea {name})`, unique by name. `managed_by: yaml` came from a
// bundle's file and is synced from it on start (all but `enabled`, which the
// map may switch off and which persists); `managed_by: map` was drawn on the
// map and is the map's to edit or delete.

use agent_brain_plugin_api::Graph;
use agent_brain_plugin_api::graph::query;
use serde_json::{Value, json};

/// An area as the graph holds it.
#[derive(Debug, Clone)]
pub struct WatchArea {
    pub area: Area,
    pub managed_by: String,
    pub enabled: bool,
}

const RETURN: &str = "RETURN a.name AS name, a.title AS title, a.description AS description, \
     a.bbox AS bbox, a.ships AS ships, a.aircraft AS aircraft, a.owner_program AS program, \
     a.owner_topic AS topic, a.owner_workflow AS workflow, a.managed_by AS managed_by, \
     a.enabled AS enabled";

fn from_row(r: &Value) -> Option<WatchArea> {
    let bbox: Vec<f64> = serde_json::from_value(r["bbox"].clone()).ok()?;
    let s = |k: &str| r[k].as_str().map(str::to_string);
    let owner =
        (s("program").is_some() || s("topic").is_some() || s("workflow").is_some()).then(|| {
            Owner {
                program: s("program"),
                topic: s("topic"),
                workflow: s("workflow"),
            }
        });
    Some(WatchArea {
        area: Area {
            name: s("name")?,
            title: s("title").unwrap_or_default(),
            description: s("description").unwrap_or_default(),
            owner,
            bbox: bbox.try_into().ok()?,
            ships: r["ships"].as_bool().unwrap_or(false),
            aircraft: r["aircraft"].as_bool().unwrap_or(false),
        },
        managed_by: s("managed_by").unwrap_or_else(|| "map".into()),
        enabled: r["enabled"].as_bool().unwrap_or(true),
    })
}

fn params(a: &Area) -> Value {
    let o = a.owner.as_ref();
    json!({
        "name": a.name, "title": a.title, "description": a.description, "bbox": a.bbox,
        "ships": a.ships, "aircraft": a.aircraft,
        "program": o.and_then(|o| o.program.clone()),
        "topic": o.and_then(|o| o.topic.clone()),
        "workflow": o.and_then(|o| o.workflow.clone()),
    })
}

const SET_FIELDS: &str = "a.title = $title, a.description = $description, a.bbox = $bbox, \
     a.ships = $ships, a.aircraft = $aircraft, a.owner_program = $program, \
     a.owner_topic = $topic, a.owner_workflow = $workflow, a.updated_at = datetime()";

pub async fn ensure_schema(g: &dyn Graph) -> anyhow::Result<()> {
    g.run(query(
        "CREATE CONSTRAINT watch_area_name IF NOT EXISTS FOR (a:WatchArea) REQUIRE a.name IS UNIQUE",
    ))
    .await
}

/// Sync the bundles' areas into the graph. Returns what could not be synced
/// (a name the map already uses).
pub async fn seed(g: &dyn Graph, areas: &[Area]) -> anyhow::Result<Vec<String>> {
    let mut clashes = vec![];
    for a in areas {
        let rows = g
            .query(
                &format!(
                    "MERGE (a:WatchArea {{name: $name}})
                     ON CREATE SET a.managed_by = 'yaml', a.enabled = true, a.created_at = datetime()
                     WITH a WHERE a.managed_by = 'yaml'
                     SET {SET_FIELDS}
                     RETURN a.name AS name"
                ),
                params(a),
            )
            .await?;
        if rows.is_empty() {
            clashes.push(format!(
                "areas/{}.yaml: an area drawn on the map has that name; the file is not used",
                a.name
            ));
        }
    }
    Ok(clashes)
}

/// Every area in the graph, enabled or not, by name.
pub async fn list(g: &dyn Graph) -> anyhow::Result<Vec<WatchArea>> {
    let rows = g
        .query(
            &format!("MATCH (a:WatchArea) {RETURN} ORDER BY a.name"),
            json!({}),
        )
        .await?;
    Ok(rows.iter().filter_map(from_row).collect())
}

/// Draw a new area (the map's).
pub async fn create(g: &dyn Graph, a: &Area) -> Result<(), String> {
    a.check()?;
    let rows = g
        .query(
            &format!(
                "OPTIONAL MATCH (x:WatchArea {{name: $name}})
                 WITH x WHERE x IS NULL
                 CREATE (a:WatchArea {{name: $name, managed_by: 'map', enabled: true, created_at: datetime()}})
                 SET {SET_FIELDS}
                 RETURN a.name AS name"
            ),
            params(a),
        )
        .await
        .map_err(|e| e.to_string())?;
    if rows.is_empty() {
        return Err(format!("an area named '{}' already exists", a.name));
    }
    Ok(())
}

/// Change a drawn area. An area from a file is changed in its file.
pub async fn update(g: &dyn Graph, a: &Area) -> Result<(), String> {
    a.check()?;
    let rows = g
        .query(
            &format!(
                "MATCH (a:WatchArea {{name: $name}}) WITH a, a.managed_by AS by
                 FOREACH (_ IN CASE WHEN by = 'map' THEN [1] ELSE [] END | SET {SET_FIELDS})
                 RETURN by"
            ),
            params(a),
        )
        .await
        .map_err(|e| e.to_string())?;
    match rows.first().and_then(|r| r["by"].as_str()) {
        None => Err(format!("no area named '{}'", a.name)),
        Some("map") => Ok(()),
        Some(_) => Err(format!(
            "'{}' comes from a bundle's areas/{}.yaml: change it there (from the map it can only be switched off)",
            a.name, a.name
        )),
    }
}

/// Switch an area on or off (any area; it persists over restarts).
pub async fn set_enabled(g: &dyn Graph, name: &str, enabled: bool) -> Result<(), String> {
    let rows = g
        .query(
            "MATCH (a:WatchArea {name: $name}) SET a.enabled = $enabled, a.updated_at = datetime() RETURN a.name AS name",
            json!({"name": name, "enabled": enabled}),
        )
        .await
        .map_err(|e| e.to_string())?;
    if rows.is_empty() {
        return Err(format!("no area named '{name}'"));
    }
    Ok(())
}

/// Delete a drawn area. An area from a file would come back on the next
/// start: it is switched off instead.
pub async fn delete(g: &dyn Graph, name: &str) -> Result<(), String> {
    let rows = g
        .query(
            "MATCH (a:WatchArea {name: $name}) WITH a, a.managed_by AS by
             FOREACH (_ IN CASE WHEN by = 'map' THEN [1] ELSE [] END | DETACH DELETE a)
             RETURN by",
            json!({"name": name}),
        )
        .await
        .map_err(|e| e.to_string())?;
    match rows.first().and_then(|r| r["by"].as_str()) {
        None => Err(format!("no area named '{name}'")),
        Some("map") => Ok(()),
        Some(_) => Err(format!(
            "'{name}' comes from a bundle's areas/{name}.yaml: switch it off instead (deleting it here would not last past a restart)"
        )),
    }
}

/// A name for a drawn area, from its title: lowercase letters, digits and
/// hyphens.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').chars().take(60).collect()
}

#[cfg(test)]
mod graph_tests {
    use super::*;

    #[test]
    fn a_title_becomes_a_name() {
        assert_eq!(slug("  Gulf of Aden (east) "), "gulf-of-aden-east");
        assert_eq!(slug("Île-de-France"), "le-de-france");
        assert_eq!(slug("!!!"), "");
    }

    #[test]
    fn a_graph_row_becomes_an_area() {
        let r = json!({"name": "x", "title": "X", "bbox": [1.0, 2.0, 3.0, 4.0], "ships": true,
                       "aircraft": false, "program": "nia", "managed_by": "yaml", "enabled": false});
        let w = from_row(&r).unwrap();
        assert_eq!(w.area.bbox, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(w.area.owner.unwrap().program.as_deref(), Some("nia"));
        assert!(!w.enabled);
        assert!(from_row(&json!({"name": "x", "bbox": [1.0]})).is_none());
    }
}
