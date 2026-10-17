//! Search engines as data (CORE_AND_PLUGINS_PLAN 6c).
//!
//! Each engine `search_web` can call is a YAML file in a bundle's
//! `search-engines/` dir (`plugins/core/search-engines/*.yaml`): its endpoint,
//! auth, key env var, parameter names, fixed parameters, result path and field
//! mapping, and what a spent quota looks like. One executor ([`run`]) calls any
//! of them. What cannot be data stays here as a named hook the YAML picks:
//!
//! - [`Syntax`]: how the parsed query is rendered (Google operators, plain text).
//! - [`Dialect`]: engine-specific parameters derived from the query — date
//!   formats (Exa, Tavily, Google `tbs`, CSE, Brave) and the Currents and Webz
//!   query builders.
//! - [`Shape`]: result shapes a field mapping cannot read — Google News'
//!   clustered stories.
//!
//! An engine that needs nothing new (a POST/GET API answering a list of
//! results) is a YAML file and a key; no Rust.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use chrono::{DateTime, NaiveDate, Utc};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::{info, warn};

use crate::search_query::QuerySpec;

/// One search engine, as declared in `search-engines/<name>.yaml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineSpec {
    pub name: String,
    /// Position in the default ladder (`SEARCH_ENGINE_ORDER` unset), lowest
    /// first. No rank: the engine runs only when a caller names it.
    #[serde(default)]
    pub rank: Option<u32>,
    /// Env var holding the key. None: the engine needs no key.
    #[serde(default)]
    pub key_env: Option<String>,
    /// More required values sent as query parameters, `param: ENV_VAR`
    /// (Google CSE's `cx`).
    #[serde(default)]
    pub env_params: BTreeMap<String, String>,
    /// Engines on one account: a spent quota on one demotes them all.
    #[serde(default)]
    pub quota_group: Option<String>,
    #[serde(default)]
    pub limits: Limits,
    /// The free allowance. An engine with one is in the rotation
    /// (`search_rotation.rs`); one without is reached only as a fallback or
    /// when a caller names it.
    #[serde(default)]
    pub budget: Option<Budget>,
    pub request: RequestSpec,
    pub response: ResponseSpec,
}

/// What an engine's free tier allows, in its own unit and period.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// The allowance per `per`, in `unit`s.
    pub free: f64,
    #[serde(default)]
    pub unit: BudgetUnit,
    pub per: BudgetPeriod,
    /// `usd` only: dot path to the answer's own price (Exa `costDollars.total`).
    pub cost_field: Option<String>,
    /// `usd` only: the price of a call whose answer names none.
    pub cost_default: Option<f64>,
    /// Vendor rate limits, in attempts.
    pub per_hour: Option<u32>,
    pub per_minute: Option<u32>,
    /// Relative share of the rotation (default 1): an engine with weight 2 is
    /// picked until it has twice the calls of a weight-1 engine today.
    pub weight: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BudgetUnit {
    /// One search is one unit.
    #[default]
    Call,
    /// Dollars of credit.
    Usd,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPeriod {
    /// Resets at 00:00 UTC.
    Day,
    /// Resets on the 1st (UTC). Vendors that refill on the signup
    /// anniversary drift by up to a month; the spent-quota signal covers it.
    Month,
    /// Never resets (a signup grant).
    Once,
}

/// What an engine cannot serve; a query outside these skips it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_window_days: Option<i64>,
    pub max_sites: Option<usize>,
    pub max_exclude_sites: Option<usize>,
    /// Whether a `filetype:` filter can be honoured.
    #[serde(default = "yes")]
    pub filetype: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_window_days: None,
            max_sites: None,
            max_exclude_sites: None,
            filetype: true,
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Get,
    /// POST with the parameters as a JSON body.
    Post,
}

/// Where the key goes: a header (`prefix` + key), or a query parameter.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    pub header: Option<String>,
    #[serde(default)]
    pub prefix: String,
    pub query: Option<String>,
}

/// How the parsed query is rendered for the engine.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Syntax {
    /// Google operators, dates as `after:`/`before:`.
    Google,
    /// Google operators without date operators (dates go elsewhere).
    GoogleUndated,
    /// Undated Google operators plus Google News' `when:` window.
    GoogleNews,
    /// Plain text; operators become parameters.
    Plain,
    /// Plain text keeping quoted phrases.
    PlainQuoted,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryParam {
    pub param: String,
    pub syntax: Syntax,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountParam {
    pub param: String,
    /// The most results the engine returns per request.
    pub max: Option<u8>,
}

/// Engine-specific parameters derived from the query.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    /// Published-date range, include/exclude text.
    Exa,
    /// `time_range` or a date range, `exact_match` for phrases.
    Tavily,
    /// Google's `tbs` date filter (Serper, SerpApi).
    GoogleTbs,
    /// Custom Search `dateRestrict`.
    Cse,
    /// Brave `freshness`.
    Brave,
    /// Currents keywords, one domain each way, a clamped 7-day range.
    Currents,
    /// Webz Lucene query and `ts` window start.
    Webz,
    /// TinyFish `recency_minutes`, or `after_date`/`before_date`.
    Tinyfish,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestSpec {
    pub method: Method,
    /// The endpoint; `{base}` is replaced by the base URL below.
    pub url: String,
    /// The endpoint for news searches, when the engine has a separate one.
    pub news_url: Option<String>,
    pub base_url_env: Option<String>,
    pub base_url_default: Option<String>,
    pub auth: Option<Auth>,
    pub query: Option<QueryParam>,
    pub count: Option<CountParam>,
    /// Sent on every request.
    #[serde(default)]
    pub fixed: Map<String, Value>,
    /// Sent when the search asks for news.
    #[serde(default)]
    pub news: Map<String, Value>,
    /// Parameter taking the list of domains to restrict to.
    pub sites: Option<String>,
    /// Parameter taking the list of domains to exclude.
    pub exclude_sites: Option<String>,
    pub dialect: Option<Dialect>,
    pub timeout_secs: Option<u64>,
}

/// Result shapes a field mapping cannot read.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    /// A list of results at `items`, read through the field mapping.
    #[default]
    Items,
    /// SerpApi Google News: stories, highlights and topic clusters, unrolled.
    GoogleNews,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseSpec {
    #[serde(default)]
    pub shape: Shape,
    /// Dot path to the result list (`web.results`).
    pub items: String,
    /// Dot path to the list when a news search answers under another key.
    pub news_items: Option<String>,
    #[serde(default = "field_title")]
    pub title: String,
    #[serde(default = "field_link")]
    pub link: String,
    /// Paths tried in order; the first non-empty one is the snippet. An array
    /// of strings is joined.
    #[serde(default = "field_snippet")]
    pub snippet: Vec<String>,
    #[serde(default = "field_date")]
    pub date: String,
    /// The publisher's name, when the engine gives one (`source` in the result).
    pub source: Option<String>,
    pub snippet_chars: Option<usize>,
    /// A field that, when present in a 200 answer, is the error (SerpApi).
    pub error_field: Option<String>,
    /// Statuses that mean the allowance is spent (none of them a 429).
    #[serde(default)]
    pub spent_status: Vec<u16>,
    /// Words in an error body that mean the allowance is spent.
    #[serde(default)]
    pub spent_words: Vec<String>,
    /// Top-level fields logged from each answer (Webz' prepaid `balance`).
    #[serde(default)]
    pub log_fields: Vec<String>,
}

fn field_title() -> String {
    "title".into()
}
fn field_link() -> String {
    "link".into()
}
fn field_snippet() -> Vec<String> {
    vec!["snippet".into()]
}
fn field_date() -> String {
    "date".into()
}

/// Parse and check one engine file.
pub fn parse(text: &str) -> Result<EngineSpec, String> {
    let spec: EngineSpec = serde_yaml::from_str(text).map_err(|e| format!("YAML: {e}"))?;
    check(&spec)?;
    Ok(spec)
}

/// What a loader must refuse: an engine the executor could not call.
pub fn check(spec: &EngineSpec) -> Result<(), String> {
    let ident = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    if !ident(&spec.name) {
        return Err(format!("name '{}' must be an identifier", spec.name));
    }
    let r = &spec.request;
    for url in std::iter::once(&r.url).chain(r.news_url.iter()) {
        let has_base = url.starts_with("{base}");
        if has_base && r.base_url_env.is_none() && r.base_url_default.is_none() {
            return Err(format!(
                "url '{url}' uses {{base}} but no base_url_env/base_url_default is set"
            ));
        }
        if !has_base && !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(format!(
                "url '{url}' must be http(s) or start with {{base}}"
            ));
        }
    }
    let builds_query = matches!(r.dialect, Some(Dialect::Currents | Dialect::Webz));
    if r.query.is_none() && !builds_query {
        return Err("request needs `query` (or a dialect that builds it)".into());
    }
    if spec.key_env.is_some() && r.auth.is_none() {
        return Err("a keyed engine needs `auth` (header or query)".into());
    }
    if let Some(a) = &r.auth
        && a.header.is_some() == a.query.is_some()
    {
        return Err("auth takes exactly one of `header` or `query`".into());
    }
    if let Some(b) = &spec.budget {
        if b.free.is_nan() || b.free <= 0.0 {
            return Err("budget.free must be above zero".into());
        }
        if b.unit == BudgetUnit::Usd && !b.cost_default.is_some_and(|c| c > 0.0) {
            return Err("a usd budget needs cost_default above zero".into());
        }
        if b.weight.is_some_and(|w| w.is_nan() || w <= 0.0) {
            return Err("budget.weight must be above zero".into());
        }
    }
    Ok(())
}

/// Every engine from the loaded bundles, core first. A later file naming an
/// engine already loaded is skipped with a warning.
pub fn engines() -> &'static [EngineSpec] {
    static ENGINES: OnceLock<Vec<EngineSpec>> = OnceLock::new();
    ENGINES.get_or_init(|| {
        let plugins = agent_brain_graph::bundles::Plugins::global();
        let mut out: Vec<EngineSpec> = Vec::new();
        for (plugin, path) in plugins.files("search-engines", "yaml") {
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "search engine: cannot read");
                    continue;
                }
            };
            match parse(&text) {
                Ok(spec) if out.iter().any(|e| e.name == spec.name) => {
                    warn!(engine = %spec.name, %plugin, "search engine declared twice; first kept");
                }
                Ok(spec) => out.push(spec),
                Err(e) => warn!(path = %path.display(), error = %e, "search engine skipped"),
            }
        }
        info!(
            engines = ?out.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            "Search engines loaded"
        );
        out
    })
}

pub fn get(name: &str) -> Option<&'static EngineSpec> {
    engines().iter().find(|e| e.name == name)
}

/// The default ladder: ranked engines, lowest rank first.
pub fn default_order(engines: &[EngineSpec]) -> Vec<String> {
    let mut ranked: Vec<&EngineSpec> = engines.iter().filter(|e| e.rank.is_some()).collect();
    ranked.sort_by_key(|e| e.rank);
    ranked.iter().map(|e| e.name.clone()).collect()
}

/// The engines sharing a quota with any of `exhausted` (itself included).
pub fn quota_siblings(engines: &[EngineSpec], exhausted: &[String]) -> Vec<String> {
    let groups: Vec<&str> = engines
        .iter()
        .filter(|e| exhausted.contains(&e.name))
        .filter_map(|e| e.quota_group.as_deref())
        .collect();
    let mut out = exhausted.to_vec();
    for e in engines {
        if e.quota_group
            .as_deref()
            .is_some_and(|g| groups.contains(&g))
            && !out.contains(&e.name)
        {
            out.push(e.name.clone());
        }
    }
    out
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Whether the engine has every value it needs to answer.
pub fn configured(spec: &EngineSpec) -> bool {
    spec.key_env
        .as_deref()
        .is_none_or(|k| env_nonempty(k).is_some())
        && spec.env_params.values().all(|e| env_nonempty(e).is_some())
}

/// Why the engine cannot serve this query, if it cannot.
pub fn unsupported(spec: &EngineSpec, q: &QuerySpec, today: NaiveDate) -> Option<String> {
    let l = &spec.limits;
    let window_days = q.since(today).map(|s| (today - s).num_days());
    if l.max_sites.is_some_and(|m| q.sites.len() > m) {
        Some(format!(
            "restricts to {} domain(s) only",
            l.max_sites.unwrap_or(0)
        ))
    } else if l
        .max_exclude_sites
        .is_some_and(|m| q.exclude_sites.len() > m)
    {
        Some(format!(
            "excludes {} domain(s) only",
            l.max_exclude_sites.unwrap_or(0)
        ))
    } else if !l.filetype && q.filetype.is_some() {
        Some("no filetype filter".into())
    } else if let (Some(max), Some(days)) = (l.max_window_days, window_days)
        && days > max
    {
        Some(format!("searches at most {max} days back"))
    } else {
        None
    }
}

fn render(syntax: Syntax, q: &QuerySpec) -> String {
    match syntax {
        Syntax::Google => q.google_syntax(true),
        Syntax::GoogleUndated => q.google_syntax(false),
        Syntax::GoogleNews => {
            // Google News ranks by relevance; without `when:` a "today" query
            // returned stories from March (2026-10-02, 0 of 8 current).
            let mut s = q.google_syntax(false);
            if let Some(when) = q.google_news_when() {
                s.push(' ');
                s.push_str(&when);
            }
            s
        }
        Syntax::Plain => q.plain_text(false),
        Syntax::PlainQuoted => q.plain_text(true),
    }
}

/// The request parameters, before auth: fixed, query, count, news, domains,
/// then the dialect's (which may override earlier ones).
pub fn params(
    spec: &EngineSpec,
    q: &QuerySpec,
    count: u8,
    now: DateTime<Utc>,
) -> Vec<(String, Value)> {
    let r = &spec.request;
    let mut p: Vec<(String, Value)> = r
        .fixed
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if let Some(qp) = &r.query {
        p.push((qp.param.clone(), json!(render(qp.syntax, q))));
    }
    if let Some(c) = &r.count {
        p.push((
            c.param.clone(),
            json!(c.max.map_or(count, |m| count.min(m))),
        ));
    }
    if q.news {
        p.extend(r.news.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    if let Some(f) = &r.sites
        && !q.sites.is_empty()
    {
        p.push((f.clone(), json!(q.sites)));
    }
    if let Some(f) = &r.exclude_sites
        && !q.exclude_sites.is_empty()
    {
        p.push((f.clone(), json!(q.exclude_sites)));
    }
    if let Some(d) = r.dialect {
        p.extend(dialect_params(d, q, now));
    }
    p
}

fn dialect_params(d: Dialect, q: &QuerySpec, now: DateTime<Utc>) -> Vec<(String, Value)> {
    let mut p: Vec<(String, Value)> = Vec::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            p.push((k.to_string(), v));
        }
    };
    match d {
        Dialect::Exa => {
            let (start, end) = q.exa_dates();
            put("startPublishedDate", start.map(Value::String));
            put("endPublishedDate", end.map(Value::String));
            put("includeText", q.exa_include_text().map(|t| json!([t])));
            put("excludeText", q.exa_exclude_text().map(|t| json!([t])));
        }
        Dialect::Tavily => {
            if q.has_phrases() {
                put("exact_match", Some(json!(true)));
            }
            match (q.recency, q.tavily_dates()) {
                (Some(r), (_, None)) => put("time_range", Some(json!(r.word()))),
                (_, (start, end)) => {
                    put("start_date", start.map(Value::String));
                    put("end_date", end.map(Value::String));
                }
            }
        }
        Dialect::GoogleTbs => put("tbs", q.google_tbs().map(Value::String)),
        Dialect::Cse => put("dateRestrict", q.cse_date_restrict().map(Value::String)),
        Dialect::Brave => put("freshness", q.brave_freshness().map(Value::String)),
        Dialect::Currents => p.extend(currents_params(q, now)),
        Dialect::Webz => p.extend(webz_params(q, now.date_naive())),
        Dialect::Tinyfish => {
            // It refuses `recency_minutes` combined with a date bound.
            match (q.recency, q.after, q.before) {
                (Some(r), None, None) => put("recency_minutes", Some(json!(r.days() * 1440))),
                _ => {
                    let (start, end) = q.tavily_dates();
                    put("after_date", start.map(Value::String));
                    put("before_date", end.map(Value::String));
                }
            }
        }
    }
    p
}

/// The free plan rejects a range over 7 days ("Max range is 7 days",
/// 2026-10-06), so a window up to `limits.max_window_days` is clamped to its
/// newest 7 days.
const CURRENTS_RANGE_DAYS: i64 = 7;

/// Currents keywords (quoted phrases, `-` exclusions), one domain each way,
/// and a date range with both ends.
pub fn currents_params(q: &QuerySpec, now: DateTime<Utc>) -> Vec<(String, Value)> {
    let mut keywords = vec![q.free_text_for_engines()];
    keywords.extend(q.exclude_words.iter().map(|w| format!("-{w}")));
    keywords.extend(q.exclude_phrases.iter().map(|p| format!("-\"{p}\"")));
    keywords.retain(|k| !k.trim().is_empty());
    let mut p: Vec<(String, Value)> = vec![("keywords".into(), json!(keywords.join(" ")))];
    if let Some(d) = q.sites.first() {
        p.push(("domain".into(), json!(d)));
    }
    if let Some(d) = q.exclude_sites.first() {
        p.push(("domain_not".into(), json!(d)));
    }
    if let Some(since) = q.since(now.date_naive()) {
        let end = q
            .before
            .and_then(|b| b.and_hms_opt(0, 0, 0))
            .map(|b| b.and_utc())
            .filter(|b| *b < now)
            .unwrap_or(now);
        let earliest =
            end - chrono::Duration::days(CURRENTS_RANGE_DAYS) + chrono::Duration::minutes(1);
        let start = since
            .and_hms_opt(0, 0, 0)
            .map(|s| s.and_utc())
            .unwrap_or(earliest)
            .max(earliest);
        let fmt = "%Y-%m-%dT%H:%M:%SZ";
        p.push(("start_date".into(), json!(start.format(fmt).to_string())));
        p.push(("end_date".into(), json!(end.format(fmt).to_string())));
    }
    p
}

/// Webz keeps 30 days of history; a window reaching further starts there.
const WEBZ_HISTORY_DAYS: i64 = 30;

/// Webz Lucene query (`site:`, `-site:`, `language:english`, `published:<ms`)
/// and the window start as `ts` (epoch ms).
pub fn webz_params(q: &QuerySpec, today: NaiveDate) -> Vec<(String, Value)> {
    let mut words = vec![q.free_text_for_engines()];
    words.extend(q.exclude_words.iter().map(|w| format!("-{w}")));
    words.extend(q.exclude_phrases.iter().map(|p| format!("-\"{p}\"")));
    match q.sites.len() {
        0 => {}
        1 => words.push(format!("site:{}", q.sites[0])),
        _ => words.push(format!(
            "({})",
            q.sites
                .iter()
                .take(15)
                .map(|s| format!("site:{s}"))
                .collect::<Vec<_>>()
                .join(" OR ")
        )),
    }
    words.extend(q.exclude_sites.iter().map(|s| format!("-site:{s}")));
    words.push("language:english".into());
    let epoch_ms = |d: NaiveDate| {
        d.and_hms_opt(0, 0, 0)
            .map(|t| t.and_utc().timestamp_millis())
    };
    if let Some(ms) = q.before.and_then(epoch_ms) {
        words.push(format!("published:<{ms}"));
    }
    words.retain(|k| !k.trim().is_empty());
    let mut p: Vec<(String, Value)> = vec![("q".into(), json!(words.join(" ")))];
    if let Some(ms) = q
        .since(today)
        .map(|s| s.max(today - chrono::Duration::days(WEBZ_HISTORY_DAYS)))
        .and_then(epoch_ms)
    {
        p.push(("ts".into(), json!(ms.to_string())));
    }
    p
}

/// Remove a credential from text that will be logged or returned. Webz
/// echoes the token in its 401 body ("Unknown API token <token>").
pub fn redact(text: &str, secret: &str) -> String {
    if secret.len() < 6 {
        return text.to_string();
    }
    text.replace(secret, "<redacted>")
}

fn query_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(query_string).collect::<Vec<_>>().join(","),
        other => other.to_string(),
    }
}

/// One engine's answer: the results, and what the call cost in the engine's
/// budget unit.
#[derive(Debug)]
pub struct Answer {
    pub items: Vec<Value>,
    pub units: f64,
}

/// What a successful call cost: the answer's own price for a usd budget
/// (else its default), one call otherwise.
pub fn units_of(spec: &EngineSpec, json: &Value) -> f64 {
    budget_units(spec.budget.as_ref(), json)
}

/// [`units_of`] for any provider with a budget (`page_readers` too).
pub fn budget_units(budget: Option<&Budget>, json: &Value) -> f64 {
    match budget {
        Some(b) if b.unit == BudgetUnit::Usd => b
            .cost_field
            .as_deref()
            .and_then(|p| path_get(json, p))
            .and_then(Value::as_f64)
            .or(b.cost_default)
            .unwrap_or(0.0),
        _ => 1.0,
    }
}

/// Call one engine. `Ok` is the result list in the ladder's shape
/// (`{title, link, snippet, date}`) and its cost; `Err` is text
/// `classify_search_error` buckets (a spent allowance says "quota exhausted").
pub async fn run(
    client: &Client,
    spec: &EngineSpec,
    q: &QuerySpec,
    count: u8,
) -> Result<Answer, String> {
    let name = &spec.name;
    let r = &spec.request;
    let key = match &spec.key_env {
        Some(env) => Some(
            env_nonempty(env).ok_or_else(|| format!("{name} key not configured (set {env})"))?,
        ),
        None => None,
    };
    let now = Utc::now();
    let mut p = params(spec, q, count, now);
    for (param, env) in &spec.env_params {
        let v = env_nonempty(env).ok_or_else(|| format!("{name} not configured (set {env})"))?;
        p.push((param.clone(), json!(v)));
    }

    let url = match (&r.news_url, q.news) {
        (Some(u), true) => u,
        _ => &r.url,
    };
    let url = if url.contains("{base}") {
        let base = r
            .base_url_env
            .as_deref()
            .and_then(env_nonempty)
            .or_else(|| r.base_url_default.clone())
            .unwrap_or_default();
        url.replace("{base}", base.trim_end_matches('/'))
    } else {
        url.clone()
    };

    let mut headers: Vec<(String, String)> = Vec::new();
    if let (Some(auth), Some(key)) = (&r.auth, &key) {
        match (&auth.header, &auth.query) {
            (Some(h), _) => headers.push((h.clone(), format!("{}{key}", auth.prefix))),
            (None, Some(param)) => p.push((param.clone(), json!(key))),
            (None, None) => {}
        }
    }

    let mut req = match r.method {
        Method::Get => {
            let pairs: Vec<(String, String)> = p
                .iter()
                .map(|(k, v)| (k.clone(), query_string(v)))
                .collect();
            client.get(&url).query(&pairs)
        }
        Method::Post => {
            let body: Map<String, Value> = p.into_iter().collect();
            client.post(&url).json(&body)
        }
    };
    for (h, v) in &headers {
        req = req.header(h.as_str(), v.as_str());
    }
    if let Some(secs) = r.timeout_secs {
        req = req.timeout(std::time::Duration::from_secs(secs));
    }
    let secret = key.as_deref().unwrap_or("");

    let resp = req
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", redact(&e.to_string(), secret)))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let lower = text.to_lowercase();
        let rs = &spec.response;
        let spent = rs.spent_status.contains(&status.as_u16())
            || rs
                .spent_words
                .iter()
                .any(|w| lower.contains(&w.to_lowercase()));
        let prefix = if spent { "quota exhausted — " } else { "" };
        return Err(format!(
            "{name} search failed: {prefix}HTTP {status} - {}",
            redact(&text, secret).chars().take(400).collect::<String>()
        ));
    }
    let json: Value =
        serde_json::from_str(&text).map_err(|e| format!("Failed to parse {name} response: {e}"))?;
    if let Some(field) = &spec.response.error_field
        && let Some(err) = json.get(field).and_then(Value::as_str)
    {
        return Err(format!("{name} error: {}", redact(err, secret)));
    }
    if !spec.response.log_fields.is_empty() {
        let fields: Map<String, Value> = spec
            .response
            .log_fields
            .iter()
            .map(|f| (f.clone(), json[f].clone()))
            .collect();
        let fields = Value::Object(fields);
        info!(engine = %name, fields = %fields, "Search engine reported");
    }
    Ok(Answer {
        items: results(spec, &json, count),
        units: units_of(spec, &json),
    })
}

/// The engine's answer mapped onto `{title, link, snippet, date}`.
pub fn results(spec: &EngineSpec, json: &Value, count: u8) -> Vec<Value> {
    let rs = &spec.response;
    let list = |path: &str| {
        path_get(json, path)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let items = match &rs.news_items {
        Some(p) if path_get(json, p).is_some_and(Value::is_array) => list(p),
        _ => list(&rs.items),
    };
    match rs.shape {
        Shape::GoogleNews => flatten_google_news(&items, count as usize),
        Shape::Items => items
            .iter()
            .filter_map(|r| map_item(rs, r))
            .take(count as usize)
            .collect(),
    }
}

/// `a.b.0.c` into a JSON value; numeric segments index arrays.
pub fn path_get<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(v, |cur, seg| match seg.parse::<usize>() {
            Ok(i) if cur.is_array() => cur.get(i),
            _ => cur.get(seg),
        })
}

fn map_item(rs: &ResponseSpec, r: &Value) -> Option<Value> {
    let link = path_get(r, &rs.link)
        .and_then(Value::as_str)
        .filter(|l| !l.is_empty())?;
    let snippet = rs
        .snippet
        .iter()
        .filter_map(|p| path_get(r, p))
        .find_map(|v| match v {
            Value::Array(a) => {
                let joined = a
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" … ");
                (!joined.is_empty()).then_some(joined)
            }
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let snippet = match rs.snippet_chars {
        Some(max) if snippet.chars().count() > max => format!(
            "{}…",
            snippet.chars().take(max).collect::<String>().trim_end()
        ),
        _ => snippet,
    };
    let mut out = json!({
        "title": path_get(r, &rs.title).cloned().unwrap_or(Value::Null),
        "link": link,
        "snippet": snippet,
    });
    if let Some(d) = path_get(r, &rs.date).filter(|d| !d.is_null()) {
        out["date"] = d.clone();
    }
    if let Some(src) = rs
        .source
        .as_deref()
        .and_then(|p| path_get(r, p))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        out["source"] = json!(src);
    }
    Some(out)
}

/// One Google News story as `{title, link, snippet, source, date}`; `None`
/// unless it has a title and a link (a group header has neither).
fn news_item(item: &Value) -> Option<Value> {
    let title = item.get("title").and_then(Value::as_str)?;
    let link = item.get("link").and_then(Value::as_str)?;
    Some(json!({
        "title":   title,
        "link":    link,
        "snippet": item.get("snippet").and_then(Value::as_str).unwrap_or(""),
        "source":  item.get("source").and_then(|s| s.get("name")).and_then(Value::as_str),
        "date":    item.get("date").and_then(Value::as_str),
    }))
}

/// Google News clusters some results: an entry may be a flat story, or carry a
/// `highlight` featured story and/or a `stories` topic cluster. All are
/// unrolled so a cluster contributes several headlines, then capped.
pub fn flatten_google_news(news_results: &[Value], count: usize) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for entry in news_results {
        if out.len() >= count {
            break;
        }
        if let Some(n) = news_item(entry) {
            out.push(n);
        }
        if out.len() < count
            && let Some(n) = entry.get("highlight").and_then(news_item)
        {
            out.push(n);
        }
        if let Some(stories) = entry.get("stories").and_then(Value::as_array) {
            for s in stories {
                if out.len() >= count {
                    break;
                }
                if let Some(n) = news_item(s) {
                    out.push(n);
                }
            }
        }
    }
    out.truncate(count);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_query::Recency;

    fn bundled() -> Vec<EngineSpec> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/core/search-engines");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("core search-engines dir")
            .flatten()
            .map(|e| e.path())
            .collect();
        files.sort();
        files
            .iter()
            .map(|p| {
                parse(&std::fs::read_to_string(p).unwrap())
                    .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
            })
            .collect()
    }

    fn engine(name: &str) -> EngineSpec {
        bundled().into_iter().find(|e| e.name == name).unwrap()
    }

    fn pairs(p: &[(String, Value)]) -> BTreeMap<String, Value> {
        p.iter().cloned().collect()
    }

    #[test]
    fn every_bundled_engine_parses_and_the_ladder_keeps_its_order() {
        let all = bundled();
        assert_eq!(all.len(), 10);
        assert_eq!(
            default_order(&all),
            [
                "exa", "tavily", "tinyfish", "serper", "serpapi", "google", "brave"
            ]
        );
    }

    #[test]
    fn the_rotation_holds_the_free_allowances_only() {
        let budgeted: Vec<String> = bundled()
            .into_iter()
            .filter(|e| e.budget.is_some())
            .map(|e| e.name)
            .collect();
        // SerpApi's 250/month is google_news's; Brave bills a card.
        assert_eq!(budgeted, ["exa", "serper", "tavily", "tinyfish"]);
        let exa = engine("exa");
        let b = exa.budget.as_ref().unwrap();
        assert_eq!((b.unit, b.per), (BudgetUnit::Usd, BudgetPeriod::Month));
    }

    #[test]
    fn a_usd_budget_without_a_default_price_is_refused() {
        let text = "name: x\nbudget: { free: 10, unit: usd, per: month }\n\
                    request: { method: get, url: https://x, query: { param: q, syntax: plain } }\n\
                    response: { items: r }";
        assert!(parse(text).unwrap_err().contains("cost_default"));
    }

    #[test]
    fn a_call_is_priced_from_the_answer_or_its_default() {
        let exa = engine("exa");
        assert_eq!(
            units_of(&exa, &json!({"costDollars": {"total": 0.012}})),
            0.012
        );
        assert_eq!(units_of(&exa, &json!({})), 0.007);
        assert_eq!(units_of(&engine("tavily"), &json!({})), 1.0);
    }

    #[test]
    fn tinyfish_takes_a_window_in_minutes_or_dates_never_both() {
        let tf = engine("tinyfish");
        let mut q = QuerySpec::parse("fed rate site:reuters.com");
        q.recency = crate::search_query::Recency::parse("week");
        q.news = true;
        let p = pairs(&params(&tf, &q, 5, Utc::now()));
        assert_eq!(p["recency_minutes"], json!(7 * 1440));
        assert_eq!(p["domain_type"], json!("news"));
        assert_eq!(p["include_domains"], json!(["reuters.com"]));
        assert!(!p.contains_key("after_date"));
        let q = QuerySpec::parse("fed rate after:2026-10-01");
        let p = pairs(&params(&tf, &q, 5, Utc::now()));
        assert_eq!(p["after_date"], json!("2026-10-01"));
        assert!(!p.contains_key("recency_minutes"));
    }

    #[test]
    fn tinyfish_news_maps_its_publisher() {
        let json = json!({"results": [{"url": "https://www.reuters.com/x", "title": "T",
            "snippet": "S", "date": "2 days ago", "publisher": "Reuters"}]});
        let r = results(&engine("tinyfish"), &json, 5);
        assert_eq!(r[0]["source"], json!("Reuters"));
        assert_eq!(r[0]["date"], json!("2 days ago"));
        assert_eq!(r[0]["link"], json!("https://www.reuters.com/x"));
    }

    #[test]
    fn a_broken_engine_is_refused_with_the_reason() {
        let bad = "name: x\nrequest: { method: get, url: ftp://x }\nresponse: { items: r }";
        assert!(parse(bad).unwrap_err().contains("http(s)"));
        let no_query = "name: x\nrequest: { method: get, url: https://x }\nresponse: { items: r }";
        assert!(parse(no_query).unwrap_err().contains("query"));
        let keyless_auth = "name: x\nkey_env: K\nrequest: { method: get, url: https://x, query: { param: q, syntax: plain } }\nresponse: { items: r }";
        assert!(parse(keyless_auth).unwrap_err().contains("auth"));
        assert!(parse("name: x\nbogus: 1").unwrap_err().contains("YAML"));
    }

    #[test]
    fn a_spent_serpapi_account_demotes_google_news_too_and_vice_versa() {
        let all = bundled();
        let s = quota_siblings(&all, &["serpapi".into()]);
        assert!(s.contains(&"google_news".to_string()));
        let n = quota_siblings(&all, &["google_news".into()]);
        assert!(n.contains(&"serpapi".to_string()));
        assert_eq!(
            quota_siblings(&all, &["exa".into()]),
            vec!["exa".to_string()]
        );
    }

    #[test]
    fn engines_that_cannot_serve_a_query_say_why() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 6).unwrap();
        let currents = engine("currents");
        let mut q = QuerySpec::parse("fed rates site:a.com site:b.com");
        assert!(
            unsupported(&currents, &q, today)
                .unwrap()
                .contains("domain")
        );
        q = QuerySpec::parse("fed rates");
        q.recency = Some(Recency::Month);
        assert!(
            unsupported(&currents, &q, today)
                .unwrap()
                .contains("8 days")
        );
        q.recency = Some(Recency::Week);
        assert_eq!(unsupported(&currents, &q, today), None);
        let webz = engine("webz");
        assert!(unsupported(&webz, &QuerySpec::parse("report filetype:pdf"), today).is_some());
        assert_eq!(
            unsupported(&engine("exa"), &QuerySpec::parse("x filetype:pdf"), today),
            None
        );
    }

    #[test]
    fn exa_sends_domains_dates_and_news_as_parameters() {
        let mut q = QuerySpec::parse("chip export rules site:reuters.com -site:x.com");
        q.news = true;
        q.recency = Some(Recency::Day);
        let p = pairs(&params(&engine("exa"), &q, 5, Utc::now()));
        assert_eq!(p["query"], json!("chip export rules"));
        assert_eq!(p["numResults"], json!(5));
        assert_eq!(p["category"], json!("news"));
        assert_eq!(p["includeDomains"], json!(["reuters.com"]));
        assert_eq!(p["excludeDomains"], json!(["x.com"]));
        assert!(p.contains_key("startPublishedDate"));
        assert_eq!(p["type"], json!("auto"));
    }

    #[test]
    fn tavily_uses_time_range_for_a_recency_window() {
        let mut q = QuerySpec::parse("\"budget vote\" council");
        q.recency = Some(Recency::Week);
        let p = pairs(&params(&engine("tavily"), &q, 3, Utc::now()));
        assert_eq!(p["time_range"], json!("week"));
        assert_eq!(p["exact_match"], json!(true));
        assert_eq!(p["max_results"], json!(3));
    }

    #[test]
    fn a_count_is_capped_at_the_engines_max() {
        let p = pairs(&params(
            &engine("webz"),
            &QuerySpec::parse("x"),
            20,
            Utc::now(),
        ));
        assert_eq!(p["size"], json!(5));
    }

    #[test]
    fn currents_dates_are_clamped_to_seven_days_and_domains_mapped() {
        let now = DateTime::parse_from_rfc3339("2026-10-06T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut q = QuerySpec::parse("refinery fire site:freep.com -site:x.com");
        q.recency = Some(Recency::Week);
        let p = pairs(&currents_params(&q, now));
        assert_eq!(p["domain"], json!("freep.com"));
        assert_eq!(p["domain_not"], json!("x.com"));
        assert_eq!(p["end_date"], json!("2026-10-06T12:00:00Z"));
        assert_eq!(p["start_date"], json!("2026-09-29T12:01:00Z"));
    }

    #[test]
    fn webz_queries_carry_sites_language_and_the_window() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 6).unwrap();
        let mut q = QuerySpec::parse("grid outage site:a.com site:b.com -site:c.com");
        q.recency = Some(Recency::Week);
        let p = pairs(&webz_params(&q, today));
        let text = p["q"].as_str().unwrap();
        assert!(text.contains("(site:a.com OR site:b.com)"));
        assert!(text.contains("-site:c.com"));
        assert!(text.contains("language:english"));
        assert!(p.contains_key("ts"));
    }

    #[test]
    fn get_parameters_render_as_strings() {
        assert_eq!(query_string(&json!(5)), "5");
        assert_eq!(query_string(&json!("x")), "x");
        assert_eq!(query_string(&json!(["a", "b"])), "a,b");
    }

    #[test]
    fn credentials_are_redacted_from_error_text() {
        assert_eq!(
            redact("Unknown API token abc123xyz", "abc123xyz"),
            "Unknown API token <redacted>"
        );
        assert_eq!(redact("short", "abc"), "short");
    }

    #[test]
    fn exa_highlights_become_the_snippet_and_text_is_the_fallback() {
        let json = json!({"results": [
            {"title": "A", "url": "https://a", "highlights": ["one", "two"], "publishedDate": "2026-10-01"},
            {"title": "B", "url": "https://b", "highlights": [], "text": "body"},
            {"title": "C", "url": ""},
        ]});
        let r = results(&engine("exa"), &json, 5);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0]["snippet"], json!("one … two"));
        assert_eq!(r[0]["date"], json!("2026-10-01"));
        assert_eq!(r[1]["snippet"], json!("body"));
    }

    #[test]
    fn tavily_snippets_are_clipped_to_the_budget() {
        let long = "x".repeat(800);
        let json = json!({"results": [{"title": "T", "url": "https://t", "content": long}]});
        let r = results(&engine("tavily"), &json, 5);
        assert_eq!(r[0]["snippet"].as_str().unwrap().chars().count(), 501);
    }

    #[test]
    fn serper_news_answers_under_news_and_brave_maps_to_the_common_shape() {
        let json =
            json!({"news": [{"title": "N", "link": "https://n", "snippet": "s", "date": "1h"}]});
        let r = results(&engine("serper"), &json, 5);
        assert_eq!(r[0]["link"], json!("https://n"));
        let brave = json!({"web": {"results": [{"title": "B", "url": "https://b", "description": "d", "age": "2d"}]}});
        let r = results(&engine("brave"), &brave, 5);
        assert_eq!(
            r[0],
            json!({"title": "B", "link": "https://b", "snippet": "d", "date": "2d"})
        );
    }

    #[test]
    fn google_news_unrolls_highlights_and_clusters_and_caps() {
        let json = json!({"news_results": [
            {"title": "Flat", "link": "https://f", "source": {"name": "Freep"}},
            {"highlight": {"title": "H", "link": "https://h"},
             "stories": [{"title": "S1", "link": "https://s1"}, {"title": "S2", "link": "https://s2"}]},
            {"title": "Header only"},
        ]});
        let r = results(&engine("google_news"), &json, 3);
        let links: Vec<_> = r.iter().map(|v| v["link"].as_str().unwrap()).collect();
        assert_eq!(links, ["https://f", "https://h", "https://s1"]);
        assert_eq!(r[0]["source"], json!("Freep"));
    }

    /// Mapped results must feed the claim verifier's domain count
    /// (epistemics' `evidence_domains` reads `link`).
    #[test]
    fn mapped_results_count_as_evidence_domains() {
        let json = json!({"organic": [{"title": "1", "link": "https://www.a.com/x"},
                                      {"title": "2", "link": "https://b.org/y"}]});
        let out = results(&engine("serper"), &json, 5);
        let domains: Vec<String> = out
            .iter()
            .filter_map(|i| i["link"].as_str())
            .filter_map(agent_brain_knowledge::web_result::registrable_domain)
            .collect();
        assert_eq!(domains, vec!["a.com".to_string(), "b.org".to_string()]);
    }

    #[test]
    fn dot_paths_index_objects_and_arrays() {
        let v = json!({"web": {"results": [{"a": 1}]}});
        assert_eq!(path_get(&v, "web.results.0.a"), Some(&json!(1)));
        assert_eq!(path_get(&v, "web.missing"), None);
    }
}
