//! `area_traffic`: what the collectors saw in a watch area (MAP_PLAN Phase
//! 4a). Every count comes with its coverage, so "no ships" is never read as
//! "quiet": the networks are crowdsourced receivers, and an area with none
//! (the Red Sea, measured 2026-10-21) hears nothing at all.

use agent_brain_plugin_api::{Skill, ToolCallResult, ToolDefinition, async_trait, parse_args};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Tracks, now};

const RECENT: usize = 15;

pub struct AreaTrafficSkill {
    tracks: Tracks,
}

impl AreaTrafficSkill {
    pub fn new(tracks: Tracks) -> Self {
        Self { tracks }
    }
}

fn iso(t: i64) -> String {
    DateTime::<Utc>::from_timestamp(t, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%MZ").to_string())
        .unwrap_or_default()
}

/// What a source's coverage of a window says: `(state, says)`.
pub fn judge(what: &str, window_secs: i64, up_secs: i64, reports: i64) -> (&'static str, String) {
    let hours = |s: i64| format!("{:.1}", s as f64 / 3600.0);
    if up_secs <= 0 {
        return (
            "not collected",
            format!(
                "No {what} were collected here in this window: the collector was off or did not watch this area. Nothing can be said about traffic."
            ),
        );
    }
    // Silence over a few minutes says nothing yet: give it an hour, or a
    // quarter of the window, before calling an area unseen.
    if reports == 0 && up_secs < 3600_i64.min(window_secs / 4).max(600) {
        return (
            "too early",
            format!(
                "Collected for only {} min and heard nothing yet: too little to tell an unseen area from a quiet one.",
                up_secs / 60
            ),
        );
    }
    if reports == 0 {
        return (
            "no coverage",
            format!(
                "Collected for {} h and heard nothing. These networks are crowdsourced receivers and none report from here: zero is NOT quiet, it is unseen.",
                hours(up_secs)
            ),
        );
    }
    if up_secs * 10 < window_secs * 8 {
        return (
            "partial",
            format!(
                "Collected for {} of {} h: the counts cover only that time.",
                hours(up_secs),
                hours(window_secs)
            ),
        );
    }
    (
        "covered",
        "Collected for the whole window. Counts are what the receivers saw: a vessel or aircraft with its transponder off is not in them."
            .to_string(),
    )
}

#[derive(Deserialize)]
struct Input {
    #[serde(default)]
    area: String,
    #[serde(default = "day")]
    hours: i64,
}

fn day() -> i64 {
    24
}

impl AreaTrafficSkill {
    fn def() -> ToolDefinition {
        ToolDefinition {
            name: "area_traffic".to_string(),
            description: "Ships (AIS) and aircraft (ADS-B) the brain's collectors saw in a watch \
                          area: how many distinct, by class (tanker, cargo, military, …; heavy, \
                          rotorcraft, …), and the most recently seen with their last position. \
                          Every count carries its COVERAGE: 'no coverage' means no receivers \
                          report from there (zero is not quiet), 'not collected' means the \
                          collector was off. Call with no area to list the watch areas and the \
                          collectors' state."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "area": {"type": "string", "description": "A watch area's name (call with none to list them)."},
                    "hours": {"type": "integer", "minimum": 1, "maximum": 336, "default": 24, "description": "How far back, in hours (at most 14 days, the retention)."}
                }
            }),
        }
    }

    async fn handle(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: Input = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };
        if !(1..=336).contains(&input.hours) {
            return ToolCallResult::error(
                "hours must be 1 to 336 (14 days, the retention)".to_string(),
            );
        }
        let store = match &self.tracks.store {
            Ok(s) => s.clone(),
            Err(e) => return ToolCallResult::error(e.clone()),
        };
        let all = match self.tracks.all_areas().await {
            Ok(a) => a,
            Err(e) => return ToolCallResult::error(e),
        };
        let problems = self.tracks.problems();
        let collectors = self.tracks.status.snapshot();
        let area = input.area.trim().to_lowercase();
        if area.is_empty() {
            let list: Vec<Value> = all
                .iter()
                .map(|w| {
                    let a = &w.area;
                    json!({
                        "name": a.name, "title": a.title, "description": a.description,
                        "ships": a.ships, "aircraft": a.aircraft, "bbox": a.bbox,
                        "on": w.enabled, "from": if w.managed_by == "yaml" { "a bundle file" } else { "drawn on the map" },
                    })
                })
                .collect();
            return ToolCallResult::success_json(json!({
                "areas": list,
                "note": if list.is_empty() { Some("No watch areas: draw one on the HBI's Map, or ship one in a bundle's areas/.") } else { None },
                "collectors": collectors,
                "problems": problems,
            }));
        }
        let Some(w) = all.iter().find(|w| w.area.name == area).cloned() else {
            let names: Vec<&str> = all.iter().map(|w| w.area.name.as_str()).collect();
            return ToolCallResult::error(format!(
                "no watch area named '{area}'; the areas are: {}",
                if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join(", ")
                }
            ));
        };
        let a = w.area;
        let window = input.hours * 3600;
        let since = now() - window;
        let mut out = Map::new();
        if !w.enabled {
            out.insert(
                "note".into(),
                json!("This area is switched off: nothing new is collected; counts cover what was collected before."),
            );
        }
        out.insert("area".into(), json!(a.name));
        out.insert("title".into(), json!(a.title));
        out.insert(
            "window".into(),
            json!({"hours": input.hours, "since": iso(since)}),
        );
        for (source, what, on) in [("ais", "ships", a.ships), ("adsb", "aircraft", a.aircraft)] {
            if !on {
                out.insert(what.into(), json!({"watched": false}));
                continue;
            }
            let name = a.name.clone();
            let res = store
                .blocking(move |s| {
                    let cov = s.coverage(source, &name, since)?;
                    let classes = s.by_class(source, &name, since)?;
                    let recent = s.latest(Some(source), Some(&name), since, None, RECENT)?;
                    Ok((cov, classes, recent))
                })
                .await;
            let ((_, up, reports), (distinct, classes), recent) = match res {
                Ok(v) => v,
                Err(e) => return ToolCallResult::error(format!("the tracks store failed: {e}")),
            };
            let (state, says) = judge(what, window, up, reports);
            let recent: Vec<Value> = recent
                .into_iter()
                .map(|mut r| {
                    let at = r["at"].as_i64().unwrap_or(0);
                    if let Some(o) = r.as_object_mut() {
                        o.remove("at");
                        o.remove("area");
                        o.remove("source");
                        o.retain(|_, v| !v.is_null());
                        o.insert("last_seen".into(), json!(iso(at)));
                    }
                    r
                })
                .collect();
            out.insert(
                what.into(),
                json!({
                    "coverage": {"state": state, "says": says, "hours_collected": (up as f64 / 360.0).round() / 10.0, "reports": reports},
                    "distinct": distinct,
                    "by_class": classes.into_iter().map(|(k, n)| json!({"class": k, "distinct": n})).collect::<Vec<_>>(),
                    "recent": recent,
                    "recent_shown": format!("{} most recently seen of {distinct}", recent.len()),
                }),
            );
        }
        out.insert("collectors".into(), collectors);
        ToolCallResult::success_json(Value::Object(out))
    }
}

#[async_trait]
impl Skill for AreaTrafficSkill {
    fn name(&self) -> &str {
        "area_traffic"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        (tool_name == "area_traffic").then_some(self.handle(arguments).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_says_unseen_not_quiet() {
        assert_eq!(judge("ships", 86_400, 0, 0).0, "not collected");
        assert_eq!(judge("ships", 86_400, 120, 0).0, "too early");
        assert_eq!(
            judge("ships", 3600, 1200, 0).0,
            "no coverage",
            "a quarter of a short window is enough"
        );
        let (s, says) = judge("ships", 86_400, 86_400, 0);
        assert_eq!(s, "no coverage");
        assert!(says.contains("NOT quiet"), "{says}");
        assert_eq!(judge("ships", 86_400, 36_000, 50).0, "partial");
        assert_eq!(judge("ships", 86_400, 80_000, 50).0, "covered");
    }
}
