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
