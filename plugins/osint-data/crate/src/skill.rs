//! ACLED skill — live conflict-event data (`acled_events`).
//!
//! Registered only when `ACLED_EMAIL` and `ACLED_PASSWORD` are set: a tool
//! that can only fail would cost every caller a wasted call. Licence and
//! storage rules live in `client.rs`.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{AcledClient, EVENT_TYPES, EventQuery, MAX_LIMIT, summarize_events};
use agent_brain_plugin_api::{PluginContext, Skill, ToolCallResult, ToolDefinition, parse_args};

/// Sample rows returned when the caller names no `sample`.
const DEFAULT_SAMPLE: usize = 15;

pub struct AcledSkill {
    client: Arc<AcledClient>,
}

#[derive(Debug, Deserialize)]
struct EventsInput {
    #[serde(default, deserialize_with = "string_or_vec")]
    country: Vec<String>,
    #[serde(default)]
    event_type: Option<String>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    admin1: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    days: Option<i64>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    sample: Option<usize>,
}

/// `"Ukraine"` or `["Sudan", "Chad"]` — models send either.
fn string_or_vec<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Value::deserialize(d)? {
        Value::String(s) => vec![s],
        Value::Array(a) => a
            .into_iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => vec![],
    })
}

fn parse_date(field: &str, v: Option<&str>) -> Result<Option<NaiveDate>, String> {
    match v.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map(Some)
            .map_err(|_| format!("`{field}` must be YYYY-MM-DD, got {s:?}")),
    }
}

impl AcledSkill {
    pub fn from_ctx(ctx: &PluginContext) -> Option<Self> {
        AcledClient::from_ctx(ctx).map(|c| Self {
            client: Arc::new(c),
        })
    }

    fn events_def() -> ToolDefinition {
        ToolDefinition {
            name: "acled_events".to_string(),
            description: format!(
                "Query ACLED, the coded record of political violence and protest worldwide \
                 (who, where, when, what kind, fatalities), live. Returns counts by country, \
                 event type, region (admin1), actor and week, plus a newest-first sample of \
                 events with ACLED's own notes and sources. Use it to check an event claim \
                 (did fighting/strikes/protests happen there, then, by whom?) or to measure a \
                 trend. Every bucket can be drilled into by re-querying with its value. Data \
                 lags about a week. event_type is one of: {}. Cite ACLED; never store the \
                 raw rows.",
                EVENT_TYPES.join(", ")
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "country": {
                        "description": "Country name(s) in ACLED spelling, e.g. \"Ukraine\" or [\"Sudan\", \"Chad\"]. Omit for worldwide.",
                        "oneOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]
                    },
                    "event_type": {"type": "string", "enum": EVENT_TYPES},
                    "actor": {"type": "string", "description": "Primary actor (actor1), partial match, e.g. \"Wagner\""},
                    "admin1": {"type": "string", "description": "First-level region, partial match, e.g. \"Donetsk\""},
                    "location": {"type": "string", "description": "Town or place, partial match"},
                    "since": {"type": "string", "description": "Start date YYYY-MM-DD (default: `days` before `until`)"},
                    "until": {"type": "string", "description": "End date YYYY-MM-DD (default: today)"},
                    "days": {"type": "integer", "minimum": 1, "maximum": 3650, "description": "Window length when `since` is not given (default 30)"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "description": "Max rows fetched (default 500); a full page is reported as truncated"},
                    "sample": {"type": "integer", "minimum": 0, "maximum": 50, "description": "Sample events returned (default 15)"}
                }
            }),
        }
    }

    async fn handle_events(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: EventsInput = match parse_args(arguments) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let until = match parse_date("until", input.until.as_deref()) {
            Ok(d) => d,
            Err(e) => return ToolCallResult::error(e),
        };
        let mut since = match parse_date("since", input.since.as_deref()) {
            Ok(d) => d,
            Err(e) => return ToolCallResult::error(e),
        };
        let today = chrono::Utc::now().date_naive();
        if since.is_none()
            && let Some(days) = input.days
        {
            since = Some(until.unwrap_or(today) - chrono::Duration::days(days.clamp(1, 3650)));
        }
        let q = EventQuery {
            countries: input.country,
            event_type: input.event_type,
            actor: input.actor,
            admin1: input.admin1,
            location: input.location,
            since,
            until,
            limit: input.limit,
        };
        match self.client.events(&q).await {
            Ok(rows) => ToolCallResult::success_json(summarize_events(
                &rows,
                &q,
                today,
                input.sample.unwrap_or(DEFAULT_SAMPLE).min(50),
            )),
            Err(e) => ToolCallResult::error(format!("ACLED query failed: {e}")),
        }
    }
}

#[async_trait]
impl Skill for AcledSkill {
    fn name(&self) -> &str {
        "ACLED"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::events_def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "acled_events" => Some(self.handle_events(arguments).await),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_must_be_iso() {
        assert_eq!(parse_date("since", None), Ok(None));
        assert_eq!(parse_date("since", Some(" ")), Ok(None));
        assert!(parse_date("since", Some("2026-09-01")).unwrap().is_some());
        assert!(
            parse_date("since", Some("Sept 1"))
                .unwrap_err()
                .contains("YYYY-MM-DD")
        );
    }

    #[test]
    fn a_single_country_string_is_accepted() {
        let i: EventsInput = serde_json::from_value(json!({"country": "Ukraine"})).unwrap();
        assert_eq!(i.country, vec!["Ukraine".to_string()]);
        let i: EventsInput = serde_json::from_value(json!({"country": ["Sudan", "Chad"]})).unwrap();
        assert_eq!(i.country.len(), 2);
        let i: EventsInput = serde_json::from_value(json!({})).unwrap();
        assert!(i.country.is_empty());
    }
}
