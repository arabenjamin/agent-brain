//! Dataset skill — ETL-loaded external datasets (GDELT, ReliefWeb).
//!
//! `dataset_sync` runs one extract → transform → load pass (driven by
//! `schedules/gdelt-sync.yaml` and `schedules/reliefweb-sync.yaml`);
//! `gdelt_events` and `reliefweb_reports` query what is loaded;
//! `dataset_status` is the review view — freshness, failures, and who the
//! data came from. Registered when the datasets DuckDB file opens.

use async_trait::async_trait;
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::repository::{DatasetStore, Neo4jClient};
use crate::services::datasets::{
    self, SourceClasses,
    gdelt::{self, GdeltConfig, GdeltQuery},
    reliefweb::{self, ReliefwebConfig, ReliefwebQuery},
};
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};

pub struct DatasetSkill {
    store: DatasetStore,
    neo4j: Option<Neo4jClient>,
    http: reqwest::Client,
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

/// `since`/`until`/`days` → a window; `days` counts back from `until`.
fn window(
    since: Option<&str>,
    until: Option<&str>,
    days: Option<i64>,
    today: NaiveDate,
) -> Result<(Option<NaiveDate>, Option<NaiveDate>), String> {
    let until = parse_date("until", until)?;
    let mut since = parse_date("since", since)?;
    if since.is_none()
        && let Some(d) = days
    {
        since = Some(until.unwrap_or(today) - chrono::Duration::days(d.clamp(1, 3650)));
    }
    Ok((since, until))
}

#[derive(Debug, Deserialize)]
struct SyncInput {
    dataset: String,
}

#[derive(Debug, Deserialize, Default)]
struct StatusInput {
    #[serde(default)]
    dataset: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GdeltInput {
    #[serde(default, deserialize_with = "string_or_vec")]
    country: Vec<String>,
    #[serde(default, deserialize_with = "string_or_vec")]
    event_type: Vec<String>,
    #[serde(default)]
    quad_class: Option<u8>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    days: Option<i64>,
    #[serde(default)]
    min_sources: Option<i64>,
    #[serde(default)]
    root_only: Option<bool>,
    #[serde(default)]
    sample: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ReliefwebInput {
    #[serde(default, deserialize_with = "string_or_vec")]
    country: Vec<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    disaster: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    days: Option<i64>,
    #[serde(default)]
    limit: Option<usize>,
}

impl DatasetSkill {
    /// `None` when `DATASETS_DB_PATH` is unset or the file cannot be opened.
    ///
    /// The store is opened once per process: `build_skills` re-runs on a model
    /// hot-swap, and a second `Connection::open` of the same DuckDB file in
    /// one process fails on the file lock.
    pub fn from_env(neo4j: Option<Neo4jClient>) -> Option<Self> {
        static STORE: std::sync::OnceLock<Option<DatasetStore>> = std::sync::OnceLock::new();
        let store = STORE.get_or_init(|| {
            // No default path: unset means "no datasets here" (and keeps
            // tests that build the registry from creating a file in cwd).
            let path = std::env::var("DATASETS_DB_PATH")
                .ok()
                .filter(|p| !p.trim().is_empty())?;
            DatasetStore::new(&path)
                .map_err(|e| {
                    tracing::warn!(path = %path, error = %e,
                        "datasets store unavailable — dataset tools not registered")
                })
                .ok()
        });
        store.clone().map(|store| Self {
            store,
            neo4j,
            http: datasets::http_client(),
        })
    }

    fn defs() -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "gdelt_events".into(),
                description: "Query GDELT, the machine-coded record of events in world news \
                    (who did what to whom, where), loaded every 15 minutes and kept 90 days. \
                    Returns counts by event type, country, day and actor, the SOURCE MIX (which \
                    outlets: osint/news lists, state media, platforms, unlisted), and a sample of \
                    the most-corroborated events with their article URLs. Counts measure news \
                    coverage, not ground truth, and most events rest on one article (the answer \
                    says what share were multi-source; min_sources: 2 keeps only those). Good for: is coverage of fighting/protest in X rising? who is reported \
                    acting against whom? Check a sample URL before relying on a number."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "country": {"description": "Country name(s) in English, e.g. \"Ukraine\" or [\"Sudan\", \"Chad\"]",
                                    "oneOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]},
                        "event_type": {"description": "CAMEO root event(s): code 01-20 or name, e.g. \"fight\", \"protest\", \"assault\", \"threaten\"",
                                       "oneOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]},
                        "quad_class": {"type": "integer", "minimum": 1, "maximum": 4,
                                       "description": "1 verbal cooperation, 2 material cooperation, 3 verbal conflict, 4 material conflict"},
                        "actor": {"type": "string", "description": "Actor name, partial match on either side, e.g. \"WAGNER\""},
                        "since": {"type": "string", "description": "YYYY-MM-DD (default: `days` before `until`)"},
                        "until": {"type": "string", "description": "YYYY-MM-DD (default: today)"},
                        "days": {"type": "integer", "minimum": 1, "maximum": 90, "description": "Window length (default 7)"},
                        "min_sources": {"type": "integer", "minimum": 1, "description": "Minimum outlets reporting an event when coded (default 1; 2 keeps the ~3% multi-source events)"},
                        "root_only": {"type": "boolean", "description": "Only events from an article's lead paragraph"},
                        "sample": {"type": "integer", "minimum": 0, "maximum": 50, "description": "Sample events (default 12)"}
                    }
                }),
            },
            ToolDefinition {
                name: "reliefweb_reports".into(),
                description: "Search ReliefWeb (UN OCHA) humanitarian reports loaded locally: \
                    situation reports, assessments, maps and news from UN agencies, governments \
                    and NGOs. Returns counts by country, publisher, publisher type, format and \
                    theme, and the newest matching reports with links and excerpts. Cite the \
                    report's publisher. Good for: what is the humanitarian situation in X, who is \
                    reporting it, displacement, food insecurity, epidemics, disasters."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "country": {"description": "Country name(s), partial match",
                                    "oneOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]},
                        "query": {"type": "string", "description": "Words that must all appear in the title or excerpt"},
                        "theme": {"type": "string", "description": "e.g. \"Food and Nutrition\", \"Protection and Human Rights\""},
                        "source": {"type": "string", "description": "Publisher short name, e.g. \"OCHA\", \"UNHCR\", \"WFP\""},
                        "disaster": {"type": "string", "description": "Disaster name, partial match"},
                        "format": {"type": "string", "description": "e.g. \"Situation Report\", \"Assessment\", \"Map\", \"News and Press Release\""},
                        "since": {"type": "string", "description": "YYYY-MM-DD"},
                        "until": {"type": "string", "description": "YYYY-MM-DD"},
                        "days": {"type": "integer", "minimum": 1, "maximum": 365, "description": "Window length (default 30)"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Reports listed (default 15)"}
                    }
                }),
            },
            ToolDefinition {
                name: "dataset_status".into(),
                description: "Review the ETL-loaded datasets (gdelt, reliefweb): rows and date \
                    coverage, the last runs and any errors, ReliefWeb calls used today, and who \
                    the last 7 days of data came from (GDELT source classes and top unlisted \
                    outlets; ReliefWeb publishers and publisher types)."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"dataset": {"type": "string", "enum": ["gdelt", "reliefweb"], "description": "Omit for both"}}
                }),
            },
            ToolDefinition {
                name: "dataset_sync".into(),
                description: "Run one ETL pass for a dataset (normally scheduled): download what \
                    is new since the last load, load it, prune past retention, and log the run."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"dataset": {"type": "string", "enum": ["gdelt", "reliefweb"]}},
                    "required": ["dataset"]
                }),
            },
        ]
    }

    /// ReliefWeb's state, stated: paused, never run, failing, or its newest
    /// report's age. Paused is read from the schedule, so a pause is never
    /// mistaken for an outage.
    async fn reliefweb_freshness(&self) -> Value {
        let enabled = self.schedule_enabled("ReliefWeb sync").await;
        let newest = self.store.reliefweb_newest().ok().flatten();
        let last_error = self
            .store
            .query_json(
                "SELECT error FROM etl_runs WHERE dataset = 'reliefweb' ORDER BY finished_at DESC LIMIT 1",
                &[],
            )
            .ok()
            .and_then(|r| r.first().and_then(|row| row["error"].as_str().map(String::from)));
        let (state, summary) = match (enabled, &newest, &last_error) {
            (Some(false), _, _) => (
                "paused",
                "Paused: the ReliefWeb sync schedule is disabled (see schedules/reliefweb-sync.yaml for why).".to_string(),
            ),
            (_, _, Some(e)) => ("failing", format!("Last run failed: {e}")),
            (_, None, None) => ("never loaded", "No ReliefWeb report has been loaded yet.".to_string()),
            (_, Some(n), None) => ("loaded", format!("Newest report created {n}.")),
        };
        json!({"state": state, "schedule_enabled": enabled, "newest_report_utc": newest, "summary": summary})
    }

    /// Whether the sync schedule whose name starts with `prefix` is enabled
    /// (`None` without Neo4j or schedule).
    async fn schedule_enabled(&self, prefix: &str) -> Option<bool> {
        let n = self.neo4j.as_ref()?;
        n.execute(
            neo4rs::query(
                "MATCH (t:ScheduledTask) WHERE t.name STARTS WITH $p RETURN t.enabled AS enabled",
            )
            .param("p", prefix),
        )
        .await
        .ok()
        .and_then(|rows| rows.first().and_then(|r| r.get::<bool>("enabled").ok()))
    }

    async fn classes(&self) -> SourceClasses {
        SourceClasses::load(self.neo4j.as_ref()).await
    }

    async fn handle_sync(&self, args: Option<Value>) -> ToolCallResult {
        let input: SyncInput = match parse_args(args) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let report = match input.dataset.as_str() {
            "gdelt" => match datasets::load_config::<GdeltConfig>("gdelt") {
                Ok(cfg) => gdelt::sync(&self.store, &cfg, &self.http).await,
                Err(e) => return ToolCallResult::error(format!("{e:#}")),
            },
            "reliefweb" => match datasets::load_config::<ReliefwebConfig>("reliefweb") {
                Ok(cfg) => reliefweb::sync(&self.store, &cfg, &self.http).await,
                Err(e) => return ToolCallResult::error(format!("{e:#}")),
            },
            other => {
                return ToolCallResult::error(format!(
                    "unknown dataset {other:?}: use gdelt or reliefweb"
                ));
            }
        };
        // A failed run is a failed call: the scheduler records it and the
        // review shows it, instead of a success that loaded nothing.
        if report["error"].is_string()
            && report["rows_loaded"].as_i64().unwrap_or(0) == 0
            && report["reports_loaded"].as_i64().unwrap_or(0) == 0
        {
            return ToolCallResult::error(report.to_string());
        }
        ToolCallResult::success_json(report)
    }

    fn q(&self, sql: &str) -> Value {
        match self.store.query_json(sql, &[]) {
            Ok(rows) => json!(rows),
            Err(e) => json!({"error": format!("{e:#}")}),
        }
    }

    async fn handle_status(&self, args: Option<Value>) -> ToolCallResult {
        let input: StatusInput = parse_args(args).unwrap_or_default();
        let want = |d: &str| input.dataset.as_deref().is_none_or(|w| w == d);
        let week_ago = (chrono::Utc::now() - chrono::Duration::days(7))
            .date_naive()
            .to_string();
        let mut out = serde_json::Map::new();
        let runs = |d: &str| {
            self.q(&format!(
                "SELECT strftime(finished_at, '%Y-%m-%d %H:%M') AS finished_utc, api_calls, files, rows, pruned, error \
                 FROM etl_runs WHERE dataset = '{d}' ORDER BY finished_at DESC LIMIT 5"
            ))
        };
        let failures = |d: &str| {
            self.q(&format!(
                "SELECT count(*) AS runs, count(error) AS failed FROM etl_runs \
                 WHERE dataset = '{d}' AND finished_at >= CAST('{}' AS TIMESTAMP)",
                (chrono::Utc::now() - chrono::Duration::hours(24)).format("%Y-%m-%d %H:%M:%S")
            ))
        };
        if want("gdelt") {
            let classes = self.classes().await;
            let hosts = self
                .store
                .query_json(
                    "SELECT source_host AS key, count(*) AS events, count(DISTINCT source_url) AS articles \
                     FROM gdelt_events WHERE event_date >= CAST(? AS DATE) AND source_host IS NOT NULL \
                     GROUP BY 1 ORDER BY 2 DESC LIMIT 2000",
                    std::slice::from_ref(&week_ago),
                )
                .unwrap_or_default();
            let mix = classes.mix(&hosts, 0);
            let top_unlisted: Vec<Value> = hosts
                .iter()
                .filter(|h| classes.classify(h["key"].as_str().unwrap_or("")) == "unlisted")
                .take(15)
                .cloned()
                .collect();
            out.insert(
                "gdelt".into(),
                json!({
                    "freshness": if self.schedule_enabled("GDELT sync").await == Some(false) {
                        json!({
                            "state": "paused",
                            "summary": "Paused: the GDELT sync schedule is disabled (retired as a \
                                        source; see schedules/gdelt-sync.yaml). Rows already loaded \
                                        remain but no agent queries them.",
                        })
                    } else {
                        gdelt::freshness(
                            self.store.last_file("gdelt").ok().flatten().as_deref(),
                            chrono::Utc::now().naive_utc(),
                        )
                    },
                    "rows": self.q("SELECT count(*) AS events, CAST(min(event_date) AS VARCHAR) AS earliest_event, \
                                    CAST(max(event_date) AS VARCHAR) AS latest_event FROM gdelt_events"),
                    "files": self.q("SELECT status, count(*) AS files, max(file) AS newest FROM etl_files \
                                     WHERE dataset = 'gdelt' GROUP BY 1"),
                    "last_runs": runs("gdelt"),
                    "last_24h": failures("gdelt"),
                    "source_mix_7d": mix["by_class"],
                    "top_unlisted_outlets_7d": top_unlisted,
                    "review_hint": "Unlisted outlets carrying a large share are candidates for a SourceList \
                                    (or for being discounted); a rising state_media or platform share is worth a look.",
                }),
            );
        }
        if want("reliefweb") {
            let filter = format!("date_created >= CAST('{week_ago}' AS TIMESTAMP)");
            let top = |col: &str| {
                self.q(&format!(
                    "SELECT trim(v) AS key, count(*) AS reports FROM \
                       (SELECT unnest(string_split({col}, ';')) AS v FROM reliefweb_reports WHERE {filter}) \
                     WHERE trim(v) <> '' GROUP BY 1 ORDER BY 2 DESC LIMIT 10"
                ))
            };
            out.insert(
                "reliefweb".into(),
                json!({
                    "freshness": self.reliefweb_freshness().await,
                    "rows": self.q("SELECT count(*) AS reports, strftime(min(date_created), '%Y-%m-%d') AS earliest, \
                                    strftime(max(date_created), '%Y-%m-%d %H:%M') AS latest_utc FROM reliefweb_reports"),
                    "api_calls_today": self.store.api_calls_today("reliefweb").ok(),
                    "last_runs": runs("reliefweb"),
                    "last_24h": failures("reliefweb"),
                    "publishers_7d": top("sources"),
                    "publisher_types_7d": top("source_types"),
                }),
            );
        }
        ToolCallResult::success_json(Value::Object(out))
    }

    async fn handle_gdelt(&self, args: Option<Value>) -> ToolCallResult {
        let input: GdeltInput = match parse_args(args) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let today = chrono::Utc::now().date_naive();
        let (since, until) = match window(
            input.since.as_deref(),
            input.until.as_deref(),
            input.days,
            today,
        ) {
            Ok(w) => w,
            Err(e) => return ToolCallResult::error(e),
        };
        let cfg = match datasets::load_config::<GdeltConfig>("gdelt") {
            Ok(c) => c,
            Err(e) => return ToolCallResult::error(format!("{e:#}")),
        };
        let q = GdeltQuery {
            countries: input.country,
            event_roots: input.event_type,
            quad_class: input.quad_class.filter(|q| (1..=4).contains(q)),
            actor: input.actor,
            since,
            until,
            min_sources: input.min_sources,
            root_only: input.root_only.unwrap_or(false),
            sample: input.sample.unwrap_or(12).min(50),
        };
        let classes = self.classes().await;
        match gdelt::run_query(&self.store, &cfg, &classes, &q, today) {
            Ok(v) => ToolCallResult::success_json(v),
            Err(e) => ToolCallResult::error(format!("GDELT query failed: {e:#}")),
        }
    }

    async fn handle_reliefweb(&self, args: Option<Value>) -> ToolCallResult {
        let input: ReliefwebInput = match parse_args(args) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let today = chrono::Utc::now().date_naive();
        let (since, until) = match window(
            input.since.as_deref(),
            input.until.as_deref(),
            input.days,
            today,
        ) {
            Ok(w) => w,
            Err(e) => return ToolCallResult::error(e),
        };
        let q = ReliefwebQuery {
            countries: input.country,
            text: input.query,
            theme: input.theme,
            source: input.source,
            disaster: input.disaster,
            format: input.format,
            since,
            until,
            limit: input.limit.unwrap_or(15).clamp(1, 50),
        };
        match reliefweb::run_query(&self.store, &q, today) {
            Ok(v) => ToolCallResult::success_json(v),
            Err(e) => ToolCallResult::error(format!("ReliefWeb query failed: {e:#}")),
        }
    }
}

#[async_trait]
impl Skill for DatasetSkill {
    fn name(&self) -> &str {
        "Datasets"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        Self::defs()
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "dataset_sync" => Some(self.handle_sync(arguments).await),
            "dataset_status" => Some(self.handle_status(arguments).await),
            "gdelt_events" => Some(self.handle_gdelt(arguments).await),
            "reliefweb_reports" => Some(self.handle_reliefweb(arguments).await),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_count_back_from_until_and_dates_must_be_iso() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 6).unwrap();
        let (s, u) = window(None, Some("2026-10-01"), Some(10), today).unwrap();
        assert_eq!(s, NaiveDate::from_ymd_opt(2026, 9, 21));
        assert_eq!(u, NaiveDate::from_ymd_opt(2026, 10, 1));
        assert_eq!(window(None, None, None, today).unwrap(), (None, None));
        assert!(window(Some("last week"), None, None, today).is_err());
    }

    #[test]
    fn inputs_accept_a_string_or_a_list() {
        let g: GdeltInput =
            serde_json::from_value(json!({"country": "Ukraine", "event_type": ["fight", "18"]}))
                .unwrap();
        assert_eq!(g.country, vec!["Ukraine"]);
        assert_eq!(g.event_type.len(), 2);
        let r: ReliefwebInput = serde_json::from_value(json!({})).unwrap();
        assert!(r.country.is_empty());
    }
}
