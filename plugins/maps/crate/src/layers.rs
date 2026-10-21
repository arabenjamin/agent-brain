//! Map layers (MAP_PLAN Phase 3): `layers/*.yaml` in any loaded bundle. A
//! layer is a named, read-only query that returns located rows, and stores
//! its query, not its points, so it is always live. The HBI's Map panel
//! draws each one; a program, topic or workflow may own it.
//!
//! A layer's query returns `lat`, `lon` and `label`, and may return `id`
//! with `drawer` (what the HBI opens: `entity`, `note`), `kind`, `at` (an
//! ISO time), `detail`, and `weight` (a number the map sizes the point by). A `timed` layer takes `$since` and `$until`
//! (ISO strings or null; compare them as `datetime($since)`, since a
//! datetime compared to a string is null and drops the row). The engine adds
//! the LIMIT, one past the layer's own, so a cut is said, not hidden.
//!
//! Phase 5b, a map that follows the zoom: a `view` layer takes the map's box
//! (`$west $south $east $north`, all null for the whole world: test
//! `$west IS NULL` first), so a zoomed-in view loads only its own points;
//! `minzoom` hides a layer (no query at all) until the map is zoomed in that
//! far. The HBI clusters a layer's points when zoomed out unless `cluster:
//! false`.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{Value, json};

use agent_brain_plugin_api::Graph;

/// Rows a layer shows when it names no limit, and the most it may name.
const DEFAULT_LIMIT: usize = 1000;
const MAX_LIMIT: usize = 5000;

/// What a layer's query may write: nothing.
const WRITES: &[&str] = &[
    "CREATE", "MERGE", "SET", "DELETE", "DETACH", "REMOVE", "DROP", "FOREACH", "LOAD",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    #[serde(default)]
    pub program: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub workflow: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    /// Equals the file name: lowercase letters, digits and hyphens.
    pub name: String,
    pub title: String,
    pub description: String,
    /// The program, topic or workflow the layer belongs to (one of them).
    #[serde(default)]
    pub owner: Option<Owner>,
    /// `#rrggbb`.
    pub color: String,
    /// Its rows carry `at`, and it takes `$since` / `$until`.
    #[serde(default)]
    pub timed: bool,
    /// What it shows is the owner's own (hand-placed places): shown as such.
    #[serde(default)]
    pub private: bool,
    /// Drawn when the Map panel opens, until the viewer turns it off.
    #[serde(default = "yes")]
    pub on: bool,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Takes the map's box (`$west $south $east $north`).
    #[serde(default)]
    pub view: bool,
    /// Not drawn, and not queried, below this zoom.
    #[serde(default)]
    pub minzoom: Option<f64>,
    /// Points merge into count bubbles when zoomed out (default true).
    #[serde(default = "yes")]
    pub cluster: bool,
    pub cypher: String,
}

fn yes() -> bool {
    true
}

impl Layer {
    pub fn parse(text: &str) -> Result<Self, String> {
        let l: Layer = serde_yaml::from_str(text).map_err(|e| format!("YAML: {e}"))?;
        l.check()?;
        Ok(l)
    }

    /// Parse a file: its name must equal the file's stem.
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
        let l = Self::parse(&text)?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if l.name != stem {
            return Err(format!(
                "name '{}' must equal the file name '{stem}'",
                l.name
            ));
        }
        Ok(l)
    }

    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT)
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
        if self.title.trim().is_empty() || self.description.trim().is_empty() {
            return Err("a layer needs a title and a description".into());
        }
        let hex = self.color.strip_prefix('#').unwrap_or("");
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("color '{}' must be #rrggbb", self.color));
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
        if !(1..=MAX_LIMIT).contains(&self.limit()) {
            return Err(format!("limit must be 1 to {MAX_LIMIT}"));
        }
        let scan = scan(&self.cypher);
        if let Some(w) = scan.write {
            return Err(format!("a layer only reads: its cypher writes ({w})"));
        }
        let allowed: BTreeSet<&str> = ["since", "until", "west", "south", "east", "north"].into();
        if let Some(p) = scan.params.iter().find(|p| !allowed.contains(p.as_str())) {
            return Err(format!(
                "the cypher uses ${p}: a layer takes only $since, $until and the view ($west $south $east $north)"
            ));
        }
        let uses = scan.params.contains("since") && scan.params.contains("until");
        if self.timed && !uses {
            return Err("a timed layer filters on both $since and $until".into());
        }
        if !self.timed && (scan.params.contains("since") || scan.params.contains("until")) {
            return Err("$since and $until are for a timed layer (timed: true)".into());
        }
        let view_params = ["west", "south", "east", "north"]
            .iter()
            .filter(|p| scan.params.contains(**p))
            .count();
        if self.view && view_params != 4 {
            return Err("a view layer filters on all of $west, $south, $east and $north".into());
        }
        if !self.view && view_params > 0 {
            return Err("$west $south $east $north are for a view layer (view: true)".into());
        }
        if self.minzoom.is_some_and(|z| !(0.0..=22.0).contains(&z)) {
            return Err("minzoom is 0 to 22".into());
        }
        let ret = scan.last_return.as_deref().unwrap_or("");
        if ret.is_empty() {
            return Err("the cypher needs a RETURN".into());
        }
        if ret.split_whitespace().any(|w| w == "LIMIT") {
            return Err("leave LIMIT out: the engine adds it from `limit`".into());
        }
        for col in ["LAT", "LON", "LABEL"] {
            if !ret.contains(&format!("AS {col}")) {
                return Err(format!("the RETURN needs `AS {}`", col.to_lowercase()));
            }
        }
        if self.timed && !ret.contains("AS AT") {
            return Err("a timed layer returns `AS at`".into());
        }
        Ok(())
    }
}

/// What a query's text shows, outside string literals and comments.
struct Scan {
    /// The first write keyword, or a procedure `CALL`.
    write: Option<String>,
    params: BTreeSet<String>,
    /// The upper-cased words after the last `RETURN`.
    last_return: Option<String>,
}

fn scan(cypher: &str) -> Scan {
    // Words, upper-cased, with string literals and comments blanked.
    let mut words: Vec<(String, bool)> = Vec::new(); // (word, follows '.')
    let mut params = BTreeSet::new();
    let chars: Vec<char> = cypher.chars().collect();
    let mut i = 0;
    let mut prev_dot = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '"' || c == '`' {
            let q = c;
            i += 1;
            while i < chars.len() && chars[i] != q {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            prev_dot = false;
            words.push(("''".into(), false));
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '$' {
            let start = i + 1;
            i = start;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            params.insert(chars[start..i].iter().collect());
            prev_dot = false;
        } else if c.is_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let w: String = chars[start..i].iter().collect();
            words.push((w.to_uppercase(), prev_dot));
            prev_dot = false;
        } else {
            if !c.is_whitespace() {
                words.push((c.to_string(), false));
            }
            prev_dot = c == '.';
            i += 1;
        }
    }
    let mut write = None;
    for (k, (w, dotted)) in words.iter().enumerate() {
        if *dotted {
            continue; // a property: `n.set`
        }
        if WRITES.contains(&w.as_str()) {
            write = Some(w.clone());
            break;
        }
        // `CALL {` / `CALL (` is a subquery; `CALL db.labels()` a procedure.
        if w == "CALL" && !matches!(words.get(k + 1).map(|x| x.0.as_str()), Some("{" | "(")) {
            write = Some("CALL of a procedure".into());
            break;
        }
    }
    let last_return = words
        .iter()
        .rposition(|(w, d)| w == "RETURN" && !d)
        .map(|k| {
            words[k + 1..]
                .iter()
                .map(|(w, _)| w.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        });
    Scan {
        write,
        params,
        last_return,
    }
}

/// One layer's features, as GeoJSON, with what was cut or skipped.
pub async fn features(
    graph: &dyn Graph,
    layer: &Layer,
    since: Option<String>,
    until: Option<String>,
    view: Option<[f64; 4]>,
) -> anyhow::Result<Value> {
    let limit = layer.limit();
    let cypher = format!("{}\nLIMIT {}", layer.cypher.trim_end(), limit + 1);
    let mut params = serde_json::Map::new();
    if layer.timed {
        params.insert("since".into(), json!(since));
        params.insert("until".into(), json!(until));
    }
    if layer.view {
        let v = view.map(|v| v.map(Some)).unwrap_or([None; 4]);
        for (k, x) in ["west", "south", "east", "north"].into_iter().zip(v) {
            params.insert(k.into(), json!(x));
        }
    }
    let params = Value::Object(params);
    let rows = graph.query(&cypher, params).await?;
    Ok(collect(layer, rows, limit))
}

fn collect(layer: &Layer, rows: Vec<Value>, limit: usize) -> Value {
    let truncated = rows.len() > limit;
    let mut features = Vec::new();
    let mut skipped = 0;
    for r in rows.into_iter().take(limit) {
        let (Some(lat), Some(lon)) = (r["lat"].as_f64(), r["lon"].as_f64()) else {
            skipped += 1;
            continue;
        };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            skipped += 1;
            continue;
        }
        let mut props = json!({"label": r["label"].as_str().unwrap_or("(unnamed)")});
        for k in ["id", "drawer", "kind", "at", "detail"] {
            if !r[k].is_null() {
                props[k] = r[k].clone();
            }
        }
        if let Some(w) = r["weight"].as_f64() {
            props["weight"] = json!(w);
        }
        features.push(json!({
            "type": "Feature",
            "geometry": {"type": "Point", "coordinates": [lon, lat]},
            "properties": props,
        }));
    }
    let count = features.len();
    let note = if truncated {
        Some(format!(
            "Showing the first {limit}: the layer has more (its limit is {limit})."
        ))
    } else if count == 0 && skipped == 0 {
        Some(if layer.timed {
            "Nothing on this layer in the chosen time.".to_string()
        } else {
            "Nothing on this layer yet.".to_string()
        })
    } else {
        None
    };
    json!({
        "type": "FeatureCollection",
        "layer": layer.name,
        "features": features,
        "count": count,
        "skipped": skipped,
        "truncated": truncated,
        "note": note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(extra: &str, cypher: &str) -> Result<Layer, String> {
        let text = format!(
            "name: test\ntitle: T\ndescription: D\ncolor: \"#aabbcc\"\n{extra}cypher: |\n  {}\n",
            cypher.replace('\n', "\n  ")
        );
        Layer::parse(&text)
    }

    const OK: &str =
        "MATCH (p:Place) RETURN p.location.y AS lat, p.location.x AS lon, p.name AS label";

    #[test]
    fn a_layer_only_reads() {
        assert!(layer("", OK).is_ok());
        for bad in [
            "MATCH (p:Place) SET p.x = 1 RETURN 1 AS lat, 1 AS lon, '' AS label",
            "MATCH (p:Place) DETACH DELETE p RETURN 1 AS lat, 1 AS lon, '' AS label",
            "CALL db.labels() YIELD label RETURN 1 AS lat, 1 AS lon, label AS label",
            "MERGE (p:Place {key:'x'}) RETURN 1 AS lat, 1 AS lon, '' AS label",
        ] {
            let e = layer("", bad).unwrap_err();
            assert!(e.contains("only reads"), "{bad}: {e}");
        }
        // A property or a string that spells a keyword is not a write, and a
        // subquery CALL is not a procedure.
        assert!(
            layer(
                "",
                "MATCH (p:Place) WHERE p.set = 'DELETE me' CALL { RETURN 1 AS x } RETURN p.location.y AS lat, p.location.x AS lon, p.name AS label"
            )
            .is_ok()
        );
    }

    #[test]
    fn a_layer_returns_its_columns_and_takes_only_the_time_params() {
        let e = layer("", "MATCH (p:Place) RETURN p.name AS label").unwrap_err();
        assert!(e.contains("AS lat"), "{e}");
        let e = layer("", &format!("{OK} LIMIT 5")).unwrap_err();
        assert!(e.contains("LIMIT"), "{e}");
        let e = layer(
            "",
            "MATCH (p:Place {key: $key}) RETURN 1 AS lat, 1 AS lon, '' AS label",
        )
        .unwrap_err();
        assert!(e.contains("$key"), "{e}");
        let timed = "MATCH (n:Note) WHERE ($since IS NULL OR n.created_at >= datetime($since)) AND ($until IS NULL OR n.created_at < datetime($until)) RETURN 1 AS lat, 1 AS lon, '' AS label, toString(n.created_at) AS at";
        assert!(layer("timed: true\n", timed).is_ok());
        assert!(layer("", timed).unwrap_err().contains("timed"));
        assert!(layer("timed: true\n", OK).unwrap_err().contains("$since"));
    }

    #[test]
    fn a_view_layer_takes_the_whole_box() {
        let v = "MATCH (p:Place) WHERE $west IS NULL OR point.withinBBox(p.location, point({longitude: $west, latitude: $south}), point({longitude: $east, latitude: $north})) RETURN p.location.y AS lat, p.location.x AS lon, p.name AS label";
        assert!(layer("view: true\nminzoom: 4\n", v).is_ok());
        assert!(layer("", v).unwrap_err().contains("view: true"));
        assert!(layer("view: true\n", OK).unwrap_err().contains("all of"));
        let three = v.replace("latitude: $north", "latitude: 90");
        assert!(
            layer("view: true\n", &three)
                .unwrap_err()
                .contains("all of")
        );
        assert!(layer("minzoom: 30\n", OK).unwrap_err().contains("minzoom"));
    }

    #[test]
    fn a_layer_names_one_owner_and_a_colour() {
        assert!(layer("owner: {program: nia}\n", OK).is_ok());
        assert!(layer("owner: {program: nia, topic: x}\n", OK).is_err());
        let bad = Layer::parse(&format!(
            "name: test\ntitle: T\ndescription: D\ncolor: red\ncypher: \"{OK}\"\n"
        ));
        assert!(bad.unwrap_err().contains("#rrggbb"));
    }

    #[test]
    fn a_cut_and_a_skipped_row_are_said() {
        let l = layer("", OK).unwrap();
        let rows = vec![
            json!({"lat": 42.3, "lon": -83.0, "label": "Detroit", "id": "e1", "drawer": "entity", "weight": 4}),
            json!({"lat": null, "lon": -83.0, "label": "no point"}),
            json!({"lat": 95.0, "lon": 0.0, "label": "off the earth"}),
            json!({"lat": 1.0, "lon": 1.0, "label": "one too many"}),
        ];
        let v = collect(&l, rows, 3);
        assert_eq!(v["count"], 1);
        assert_eq!(v["skipped"], 2);
        assert_eq!(v["truncated"], true);
        assert!(v["note"].as_str().unwrap().contains("first 3"));
        let f = &v["features"][0];
        assert_eq!(f["geometry"]["coordinates"], json!([-83.0, 42.3]));
        assert_eq!(f["properties"]["drawer"], "entity");
        assert_eq!(f["properties"]["weight"], 4.0);
        let empty = collect(&l, vec![], 3);
        assert_eq!(empty["note"], "Nothing on this layer yet.");
    }

    #[test]
    fn shipped_layers_parse() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../layers");
        let mut n = 0;
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            Layer::load(&e.path()).unwrap_or_else(|err| panic!("{}: {err}", e.path().display()));
            n += 1;
        }
        assert!(n >= 4, "expected the shipped layers, found {n}");
    }
}
