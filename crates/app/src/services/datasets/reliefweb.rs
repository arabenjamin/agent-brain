//! ReliefWeb (UN OCHA) reports: ETL from the v2 API, and queries.
//!
//! Extract: POST `/v2/reports?appname=…` for reports created since the newest
//! one held (inclusive; the upsert dedups by id), oldest first, paged by
//! offset. Two budgets bound it: calls per run, and calls per UTC day summed
//! from `etl_runs` (ReliefWeb allows 1,000/day). Transform: flatten the
//! multi-valued fields and keep a body excerpt. Load: upsert by id.

use anyhow::{Context, Result, anyhow};
use chrono::{Duration, NaiveDate};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use super::text;
use crate::repository::{DatasetStore, ReliefwebRow};

#[derive(Debug, Clone, Deserialize)]
pub struct ReliefwebConfig {
    pub api_url: String,
    pub appname_env: String,
    pub retention_days: i64,
    pub initial_backfill_days: i64,
    pub page_size: usize,
    pub max_calls_per_run: i64,
    pub max_calls_per_day: i64,
    pub body_excerpt_chars: usize,
}

const FIELDS: &[&str] = &[
    "title",
    "url",
    "url_alias",
    "date.created",
    "date.original",
    "source.name",
    "source.shortname",
    "source.type.name",
    "primary_country.name",
    "country.name",
    "disaster.name",
    "disaster_type.name",
    "theme.name",
    "format.name",
    "language.code",
    "body",
];

/// Request body for one page of reports created at or after `since`.
pub fn request_body(since: &str, offset: usize, limit: usize) -> Value {
    json!({
        "filter": {"field": "date.created", "value": {"from": since}},
        "sort": ["date.created:asc", "id:asc"],
        "limit": limit,
        "offset": offset,
        "fields": {"include": FIELDS},
    })
}

/// `"; "`-joined values of `key` across an array of objects (or one object).
fn joined(v: &Value, key: &str) -> String {
    let pick = |o: &Value| -> Option<String> {
        o.get(key)
            .or_else(|| o.get("name"))
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let mut out: Vec<String> = match v {
        Value::Array(a) => a.iter().filter_map(pick).collect(),
        Value::Object(_) => pick(v).into_iter().collect(),
        _ => vec![],
    };
    out.dedup();
    out.join("; ")
}

fn excerpt(body: &str, max: usize) -> String {
    if body.chars().count() <= max {
        return body.trim().to_string();
    }
    let cut: String = body.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

/// Rows of one response, plus ReliefWeb's `count` and `totalCount`.
pub fn parse_reports(
    body: &Value,
    excerpt_chars: usize,
) -> Result<(Vec<ReliefwebRow>, usize, i64)> {
    let data = body.get("data").and_then(|d| d.as_array()).ok_or_else(|| {
        anyhow!(
            "ReliefWeb response had no data array: {}",
            error_message(body)
        )
    })?;
    let mut rows = Vec::with_capacity(data.len());
    for item in data {
        let f = &item["fields"];
        let id = item["id"]
            .as_i64()
            .or_else(|| item["id"].as_str().and_then(|s| s.parse().ok()))
            .ok_or_else(|| anyhow!("report without an id"))?;
        let date_created = text(&f["date"]["created"]);
        if date_created.is_empty() {
            continue;
        }
        let url = Some(text(&f["url_alias"]))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| text(&f["url"]));
        rows.push(ReliefwebRow {
            id,
            title: text(&f["title"]),
            url,
            date_created,
            date_original: Some(text(&f["date"]["original"])).filter(|s| !s.is_empty()),
            sources: joined(&f["source"], "shortname"),
            source_types: {
                let types: Vec<Value> = f["source"]
                    .as_array()
                    .map(|a| a.iter().map(|s| s["type"].clone()).collect())
                    .unwrap_or_default();
                joined(&Value::Array(types), "name")
            },
            primary_country: joined(&f["primary_country"], "name"),
            countries: joined(&f["country"], "name"),
            disasters: joined(&f["disaster"], "name"),
            disaster_types: joined(&f["disaster_type"], "name"),
            themes: joined(&f["theme"], "name"),
            formats: joined(&f["format"], "name"),
            languages: joined(&f["language"], "code"),
            body_excerpt: excerpt(&text(&f["body"]), excerpt_chars),
        });
    }
    let count = body["count"]
        .as_u64()
        .map(|n| n as usize)
        .unwrap_or(data.len());
    Ok((rows, count, body["totalCount"].as_i64().unwrap_or(-1)))
}

fn error_message(body: &Value) -> String {
    body.pointer("/error/message")
        .or_else(|| body.get("message"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| body.to_string().chars().take(300).collect())
}

/// One extract → transform → load run, within both call budgets.
pub async fn sync(store: &DatasetStore, cfg: &ReliefwebConfig, http: &reqwest::Client) -> Value {
    let started = chrono::Utc::now();
    let mut calls = 0i64;
    let mut rows_loaded = 0i64;
    let mut more = false;

    let result: Result<i64> = async {
        let appname = std::env::var(&cfg.appname_env)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "{} is not set — ReliefWeb needs the approved appname",
                    cfg.appname_env
                )
            })?;
        let used_today = store.api_calls_today("reliefweb")?;
        let since = match store.reliefweb_newest()? {
            Some(s) => s,
            None => (started - Duration::days(cfg.initial_backfill_days))
                .format("%Y-%m-%dT%H:%M:%S+00:00")
                .to_string(),
        };
        let mut offset = 0usize;
        loop {
            if calls >= cfg.max_calls_per_run || used_today + calls >= cfg.max_calls_per_day {
                more = true;
                break;
            }
            calls += 1;
            let resp = http
                .post(&cfg.api_url)
                .query(&[("appname", appname.as_str())])
                .json(&request_body(&since, offset, cfg.page_size))
                .send()
                .await?;
            let status = resp.status();
            let body: Value = resp
                .json()
                .await
                .with_context(|| format!("ReliefWeb HTTP {status}: non-JSON body"))?;
            if !status.is_success() {
                return Err(anyhow!("ReliefWeb HTTP {status}: {}", error_message(&body)));
            }
            let (rows, count, _) = parse_reports(&body, cfg.body_excerpt_chars)?;
            rows_loaded += store.upsert_reliefweb(&rows)? as i64;
            if count < cfg.page_size {
                break;
            }
            offset += count;
        }
        Ok(used_today + calls)
    }
    .await;

    let pruned = store.prune_reliefweb(cfg.retention_days).unwrap_or(0) as i64;
    let _ = store.checkpoint();
    let error = result.as_ref().err().map(|e| format!("{e:#}"));
    let _ = store.record_run(
        "reliefweb",
        &started.format("%Y-%m-%d %H:%M:%S").to_string(),
        calls,
        0,
        rows_loaded,
        pruned,
        error.as_deref(),
    );
    info!(calls, rows_loaded, pruned, error = ?error, "reliefweb sync");
    json!({
        "dataset": "reliefweb",
        "api_calls": calls,
        "api_calls_today": result.as_ref().ok(),
        "daily_budget": cfg.max_calls_per_day,
        "reports_loaded": rows_loaded,
        "reports_pruned": pruned,
        "more_pending": more,
        "error": error,
    })
}

/// One reports query against the local table. Dates default to 30 days.
#[derive(Debug, Clone, Default)]
pub struct ReliefwebQuery {
    pub countries: Vec<String>,
    /// Every word must appear in the title or body excerpt.
    pub text: Option<String>,
    pub theme: Option<String>,
    pub source: Option<String>,
    pub disaster: Option<String>,
    pub format: Option<String>,
    pub since: Option<NaiveDate>,
    pub until: Option<NaiveDate>,
    pub limit: usize,
}

impl ReliefwebQuery {
    pub fn window(&self, today: NaiveDate) -> (NaiveDate, NaiveDate) {
        let until = self.until.unwrap_or(today);
        (self.since.unwrap_or(until - Duration::days(30)), until)
    }

    pub fn filter(&self, today: NaiveDate) -> (String, Vec<String>) {
        let (since, until) = self.window(today);
        let mut sql =
            String::from("CAST(date_created AS DATE) BETWEEN CAST(? AS DATE) AND CAST(? AS DATE)");
        let mut args = vec![since.to_string(), until.to_string()];
        let countries: Vec<&String> = self
            .countries
            .iter()
            .filter(|c| !c.trim().is_empty())
            .collect();
        if !countries.is_empty() {
            let one = "(primary_country ILIKE ? OR countries ILIKE ?)";
            sql.push_str(&format!(
                " AND ({})",
                vec![one; countries.len()].join(" OR ")
            ));
            for c in countries {
                let pat = format!("%{}%", c.trim());
                args.push(pat.clone());
                args.push(pat);
            }
        }
        for (col, v) in [
            ("themes", &self.theme),
            ("sources", &self.source),
            ("disasters", &self.disaster),
            ("formats", &self.format),
        ] {
            if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                sql.push_str(&format!(" AND {col} ILIKE ?"));
                args.push(format!("%{v}%"));
            }
        }
        if let Some(t) = &self.text {
            for word in t.split_whitespace().filter(|w| w.len() > 1) {
                sql.push_str(" AND (title || ' ' || COALESCE(body_excerpt, '')) ILIKE ?");
                args.push(format!("%{word}%"));
            }
        }
        (sql, args)
    }
}

/// Counts of a `"; "`-joined column, one row per value.
fn unnested(
    store: &DatasetStore,
    col: &str,
    filter: &str,
    args: &[String],
    limit: usize,
) -> Result<Vec<Value>> {
    store.query_json(
        &format!(
            "SELECT trim(v) AS key, count(*) AS reports FROM \
               (SELECT unnest(string_split({col}, ';')) AS v FROM reliefweb_reports WHERE {filter}) \
             WHERE trim(v) <> '' GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT {limit}"
        ),
        args,
    )
}

pub fn run_query(store: &DatasetStore, q: &ReliefwebQuery, today: NaiveDate) -> Result<Value> {
    let (filter, args) = q.filter(today);
    let (since, until) = q.window(today);
    let total = store
        .query_json(
            &format!("SELECT count(*) AS n FROM reliefweb_reports WHERE {filter}"),
            &args,
        )?
        .first()
        .and_then(|r| r["n"].as_i64())
        .unwrap_or(0);

    let mut list_args = args.clone();
    list_args.push(q.limit.to_string());
    let reports: Vec<Value> = store.query_json(
        &format!(
            "SELECT id, strftime(date_created, '%Y-%m-%d') AS date, title, \
                   sources, source_types, primary_country, formats, themes, disasters, url, \
                   left(body_excerpt, 300) AS excerpt \
                 FROM reliefweb_reports WHERE {filter} \
                 ORDER BY date_created DESC LIMIT CAST(? AS INTEGER)"
        ),
        &list_args,
    )?;

    let coverage = store
        .query_json(
            "SELECT count(*) AS reports, \
               strftime(min(date_created), '%Y-%m-%d') AS first, \
               strftime(max(date_created), '%Y-%m-%d') AS last \
             FROM reliefweb_reports",
            &[],
        )?
        .first()
        .cloned()
        .unwrap_or(json!({}));

    let mut notes = vec![
        "ReliefWeb reports are documents published by UN agencies, governments, NGOs and \
         media; source_types says which. A report states its publisher's assessment — cite \
         the publisher, not ReliefWeb."
            .to_string(),
    ];
    if (reports.len() as i64) < total {
        notes.push(format!(
            "Showing {} of {total} matching reports, newest first; narrow the query or raise `limit` for the rest.",
            reports.len()
        ));
    }
    if coverage["reports"].as_i64().unwrap_or(0) == 0 {
        notes.push(
            "No ReliefWeb reports are loaded yet: run dataset_sync(dataset: \"reliefweb\").".into(),
        );
    } else if let Some(first) = coverage["first"].as_str()
        && first > since.to_string().as_str()
    {
        notes.push(format!(
            "Loaded coverage starts {first}; the window asked from {since}, so earlier reports are missing, not absent."
        ));
    }
    if total == 0 && !q.countries.is_empty() {
        let mut close = Vec::new();
        for c in &q.countries {
            for r in store.query_json(
                "SELECT DISTINCT primary_country FROM reliefweb_reports WHERE primary_country ILIKE ? LIMIT 5",
                &[format!("%{}%", c.trim())],
            )? {
                close.push(text(&r["primary_country"]));
            }
        }
        notes.push(format!(
            "No reports matched. Loaded primary countries resembling the request: {close:?}."
        ));
    }

    Ok(json!({
        "window": {"since": since.to_string(), "until": until.to_string()},
        "matching": total,
        "by_country": unnested(store, "primary_country", &filter, &args, 15)?,
        "by_source": unnested(store, "sources", &filter, &args, 15)?,
        "by_source_type": unnested(store, "source_types", &filter, &args, 8)?,
        "by_format": unnested(store, "formats", &filter, &args, 10)?,
        "by_theme": unnested(store, "themes", &filter, &args, 12)?,
        "reports": reports,
        "coverage": coverage,
        "notes": notes,
        "attribution": "Data: ReliefWeb (reliefweb.int), UN OCHA; each report's own publisher is named in `sources`.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_response() -> Value {
        json!({
            "totalCount": 2, "count": 2,
            "data": [
                {"id": "4101", "fields": {
                    "title": "Sudan: Flash Update",
                    "url_alias": "https://reliefweb.int/report/sudan/flash-update",
                    "url": "https://reliefweb.int/node/4101",
                    "date": {"created": "2026-10-05T08:00:00+00:00", "original": "2026-10-04T00:00:00+00:00"},
                    "source": [{"name": "UN Office for the Coordination of Humanitarian Affairs",
                                "shortname": "OCHA", "type": {"name": "International Organization"}},
                               {"name": "Sudan Red Crescent", "shortname": "SRCS",
                                "type": {"name": "Red Cross/Red Crescent Movement"}}],
                    "primary_country": {"name": "Sudan"},
                    "country": [{"name": "Sudan"}, {"name": "Chad"}],
                    "disaster": [{"name": "Sudan: Conflict - 2023-2026"}],
                    "theme": [{"name": "Protection and Human Rights"}, {"name": "Food and Nutrition"}],
                    "format": [{"name": "Situation Report"}],
                    "language": [{"code": "en"}],
                    "body": "Fighting in El Fasher displaced 12,000 people. ".repeat(10)
                }},
                {"id": 4102, "fields": {"title": "undated", "date": {}}}
            ]
        })
    }

    #[test]
    fn reports_flatten_into_rows() {
        let (rows, count, total) = parse_reports(&sample_response(), 60).unwrap();
        assert_eq!((count, total), (2, 2));
        assert_eq!(rows.len(), 1, "a report without date.created is skipped");
        let r = &rows[0];
        assert_eq!(r.id, 4101);
        assert_eq!(r.url, "https://reliefweb.int/report/sudan/flash-update");
        assert_eq!(r.sources, "OCHA; SRCS");
        assert_eq!(
            r.source_types,
            "International Organization; Red Cross/Red Crescent Movement"
        );
        assert_eq!(r.primary_country, "Sudan");
        assert_eq!(r.countries, "Sudan; Chad");
        assert_eq!(r.themes, "Protection and Human Rights; Food and Nutrition");
        assert_eq!(r.languages, "en");
        assert!(r.body_excerpt.ends_with('…') && r.body_excerpt.chars().count() <= 61);
    }

    #[test]
    fn an_error_body_is_an_error_with_its_message() {
        let err = parse_reports(
            &json!({"error": {"type": "x", "message": "You are not using an approved appname."}}),
            10,
        )
        .unwrap_err();
        assert!(err.to_string().contains("approved appname"));
    }

    #[test]
    fn the_request_pages_oldest_first_from_since() {
        let b = request_body("2026-10-01T00:00:00+00:00", 1000, 1000);
        assert_eq!(b["filter"]["value"]["from"], "2026-10-01T00:00:00+00:00");
        assert_eq!(b["sort"][0], "date.created:asc");
        assert_eq!(b["offset"], 1000);
        assert!(
            b["fields"]["include"]
                .as_array()
                .unwrap()
                .contains(&json!("source.type.name"))
        );
    }

    #[test]
    fn the_filter_binds_every_value() {
        let q = ReliefwebQuery {
            countries: vec!["Sudan".into(), "Chad".into()],
            text: Some("El Fasher displacement".into()),
            theme: Some("Protection".into()),
            limit: 10,
            ..Default::default()
        };
        let (sql, args) = q.filter(NaiveDate::from_ymd_opt(2026, 10, 6).unwrap());
        assert_eq!(sql.matches('?').count(), args.len());
        assert_eq!(args[0], "2026-09-06");
        assert!(args.contains(&"%Fasher%".to_string()));
    }

    #[test]
    fn queries_run_against_a_real_store() {
        let dir = std::env::temp_dir().join(format!("rw-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = DatasetStore::new(dir.join("d.duckdb")).unwrap();
        let (rows, _, _) = parse_reports(&sample_response(), 500).unwrap();
        store.upsert_reliefweb(&rows).unwrap();
        let q = ReliefwebQuery {
            countries: vec!["Sudan".into()],
            text: Some("Fasher".into()),
            limit: 5,
            since: NaiveDate::from_ymd_opt(2026, 10, 1),
            until: NaiveDate::from_ymd_opt(2026, 10, 6),
            ..Default::default()
        };
        let out = run_query(&store, &q, NaiveDate::from_ymd_opt(2026, 10, 6).unwrap()).unwrap();
        assert_eq!(out["matching"], 1);
        assert_eq!(out["reports"][0]["date"], "2026-10-05");
        assert_eq!(out["by_source"][0]["key"], "OCHA");
        assert_eq!(out["by_source_type"][0]["reports"], 1);
        let none = run_query(
            &store,
            &ReliefwebQuery {
                countries: vec!["Sud".into()],
                text: Some("zzz".into()),
                limit: 5,
                ..Default::default()
            },
            NaiveDate::from_ymd_opt(2026, 10, 6).unwrap(),
        )
        .unwrap();
        assert_eq!(none["matching"], 0);
        assert!(
            none["notes"].to_string().contains("Sudan"),
            "names the loaded country"
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
