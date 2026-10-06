//! ACLED (Armed Conflict Location & Event Data) API client.
//!
//! ACLED codes political violence and protest events worldwide — who, where,
//! when, what kind, how many killed — from news, local partners and reports,
//! updated weekly. The brain queries it live; it does not mirror it.
//!
//! **Licence constraints shape this module** (EULA, read 2026-10-06):
//! - §7.1: ACLED content must not be used to train, test, develop or improve an
//!   AI system in a way that substitutes for ACLED, lets third parties retrieve
//!   ACLED content through it, or exceeds the licence. So rows are fetched per
//!   question and summarised; nothing here writes raw rows to the graph, and
//!   ACLED-derived material is never part of a fine-tuning set.
//! - Derivative material must be transformative ("not sufficient for Licensed
//!   Content to simply be supplemented, appended, excerpted, reorganized"),
//!   which is why `summarize_events` returns aggregates and a small sample.
//! - Every answer carries the attribution line.
//!
//! Auth is OAuth2 password grant (`client_id=acled`): the access token lives
//! 24 h, the refresh token 14 days. Tokens are cached in memory; a 401 drops
//! the cache and retries once with a fresh login.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chrono::NaiveDate;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::{info, warn};

const TOKEN_URL: &str = "https://acleddata.com/oauth/token";
const EVENTS_URL: &str = "https://acleddata.com/api/acled/read";

/// Columns requested. Fewer than the full 31 keeps the payload small; these
/// are the ones a summary or a claim check reads.
const FIELDS: &str = "event_id_cnty|event_date|disorder_type|event_type|sub_event_type|actor1|\
                      actor2|country|admin1|location|fatalities|notes|source|civilian_targeting";

/// Rows fetched when the caller names no limit, and the ceiling. ACLED's own
/// per-call ceiling is 5,000; past a couple of thousand the aggregates are
/// what matter, and a capped answer says it is capped.
pub const DEFAULT_LIMIT: usize = 500;
pub const MAX_LIMIT: usize = 2000;

/// Days covered when the caller gives no start date.
pub const DEFAULT_DAYS: i64 = 30;

pub const ATTRIBUTION: &str = "Data: ACLED (Armed Conflict Location & Event Data), acleddata.com";

/// ACLED's six event types — the values `event_type` matches against.
pub const EVENT_TYPES: &[&str] = &[
    "Battles",
    "Violence against civilians",
    "Explosions/Remote violence",
    "Riots",
    "Protests",
    "Strategic developments",
];

struct Token {
    access: String,
    refresh: Option<String>,
    expires_at: Instant,
    refresh_expires_at: Instant,
}

pub struct AcledClient {
    http: reqwest::Client,
    email: String,
    password: String,
    token: Mutex<Option<Token>>,
}

/// One events query. Every filter is optional; dates default to the last
/// [`DEFAULT_DAYS`].
#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    pub countries: Vec<String>,
    pub event_type: Option<String>,
    pub actor: Option<String>,
    pub admin1: Option<String>,
    pub location: Option<String>,
    pub since: Option<NaiveDate>,
    pub until: Option<NaiveDate>,
    pub limit: Option<usize>,
}

impl EventQuery {
    /// The effective date window, defaulting to the last [`DEFAULT_DAYS`].
    pub fn window(&self, today: NaiveDate) -> (NaiveDate, NaiveDate) {
        let until = self.until.unwrap_or(today);
        let since = self
            .since
            .unwrap_or(until - chrono::Duration::days(DEFAULT_DAYS));
        (since, until)
    }

    pub fn effective_limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }

    /// Query-string pairs for `acled/read`. `|` is ACLED's OR; a date range is
    /// `event_date=a|b` with `event_date_where=BETWEEN`.
    pub fn params(&self, today: NaiveDate) -> Vec<(String, String)> {
        let (since, until) = self.window(today);
        let mut p: Vec<(String, String)> = vec![
            ("_format".into(), "json".into()),
            ("fields".into(), FIELDS.into()),
            ("limit".into(), self.effective_limit().to_string()),
            ("event_date".into(), format!("{since}|{until}")),
            ("event_date_where".into(), "BETWEEN".into()),
        ];
        let countries: Vec<&str> = self
            .countries
            .iter()
            .map(|c| c.trim())
            .filter(|c| !c.is_empty())
            .collect();
        if !countries.is_empty() {
            p.push(("country".into(), countries.join("|")));
        }
        let mut opt = |key: &str, v: &Option<String>| {
            if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                p.push((key.into(), v.into()));
            }
        };
        opt("event_type", &self.event_type);
        opt("actor1", &self.actor);
        opt("admin1", &self.admin1);
        opt("location", &self.location);
        p
    }
}

impl AcledClient {
    /// `None` unless both `ACLED_EMAIL` and `ACLED_PASSWORD` are set.
    pub fn from_env() -> Option<Self> {
        let email = std::env::var("ACLED_EMAIL")
            .ok()
            .filter(|v| !v.trim().is_empty())?;
        let password = std::env::var("ACLED_PASSWORD")
            .ok()
            .filter(|v| !v.is_empty())?;
        Some(Self {
            // Explicit agent: ACLED's Cloudflare blocks some default ones
            // outright (error 1010 for Python's urllib, 2026-10-06).
            http: reqwest::Client::builder()
                .user_agent(concat!("agent-brain/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            email: email.trim().to_string(),
            password,
            token: Mutex::new(None),
        })
    }

    async fn request_token(&self, form: &[(&str, &str)]) -> Result<Token> {
        let resp = self.http.post(TOKEN_URL).form(form).send().await?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            // Never echo the form: it carries the password.
            let msg = body
                .get("error_description")
                .or_else(|| body.get("message"))
                .or_else(|| body.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("no detail");
            return Err(anyhow!("ACLED token request failed: HTTP {status} — {msg}"));
        }
        let access = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("ACLED token response had no access_token"))?
            .to_string();
        let expires_in = body
            .get("expires_in")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(86_400);
        let now = Instant::now();
        Ok(Token {
            access,
            refresh: body
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .map(String::from),
            // Renew five minutes early rather than race the expiry.
            expires_at: now + Duration::from_secs(expires_in.saturating_sub(300)),
            refresh_expires_at: now + Duration::from_secs(13 * 86_400),
        })
    }

    /// A valid access token: cached, refreshed, or from a fresh login.
    async fn access_token(&self) -> Result<String> {
        let mut guard = self.token.lock().await;
        let now = Instant::now();
        if let Some(t) = guard.as_ref()
            && t.expires_at > now
        {
            return Ok(t.access.clone());
        }
        let refreshed = match guard.as_ref() {
            Some(Token {
                refresh: Some(r),
                refresh_expires_at,
                ..
            }) if *refresh_expires_at > now => self
                .request_token(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", r.as_str()),
                    ("client_id", "acled"),
                ])
                .await
                .map_err(|e| warn!(error = %e, "ACLED token refresh failed; logging in again"))
                .ok(),
            _ => None,
        };
        let token = match refreshed {
            Some(t) => t,
            None => {
                info!("ACLED: requesting access token");
                self.request_token(&[
                    ("username", self.email.as_str()),
                    ("password", self.password.as_str()),
                    ("grant_type", "password"),
                    ("client_id", "acled"),
                    ("scope", "authenticated"),
                ])
                .await?
            }
        };
        let access = token.access.clone();
        *guard = Some(token);
        Ok(access)
    }

    /// Fetch event rows. A 401 drops the cached token and retries once.
    pub async fn events(&self, q: &EventQuery) -> Result<Vec<Value>> {
        let params = q.params(chrono::Utc::now().date_naive());
        for attempt in 0..2 {
            let token = self.access_token().await?;
            let resp = self
                .http
                .get(EVENTS_URL)
                .bearer_auth(&token)
                .query(&params)
                .send()
                .await
                .context("ACLED request failed")?;
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                *self.token.lock().await = None;
                continue;
            }
            let body: Value = resp
                .json()
                .await
                .with_context(|| format!("ACLED returned HTTP {status} with a non-JSON body"))?;
            if !status.is_success() {
                return Err(anyhow!(
                    "{}",
                    read_error(status.as_u16(), &error_text(&body))
                ));
            }
            return rows_of(&body);
        }
        Err(anyhow!(
            "ACLED rejected a fresh token (HTTP 401) — check the account's API access"
        ))
    }
}

/// A read failure, with the likely cause named. A 403 after a successful
/// login is ACLED's access tier, not the request: the free "Open" tier (the
/// one any generic-domain address such as gmail.com gets) has no API access,
/// and its answer to every read is a bare "Access denied" (2026-10-06).
fn read_error(status: u16, message: &str) -> String {
    if status == 403 {
        format!(
            "ACLED HTTP 403: {message}. The login succeeded, so this account is not \
             authorized for API data: ACLED's Open tier (assigned to generic email \
             domains such as gmail.com) has no API access; the Research tier and above do. \
             Access is changed by contacting ACLED, not by changing the query."
        )
    } else {
        format!("ACLED HTTP {status}: {message}")
    }
}

/// The message in an ACLED error body, whatever shape it takes.
fn error_text(body: &Value) -> String {
    for key in ["message", "error", "messages"] {
        match body.get(key) {
            Some(Value::String(s)) => return s.clone(),
            Some(v @ (Value::Array(_) | Value::Object(_))) => return v.to_string(),
            _ => {}
        }
    }
    body.to_string().chars().take(300).collect()
}

/// The rows of a read response: `{"data": [...]}`, or a bare array. A body
/// that reports failure (`"success": false`) is an error, not zero rows.
pub fn rows_of(body: &Value) -> Result<Vec<Value>> {
    if body.get("success").and_then(|v| v.as_bool()) == Some(false) {
        return Err(anyhow!("ACLED reported failure: {}", error_text(body)));
    }
    match body {
        Value::Array(a) => Ok(a.clone()),
        _ => match body.get("data") {
            Some(Value::Array(a)) => Ok(a.clone()),
            _ => Err(anyhow!(
                "ACLED response had no data array: {}",
                error_text(body)
            )),
        },
    }
}

fn s<'a>(row: &'a Value, key: &str) -> &'a str {
    row.get(key).and_then(|v| v.as_str()).unwrap_or("").trim()
}

/// ACLED sends numbers as strings.
fn fatalities(row: &Value) -> i64 {
    match row.get("fatalities") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(t)) => t.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// Count events and fatalities per key, largest first, at most `top`.
fn tally(rows: &[Value], top: usize, key: impl Fn(&Value) -> Vec<String>) -> Vec<Value> {
    let mut m: std::collections::HashMap<String, (i64, i64)> = std::collections::HashMap::new();
    for r in rows {
        for k in key(r).into_iter().filter(|k| !k.is_empty()) {
            let e = m.entry(k).or_insert((0, 0));
            e.0 += 1;
            e.1 += fatalities(r);
        }
    }
    let mut v: Vec<(String, (i64, i64))> = m.into_iter().collect();
    v.sort_by(|a, b| {
        b.1.0
            .cmp(&a.1.0)
            .then(b.1.1.cmp(&a.1.1))
            .then(a.0.cmp(&b.0))
    });
    v.into_iter()
        .take(top)
        .map(|(k, (events, fat))| json!({"key": k, "events": events, "fatalities": fat}))
        .collect()
}

/// Monday of the ISO week containing an ACLED `event_date`.
fn week_of(date: &str) -> Option<String> {
    use chrono::Datelike;
    let d = NaiveDate::parse_from_str(date.get(..10)?, "%Y-%m-%d").ok()?;
    Some((d - chrono::Duration::days(d.weekday().num_days_from_monday() as i64)).to_string())
}

/// Aggregates plus a small, most-recent-first sample — the transformative
/// shape the licence asks for, and the count-then-drill shape the brain's
/// tools follow: every bucket can be re-queried by its own filter.
pub fn summarize_events(rows: &[Value], q: &EventQuery, today: NaiveDate, sample: usize) -> Value {
    let (since, until) = q.window(today);
    let limit = q.effective_limit();
    let total_fatalities: i64 = rows.iter().map(fatalities).sum();
    let mut recent: Vec<&Value> = rows.iter().collect();
    recent.sort_by(|a, b| s(b, "event_date").cmp(s(a, "event_date")));
    let sample_rows: Vec<Value> = recent
        .iter()
        .take(sample)
        .map(|r| {
            let notes = s(r, "notes");
            json!({
                "event_id": s(r, "event_id_cnty"),
                "date": s(r, "event_date"),
                "country": s(r, "country"),
                "admin1": s(r, "admin1"),
                "location": s(r, "location"),
                "event_type": s(r, "event_type"),
                "sub_event_type": s(r, "sub_event_type"),
                "actor1": s(r, "actor1"),
                "actor2": s(r, "actor2"),
                "fatalities": fatalities(r),
                "notes": notes.chars().take(300).collect::<String>(),
                "source": s(r, "source"),
            })
        })
        .collect();

    let mut notes: Vec<String> = Vec::new();
    let complete = rows.len() < limit;
    if !complete {
        notes.push(format!(
            "TRUNCATED: the limit of {limit} rows was reached, so the counts below cover \
             only the {limit} matching events ACLED returned, not all of them. Narrow the query (country, \
             event_type, a shorter window) or raise `limit` (max {MAX_LIMIT})."
        ));
    }
    if (today - until).num_days() < 10 {
        notes.push(
            "ACLED publishes weekly, so the most recent ~7-10 days are usually incomplete.".into(),
        );
    }
    if rows.is_empty() {
        notes.push(format!(
            "No events matched. Country names must match ACLED's spelling (e.g. \
             \"Democratic Republic of Congo\", \"United States\"); `actor` matches the \
             primary actor only; `event_type` is one of: {}.",
            EVENT_TYPES.join(", ")
        ));
    }
    notes.push(format!(
        "{ATTRIBUTION}. Licence: query live for analysis; do not store these rows in the \
         knowledge graph — store your analysis, citing ACLED."
    ));

    let mut by_week = tally(rows, usize::MAX, |r| {
        week_of(s(r, "event_date")).into_iter().collect()
    });
    by_week.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));

    json!({
        "window": {"since": since.to_string(), "until": until.to_string()},
        "returned": rows.len(),
        "complete": complete,
        "total_fatalities": total_fatalities,
        "by_country": tally(rows, 15, |r| vec![s(r, "country").to_string()]),
        "by_event_type": tally(rows, 10, |r| vec![s(r, "event_type").to_string()]),
        "by_sub_event_type": tally(rows, 10, |r| vec![s(r, "sub_event_type").to_string()]),
        "by_admin1": tally(rows, 10, |r| vec![s(r, "admin1").to_string()]),
        "top_actors": tally(rows, 10, |r| vec![s(r, "actor1").to_string(), s(r, "actor2").to_string()]),
        "by_week": by_week,
        "sample": sample_rows,
        "notes": notes,
        "attribution": ATTRIBUTION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn param<'a>(p: &'a [(String, String)], k: &str) -> Option<&'a str> {
        p.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
    }

    #[test]
    fn params_default_to_the_last_thirty_days_with_between() {
        let p = EventQuery::default().params(d("2026-10-06"));
        assert_eq!(param(&p, "event_date"), Some("2026-09-06|2026-10-06"));
        assert_eq!(param(&p, "event_date_where"), Some("BETWEEN"));
        assert_eq!(param(&p, "limit"), Some("500"));
        assert_eq!(param(&p, "_format"), Some("json"));
        assert!(param(&p, "country").is_none());
    }

    #[test]
    fn countries_are_ored_and_filters_map_to_acled_names() {
        let q = EventQuery {
            countries: vec!["Ukraine".into(), " Russia ".into(), "".into()],
            event_type: Some("Battles".into()),
            actor: Some("Wagner".into()),
            limit: Some(99_999),
            ..Default::default()
        };
        let p = q.params(d("2026-10-06"));
        assert_eq!(param(&p, "country"), Some("Ukraine|Russia"));
        assert_eq!(param(&p, "event_type"), Some("Battles"));
        assert_eq!(param(&p, "actor1"), Some("Wagner"));
        assert_eq!(param(&p, "limit"), Some("2000"), "clamped to MAX_LIMIT");
    }

    #[test]
    fn a_403_after_login_names_the_access_tier() {
        let m = read_error(403, "Access denied");
        assert!(m.contains("Open tier") && m.contains("not authorized for API data"));
        assert_eq!(read_error(500, "boom"), "ACLED HTTP 500: boom");
    }

    #[test]
    fn rows_come_from_data_or_a_bare_array_and_failure_is_an_error() {
        assert_eq!(
            rows_of(&json!({"success": true, "data": [{"a": 1}]}))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(rows_of(&json!([{"a": 1}, {"a": 2}])).unwrap().len(), 2);
        let err = rows_of(&json!({"success": false, "message": "Access denied"})).unwrap_err();
        assert!(err.to_string().contains("Access denied"));
        assert!(rows_of(&json!({"status": 200})).is_err());
    }

    fn row(date: &str, country: &str, et: &str, a1: &str, fat: &str) -> Value {
        json!({"event_id_cnty": format!("{country}{date}"), "event_date": date,
               "country": country, "event_type": et, "sub_event_type": "x",
               "actor1": a1, "actor2": "", "admin1": "A", "fatalities": fat, "notes": "n"})
    }

    #[test]
    fn summaries_count_events_and_string_fatalities_and_sample_newest_first() {
        let rows = vec![
            row(
                "2026-09-01",
                "Ukraine",
                "Battles",
                "Military Forces of Russia",
                "3",
            ),
            row(
                "2026-09-20",
                "Ukraine",
                "Explosions/Remote violence",
                "Military Forces of Russia",
                "5",
            ),
            row("2026-09-10", "Sudan", "Battles", "RSF", "0"),
        ];
        let q = EventQuery {
            until: Some(d("2026-09-30")),
            ..Default::default()
        };
        let out = summarize_events(&rows, &q, d("2026-10-06"), 2);
        assert_eq!(out["returned"], 3);
        assert_eq!(out["complete"], true);
        assert_eq!(out["total_fatalities"], 8);
        assert_eq!(
            out["by_country"][0],
            json!({"key": "Ukraine", "events": 2, "fatalities": 8})
        );
        assert_eq!(out["top_actors"][0]["key"], "Military Forces of Russia");
        assert_eq!(out["sample"].as_array().unwrap().len(), 2);
        assert_eq!(out["sample"][0]["date"], "2026-09-20");
        assert_eq!(out["by_week"][0]["key"], "2026-08-31");
        let notes = out["notes"].to_string();
        assert!(notes.contains("ACLED publishes weekly"));
        assert!(notes.contains("do not store these rows"));
    }

    #[test]
    fn a_full_page_is_marked_truncated_and_an_empty_one_names_the_valid_values() {
        let q = EventQuery {
            limit: Some(2),
            ..Default::default()
        };
        let rows = vec![row("2026-09-01", "Mali", "Battles", "JNIM", "1"); 2];
        let out = summarize_events(&rows, &q, d("2026-10-06"), 5);
        assert_eq!(out["complete"], false);
        assert!(out["notes"][0].as_str().unwrap().starts_with("TRUNCATED"));

        let empty = summarize_events(&[], &EventQuery::default(), d("2026-10-06"), 5);
        assert!(
            empty["notes"]
                .to_string()
                .contains("Violence against civilians")
        );
    }
}
