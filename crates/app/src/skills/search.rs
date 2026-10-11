//! Search Skill — web search with credentials loaded from ApiContext nodes.
//!
//! API keys are no longer stored as skill fields. Instead each engine's
//! `ApiContext` node in Neo4j holds the env var name; credentials are
//! resolved at call time via `std::env::var`.  Falls back to the legacy
//! env vars (`SERPAPI_KEY`, `BRAVE_API_KEY`, etc.) when no context is found
//! so existing deployments keep working without a schema migration.

use std::time::Instant;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::web_result::{SEARXNG_FALLBACK_BANNER, is_search_page};
use crate::skills::Skill;
use crate::skills::search_query::{QuerySpec, Recency};
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};
use chrono::NaiveDate;

/// Failover order when `SEARCH_ENGINE_ORDER` is unset.
///
/// SearXNG is the LAST resort before Brave, not the lead (2026-10-06). It is
/// unmetered, but with the mainstream engines CAPTCHA-blocked its answers come
/// from Naver, Seznam and Zapmeta: weaker relevance, undated, foreign-index
/// pages, and with a `recency` window only Naver answers at all (a "Fed rate
/// decision, past day" search returned Nigerian and Indian central-bank
/// stories). Because it always returns *something*, leading with it meant the
/// keyed engines were never reached — 720 SearXNG searches against 0 Tavily or
/// Serper in a week. When it does answer, `search_web` says so in a banner
/// (`SEARXNG_FALLBACK_BANNER`). Brave stays behind it: card on file, no spend
/// cap, so it is only for when SearXNG is down too.
///
/// Among the keyed engines, the ones whose free allowance REFILLS monthly with
/// no card on file come first (Exa, then Tavily — overage is impossible by
/// construction; Exa leads because it surfaced the primary source — the bill,
/// the PR, the paper — on 4 of 5 probe queries where Tavily returned blog
/// posts about it, 2026-09-26), then Serper's one-time 2,500, then SerpApi (whose quota the
/// news brief's `google_news` needs), then Google CSE (closed to new customers,
/// ends 2027-01-01) and Brave (card on file, no spend cap). An engine without a
/// key is dropped from the ladder, so listing one costs nothing.
///
/// Currents and Webz are NOT in the ladder; they run only when a caller names
/// them (`engine: "currents"` / `"webz"`). Measured 2026-10-06 on the brain's
/// own schedule queries, 7-day news searches: Currents ANDs every keyword and
/// returned 0 results for all four long queries; Webz returned QAnon-style,
/// partisan and off-topic pages (a Michigan politics query got Bangladesh and
/// Nigeria) — not one result from any SourceList — and, being non-empty,
/// stopped the ladder before Exa/Tavily. Leading news searches with them was
/// reverted the same day.
const DEFAULT_ENGINE_ORDER: &[&str] = &[
    "exa", "tavily", "serper", "serpapi", "google", "searxng", "brave",
];

/// Currents' free plan rejects a date range over 7 days ("Max range is 7
/// days", 2026-10-06); a window is clamped to the newest 7 days when it is at
/// most this long (a "week" recency from midnight is 7 days and a few hours),
/// and the engine is skipped for anything longer.
const CURRENTS_MAX_WINDOW_DAYS: i64 = 8;
/// Results per call on Currents' free plan.
const CURRENTS_MAX_RESULTS: u8 = 20;
/// Webz search holds 30 days of history here. A window up to this long (a
/// "month" recency is 31 days) is clamped to those 30; longer is skipped.
const WEBZ_MAX_WINDOW_DAYS: i64 = 31;
const WEBZ_HISTORY_DAYS: i64 = 30;
/// Characters of Webz's full article `text` kept per result.
const WEBZ_SNIPPET_CHARS: usize = 700;
/// Results per Webz call. The self-serve plan bills prepaid credit per result
/// (~$0.01 each, measured 2026-10-06: `cost: 0.02` for 2), so it stays small.
const WEBZ_MAX_RESULTS: u8 = 5;

/// Characters of page text Exa returns per result as a highlight. Exa bills
/// contents per result, so this is a snippet, not the page.
const EXA_HIGHLIGHT_CHARS: u32 = 400;

/// Characters of Tavily's `content` kept per result. Tavily returns up to three
/// page chunks (~1,500 chars measured 2026-09-26) — 3-10x the other engines —
/// so a 10-result answer would put ~15K chars into a tool loop's context.
const TAVILY_SNIPPET_CHARS: usize = 500;

/// Trim to `max` chars on a char boundary, marking the cut.
fn clip_snippet(v: &Value, max: usize) -> Value {
    match v.as_str() {
        Some(t) if t.chars().count() > max => Value::String(format!(
            "{}…",
            t.chars().take(max).collect::<String>().trim_end()
        )),
        _ => v.clone(),
    }
}

/// Base URL used when neither the `searxng` ApiContext nor `SEARXNG_URL` is set.
/// Matches the compose service name so the default deployment works unconfigured.
const DEFAULT_SEARXNG_URL: &str = "http://searxng:8080";

/// SearXNG fans out to upstream engines in series; the default reqwest timeout
/// is generous enough to stall a chain behind one slow upstream.
const SEARXNG_TIMEOUT_SECS: u64 = 20;

/// How long a quota-exhausted engine stays demoted to the back of the ladder.
/// Sized for daily-reset quotas (Google CSE); a monthly cap simply stays demoted
/// because it keeps re-recording the error on each retry.
const QUOTA_COOLDOWN_HOURS: i64 = 6;

/// Extract `(engine, reason)` pairs from a SearXNG response's
/// `unresponsive_engines` field.
///
/// The field is an array of arrays — `[["naver", "access denied"], …]` — and
/// entries carry either one or two elements depending on version and failure
/// kind. Anything unparseable is skipped rather than guessed at: this feeds a
/// diagnostic log line, and a wrong reason is worse than a missing one.
fn parse_unresponsive_engines(json: &Value) -> Vec<(String, String)> {
    json.get("unresponsive_engines")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    let parts = e.as_array()?;
                    let name = parts.first()?.as_str()?.to_string();
                    let reason = parts
                        .get(1)
                        .and_then(Value::as_str)
                        .unwrap_or("unspecified")
                        .to_string();
                    Some((name, reason))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Normalize one SerpApi `google_news` story into the `{title, link, snippet,
/// source, date}` shape the rest of the ladder emits. Returns `None` unless the
/// story has both a title and a link — a group header with neither is not a
/// citable result.
fn normalize_news_item(item: &Value) -> Option<Value> {
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

/// Flatten a SerpApi `google_news` `news_results` array into a normalized list.
///
/// Google News clusters some results: an entry may be a flat story, or carry a
/// `highlight` featured story and/or a `stories` sub-array (a topic cluster).
/// All are unrolled so a clustered topic contributes several headlines rather
/// than one opaque group, then the list is capped at `count`.
/// Map one engine's result list onto the ladder's common shape:
/// `{title, link, snippet, date}` — `link` is what `evidence_domains`, the
/// source-list post-filter and the numbered source marker all read.
fn normalized(title: &Value, link: &Value, snippet: Value, date: &Value) -> Option<Value> {
    let link = link.as_str().filter(|l| !l.is_empty())?;
    let mut out = json!({ "title": title, "link": link, "snippet": snippet });
    if !date.is_null() {
        out["date"] = date.clone();
    }
    Some(out)
}

fn take_results<'a>(json: &'a Value, key: &str, count: u8) -> impl Iterator<Item = &'a Value> {
    json.get(key)
        .and_then(|r| r.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .take(count as usize)
}

fn normalize_tavily(json: &Value, count: u8) -> Vec<Value> {
    take_results(json, "results", count)
        .filter_map(|r| {
            normalized(
                &r["title"],
                &r["url"],
                clip_snippet(&r["content"], TAVILY_SNIPPET_CHARS),
                &r["published_date"],
            )
        })
        .collect()
}

fn normalize_exa(json: &Value, count: u8) -> Vec<Value> {
    take_results(json, "results", count)
        .filter_map(|r| {
            let snippet = r["highlights"]
                .as_array()
                .map(|h| {
                    h.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(" … ")
                })
                .filter(|s| !s.is_empty())
                .map(Value::String)
                .unwrap_or_else(|| r["text"].clone());
            normalized(&r["title"], &r["url"], snippet, &r["publishedDate"])
        })
        .collect()
}

fn normalize_serper(json: &Value, count: u8) -> Vec<Value> {
    let key = if json["news"].is_array() {
        "news"
    } else {
        "organic"
    };
    take_results(json, key, count)
        .filter_map(|r| normalized(&r["title"], &r["link"], r["snippet"].clone(), &r["date"]))
        .collect()
}

fn normalize_currents(json: &Value, count: u8) -> Vec<Value> {
    take_results(json, "news", count)
        .filter_map(|r| {
            normalized(
                &r["title"],
                &r["url"],
                r["description"].clone(),
                &r["published"],
            )
        })
        .collect()
}

fn normalize_webz(json: &Value, count: u8) -> Vec<Value> {
    take_results(json, "posts", count)
        .filter_map(|r| {
            normalized(
                &r["title"],
                &r["url"],
                clip_snippet(&r["text"], WEBZ_SNIPPET_CHARS),
                &r["published"],
            )
        })
        .collect()
}

/// Remove a credential from text that will be logged or returned. Webz
/// echoes the token in its 401 body ("Unknown API token <token>").
fn redact(text: &str, secret: &str) -> String {
    if secret.len() < 6 {
        return text.to_string();
    }
    text.replace(secret, "<redacted>")
}

/// Why an engine cannot serve this query, if it cannot. Checked before the
/// call, so a skip costs no quota and is not recorded as a failure.
fn engine_unsupported(engine: &str, spec: &QuerySpec, today: NaiveDate) -> Option<String> {
    let window_days = spec.since(today).map(|s| (today - s).num_days());
    match engine {
        "currents" => {
            if spec.sites.len() > 1 {
                Some("restricts to one domain only".into())
            } else if spec.exclude_sites.len() > 1 {
                Some("excludes one domain only".into())
            } else if spec.filetype.is_some() {
                Some("no filetype filter".into())
            } else if window_days.is_some_and(|d| d > CURRENTS_MAX_WINDOW_DAYS) {
                Some(format!(
                    "free plan searches at most {CURRENTS_MAX_WINDOW_DAYS} days"
                ))
            } else {
                None
            }
        }
        "webz" => {
            if spec.filetype.is_some() {
                Some("no filetype filter".into())
            } else if window_days.is_some_and(|d| d > WEBZ_MAX_WINDOW_DAYS) {
                Some(format!("Lite searches at most {WEBZ_MAX_WINDOW_DAYS} days"))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Currents query parameters. A window longer than 7 days (never more than
/// `CURRENTS_MAX_WINDOW_DAYS` here — `engine_unsupported` skips longer) is
/// clamped to its newest 7 days, because the free plan rejects more.
fn currents_params(
    spec: &QuerySpec,
    count: u8,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<(String, String)> {
    let mut keywords = vec![spec.free_text_for_engines()];
    keywords.extend(spec.exclude_words.iter().map(|w| format!("-{w}")));
    keywords.extend(spec.exclude_phrases.iter().map(|p| format!("-\"{p}\"")));
    keywords.retain(|k| !k.trim().is_empty());
    let mut p: Vec<(String, String)> = vec![
        ("keywords".into(), keywords.join(" ")),
        ("language".into(), "en".into()),
        (
            "page_size".into(),
            count.min(CURRENTS_MAX_RESULTS).to_string(),
        ),
    ];
    if let Some(d) = spec.sites.first() {
        p.push(("domain".into(), d.clone()));
    }
    if let Some(d) = spec.exclude_sites.first() {
        p.push(("domain_not".into(), d.clone()));
    }
    if let Some(since) = spec.since(now.date_naive()) {
        let end = spec
            .before
            .and_then(|b| b.and_hms_opt(0, 0, 0))
            .map(|b| b.and_utc())
            .filter(|b| *b < now)
            .unwrap_or(now);
        let earliest = end - chrono::Duration::days(7) + chrono::Duration::minutes(1);
        let start = since
            .and_hms_opt(0, 0, 0)
            .map(|s| s.and_utc())
            .unwrap_or(earliest)
            .max(earliest);
        let fmt = "%Y-%m-%dT%H:%M:%SZ";
        p.push(("start_date".into(), start.format(fmt).to_string()));
        p.push(("end_date".into(), end.format(fmt).to_string()));
    }
    p
}

/// Webz Lite query parameters (without the token).
fn webz_params(spec: &QuerySpec, today: NaiveDate) -> Vec<(String, String)> {
    let mut q = vec![spec.free_text_for_engines()];
    q.extend(spec.exclude_words.iter().map(|w| format!("-{w}")));
    q.extend(spec.exclude_phrases.iter().map(|p| format!("-\"{p}\"")));
    match spec.sites.len() {
        0 => {}
        1 => q.push(format!("site:{}", spec.sites[0])),
        _ => q.push(format!(
            "({})",
            spec.sites
                .iter()
                .take(15)
                .map(|s| format!("site:{s}"))
                .collect::<Vec<_>>()
                .join(" OR ")
        )),
    }
    q.extend(spec.exclude_sites.iter().map(|s| format!("-site:{s}")));
    q.push("language:english".into());
    let epoch_ms = |d: NaiveDate| {
        d.and_hms_opt(0, 0, 0)
            .map(|t| t.and_utc().timestamp_millis())
    };
    if let Some(ms) = spec.before.and_then(epoch_ms) {
        q.push(format!("published:<{ms}"));
    }
    q.retain(|k| !k.trim().is_empty());
    let mut p: Vec<(String, String)> = vec![("q".into(), q.join(" "))];
    if let Some(ms) = spec
        .since(today)
        .map(|s| s.max(today - chrono::Duration::days(WEBZ_HISTORY_DAYS)))
        .and_then(epoch_ms)
    {
        p.push(("ts".into(), ms.to_string()));
    }
    p
}

fn flatten_google_news(news_results: &[Value], count: usize) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for entry in news_results {
        if out.len() >= count {
            break;
        }
        if let Some(n) = normalize_news_item(entry) {
            out.push(n);
        }
        if out.len() < count
            && let Some(h) = entry.get("highlight")
            && let Some(n) = normalize_news_item(h)
        {
            out.push(n);
        }
        if let Some(stories) = entry.get("stories").and_then(Value::as_array) {
            for s in stories {
                if out.len() >= count {
                    break;
                }
                if let Some(n) = normalize_news_item(s) {
                    out.push(n);
                }
            }
        }
    }
    out.truncate(count);
    out
}

/// Render engine failures for a log line: `naver (access denied), wikipedia (…)`.
/// Map SearXNG results onto the ladder's shape, keeping `publishedDate` as
/// `date` and dropping a dated result outside `[since, before]`. Undated
/// results are kept — the fallback banner says the window was not enforced on
/// them. Pure so the date rule is testable without a live SearXNG.
fn searxng_items(
    json: &Value,
    count: u8,
    since: Option<NaiveDate>,
    before: Option<NaiveDate>,
) -> Vec<Value> {
    let empty = vec![];
    json.get("results")
        .and_then(|r| r.as_array())
        .unwrap_or(&empty)
        .iter()
        .filter(|item| {
            let Some(d) = item
                .get("publishedDate")
                .and_then(|v| v.as_str())
                .and_then(|s| s.get(..10))
                .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
            else {
                return true;
            };
            since.is_none_or(|s| d >= s) && before.is_none_or(|b| d < b)
        })
        .take(count as usize)
        .map(|item| {
            json!({
                "title":   item.get("title"),
                "link":    item.get("url"),
                "snippet": item.get("content"),
                "date":    item.get("publishedDate"),
            })
        })
        .collect()
}

/// The warning that leads a SearXNG answer. Ends without a blank line; the
/// caller adds one, which `strip_fallback_banner` splits on.
fn searxng_banner(why: &str, recency_requested: bool) -> String {
    let mut b = format!(
        "{SEARXNG_FALLBACK_BANNER} — {why}.\n\
         These results come from SearXNG's working backends (Naver, Seznam, Zapmeta, \
         Wiby, Wikipedia): weaker relevance, usually undated, often foreign-index \
         pages. Treat them as leads to check, not as confirmation."
    );
    if recency_requested {
        b.push_str(
            "\nThe date window was enforced only on results that carry a date; \
             undated ones may be old.",
        );
    }
    b
}

fn format_unresponsive(failures: &[(String, String)]) -> String {
    failures
        .iter()
        .map(|(engine, reason)| format!("{engine} ({reason})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Classify an engine failure for the usage ledger.
///
/// `quota_exhausted` is the load-bearing case — it is what turns "the brain
/// silently stopped learning" into a queryable, alertable event.
fn classify_search_error(error_text: &str) -> &'static str {
    let lower = error_text.to_lowercase();
    if lower.contains("run out of searches")
        || lower.contains("not enough credits")
        || lower.contains("out of credits")
        || lower.contains("quota")
        || lower.contains("rate limit")
        || lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("insufficient credits")
    {
        "quota_exhausted"
    } else if lower.contains("not configured") {
        "not_configured"
    } else if lower.contains("has not been used in project") || lower.contains("is disabled") {
        "api_disabled"
    } else if lower.contains("401") || lower.contains("403") || lower.contains("unauthorized") {
        "auth_error"
    } else if lower.contains("request failed") || lower.contains("timed out") {
        "network_error"
    } else {
        "api_error"
    }
}

/// Build the ordered engine ladder. Pure so the ordering rules are testable
/// without a telemetry database or a live environment.
///
/// `configured` is the raw `SEARCH_ENGINE_ORDER` value (empty = use the
/// default), `requested` is the caller's preferred engine, and `exhausted`
/// lists engines that recently reported a spent quota.
fn order_engines(configured: &str, requested: Option<&str>, exhausted: &[String]) -> Vec<String> {
    let mut ladder: Vec<String> = configured
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if ladder.is_empty() {
        ladder = DEFAULT_ENGINE_ORDER.iter().map(|s| s.to_string()).collect();
    }

    // A requested engine goes to the head but never truncates the ladder:
    // the caller wants an answer more than it wants that specific engine.
    if let Some(req) = requested.map(|r| r.trim().to_lowercase())
        && !req.is_empty()
    {
        ladder.retain(|e| *e != req);
        ladder.insert(0, req);
    }

    // Demote rather than drop: a daily quota that reset overnight recovers by
    // itself, but must not cost a wasted round-trip on every search until then.
    if !exhausted.is_empty() {
        let (fresh, cooling): (Vec<String>, Vec<String>) =
            ladder.into_iter().partition(|e| !exhausted.contains(e));
        ladder = fresh;
        ladder.extend(cooling);
    }

    ladder
}

/// Drop engines with no working credentials from an already-ordered ladder so
/// they don't burn a rung failing `not_configured` on every single search.
/// `keep` is the subset of engines known to be configured (searxng always is —
/// it needs no key). Unlike quota exhaustion (demote-and-recover) a missing key
/// never fixes itself at runtime, so these are removed outright.
///
/// Never empties the ladder: if `keep` is empty (nothing configured at all) the
/// original ladder is returned so the caller still attempts a search and
/// surfaces a real per-engine error rather than silently doing nothing. Pure so
/// the rule is testable without env vars or a live graph.
fn retain_configured(ladder: Vec<String>, keep: &[String]) -> Vec<String> {
    let filtered: Vec<String> = ladder
        .iter()
        .filter(|e| keep.iter().any(|k| k == *e))
        .cloned()
        .collect();
    if filtered.is_empty() {
        ladder
    } else {
        filtered
    }
}

pub struct SearchSkill {
    client: Client,
    telemetry: Option<TelemetryClient>,
    neo4j: Option<Neo4jClient>,
    /// Used only by the "do we already know this?" probe, and only through
    /// `search_notes_readonly` so the probe never bumps a note's access count.
    /// Optional: absent it, the probe falls back to BM25 over the raw query.
    knowledge: Option<std::sync::Arc<dyn crate::services::KnowledgeStore>>,
}

impl SearchSkill {
    pub fn new(telemetry: Option<TelemetryClient>, neo4j: Option<Neo4jClient>) -> Self {
        Self {
            client: Client::new(),
            telemetry,
            neo4j,
            knowledge: None,
        }
    }

    /// Give the prior-knowledge probe hybrid (vector + BM25) recall.
    ///
    /// BM25 alone misses a paraphrase, which is most of what "do we already
    /// know this?" has to catch — the stored note rarely uses the searcher's
    /// wording. `search_notes_readonly` cannot fail the search: an embed error
    /// degrades it to BM25 internally, which is exactly the hand-rolled
    /// behaviour this replaces.
    pub fn with_knowledge(
        mut self,
        knowledge: std::sync::Arc<dyn crate::services::KnowledgeStore>,
    ) -> Self {
        self.knowledge = Some(knowledge);
        self
    }

    // =========================================================================
    // Credential resolution
    // =========================================================================

    /// Load the API key for a named ApiContext.
    /// Queries Neo4j for `auth_env_var`, then resolves from environment.
    /// Falls back to `fallback_env_var` when no context is found or the env
    /// var named by the context is unset.
    async fn resolve_key(&self, context_name: &str, fallback_env_var: &str) -> Option<String> {
        // Try ApiContext first
        if let Some(ref neo4j) = self.neo4j {
            let cypher = "MATCH (c:ApiContext {name: $name}) \
                          RETURN c.auth_env_var AS auth_env_var LIMIT 1";
            if let Ok(rows) = neo4j
                .execute(neo4rs::query(cypher).param("name", context_name))
                .await
                && let Some(env_var) = rows
                    .first()
                    .and_then(|r| r.get::<String>("auth_env_var").ok())
                && let Ok(val) = std::env::var(&env_var)
            {
                return Some(val);
            }
        }
        // Direct env var fallback
        std::env::var(fallback_env_var).ok()
    }

    /// Load a non-auth config value from ApiContext (e.g. Google CX).
    async fn resolve_context_field(
        &self,
        context_name: &str,
        field: &str,
        fallback_env_var: &str,
    ) -> Option<String> {
        if let Some(ref neo4j) = self.neo4j {
            let cypher =
                format!("MATCH (c:ApiContext {{name: $name}}) RETURN c.{field} AS val LIMIT 1");
            if let Ok(rows) = neo4j
                .execute(neo4rs::query(&cypher).param("name", context_name))
                .await
                && let Some(val) = rows.first().and_then(|r| r.get::<String>("val").ok())
                && !val.is_empty()
            {
                return Some(val);
            }
        }
        std::env::var(fallback_env_var).ok()
    }

    // ========================================================================
    // Tool Definitions
    // ========================================================================

    fn search_web_def() -> ToolDefinition {
        ToolDefinition {
            name: "search_web".to_string(),
            description:
                "Search the web. Tries engines in a failover ladder (keyed engines first; self-hosted SearXNG only as a flagged last resort) so \
                 one exhausted quota does not fail the call. \
                 QUERY SYNTAX — Google operators work on EVERY engine (translated for the ones that \
                 do not read them): \"exact phrase\", -word, -\"phrase\", site:example.com, \
                 -site:example.com, filetype:pdf, after:2026-09-01, before:2026-10-01, a OR b. \
                 Use them: site:/-site: to target or exclude outlets, quotes for names and titles, \
                 -word to drop a wrong sense. \
                 FOR CURRENT EVENTS set recency (day/week/month/year) and news:true instead of \
                 writing a date or 'today' into the query — engines rank by relevance, so 'today' \
                 alone returns months-old pages. Phrase it as 'latest …'. \
                 engine=\"exa\" is best for primary sources and dated news; \"google_news\" is \
                 SerpApi's Google News (250/month shared quota). \
                 Pass source_list to restrict results to an approved domain list stored in the graph."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    },
                    "engine": {
                        "type": "string",
                        "description": "Preferred engine, tried first. Omit to use the configured \
                                        ladder (default: exa → tavily → serper → serpapi → google → searxng → brave; currents and webz only when named here; \
                                        engines without a key are skipped). \
                                        Remaining engines are still used as fallbacks. \
                                        'google_news' is SerpApi's Google News engine (news only).",
                        "enum": ["searxng", "tavily", "exa", "serper", "serpapi", "brave", "google", "google_news", "currents", "webz"]
                    },
                    "count": {
                        "type": "integer",
                        "description": "Number of results to return (default: 5, max: 20)"
                    },
                    "recency": {
                        "type": "string",
                        "enum": ["day", "week", "month", "year"],
                        "description": "Only results from this window, applied through each engine's \
                                        own date filter. Use it for anything current instead of \
                                        putting a date or 'today' in the query."
                    },
                    "news": {
                        "type": "boolean",
                        "description": "News results only (news vertical on engines that have one)."
                    },
                    "source_list": {
                        "type": "string",
                        "description": "Name of a SourceList node in Neo4j (e.g. 'news'). \
                                        When set, restricts results to approved domains only — \
                                        adds site: operators to the query and post-filters results."
                    },
                    "prior_knowledge": {
                        "type": "boolean",
                        "description": "Prefix results with matching notes, APIs and media the graph \
                                        already holds (default: true). Set false when you need \
                                        EXTERNAL evidence only — e.g. verifying a claim, where our \
                                        own notes restating it are not corroboration."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn get_search_usage_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_search_usage".to_string(),
            description:
                "THE answer to any question about our own web searches: which search engines \
                 (searxng, exa, tavily, serper, google_news, …) answered, how many searches each, \
                 how many failed and why, and a per-day breakdown — read from the telemetry \
                 ledger, exact. Use it for \"which engines did we use\", \"how many searches\", \
                 quota burn against a free-tier cap. Notes do not record this; never infer it \
                 from them. (LLM models and providers are a different question: get_llm_budget.)"
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "window_hours": {
                        "type": "integer",
                        "description": "Restrict to the last N hours (default 720, i.e. ~30 days, \
                                        which matches a monthly quota window). Omit for all time."
                    }
                }
            }),
        }
    }

    /// The approved domains of a SourceList node (empty when it does not
    /// resolve, which degrades to an unrestricted search). They become `site:`
    /// for the Google family and include-domain parameters for Exa/Tavily, and
    /// the results are post-filtered against them either way.
    async fn source_list_domains(&self, source_list_name: &str) -> Vec<String> {
        let Some(ref neo4j) = self.neo4j else {
            return vec![];
        };
        neo4j
            .get_source_list(source_list_name)
            .await
            .unwrap_or_default()
    }

    /// Return true if `url` matches any approved domain (host ends with domain or equals it).
    fn url_matches_any(url: &str, domains: &[String]) -> bool {
        if domains.is_empty() {
            return true;
        }
        // Extract host from URL cheaply — everything between "://" and the next "/".
        let host = url
            .split_once("://")
            .map(|x| x.1)
            .unwrap_or(url)
            .split('/')
            .next()
            .unwrap_or(url)
            .split(':')
            .next()
            .unwrap_or(url)
            .to_lowercase();
        domains
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}")))
    }

    // ========================================================================
    // Engine failover
    // ========================================================================

    /// Ordered list of engines to try, most-preferred first.
    ///
    /// Base order comes from `SEARCH_ENGINE_ORDER` (comma-separated), defaulting
    /// to `DEFAULT_ENGINE_ORDER` — self-hosted SearXNG leads because it is the
    /// only entry with no quota to exhaust.
    ///
    /// An explicitly requested `engine` is promoted to the head but does **not**
    /// truncate the ladder: callers asking for a specific engine still want an
    /// answer more than they want that engine.
    ///
    /// Engines that hit `quota_exhausted` in the last `QUOTA_COOLDOWN_HOURS` are
    /// moved to the back rather than dropped — a quota that reset overnight
    /// should recover on its own, but should not cost a round-trip on all eight
    /// searches of the daily news chain until it does.
    async fn engine_ladder(&self, requested: Option<&str>) -> Vec<String> {
        let configured = std::env::var("SEARCH_ENGINE_ORDER").unwrap_or_default();
        let mut exhausted = self
            .telemetry
            .as_ref()
            .and_then(|t| {
                t.search_engines_with_recent_errors("quota_exhausted", QUOTA_COOLDOWN_HOURS)
                    .ok()
            })
            .unwrap_or_default();
        // `serpapi` (Google organic) and `google_news` are two engines on ONE
        // SerpApi account, so they share the monthly quota. A quota error is
        // recorded against whichever was called, but exhausting the account
        // exhausts both — demote the sibling too, or the ladder keeps hammering
        // a spent account through the other engine name.
        let has_serpapi = exhausted.iter().any(|e| e == "serpapi");
        let has_news = exhausted.iter().any(|e| e == "google_news");
        if has_serpapi && !has_news {
            exhausted.push("google_news".to_string());
        }
        if has_news && !has_serpapi {
            exhausted.push("serpapi".to_string());
        }
        let ladder = order_engines(&configured, requested, &exhausted);

        // Drop engines with no credentials configured. A missing key produces
        // `not_configured` on every call — the 2026-09-08 GDELT session put
        // `engine: google` on each search, and with no GOOGLE_API_KEY every one
        // wasted its first rung (and log line) before falling through to
        // SearXNG, helping exhaust the 8-round tool budget twice. Unlike a spent
        // quota this never recovers at runtime, so remove rather than demote.
        let mut keep = Vec::new();
        for engine in &ladder {
            if self.is_engine_configured(engine).await {
                keep.push(engine.clone());
            }
        }
        retain_configured(ladder, &keep)
    }

    /// Whether an engine has the credentials it needs to answer. `searxng` needs
    /// none (self-hosted, default URL); the paid engines each need a key (via
    /// their `ApiContext` or env var), and `google` additionally needs a CX.
    /// `serpapi` and `google_news` share one SerpApi account, so one key covers
    /// both. An unknown engine name is assumed configured — we don't second-guess
    /// something the ladder was explicitly told to try.
    async fn is_engine_configured(&self, engine: &str) -> bool {
        match engine {
            "searxng" => true,
            "google" => {
                self.resolve_key("google_cse", "GOOGLE_API_KEY")
                    .await
                    .is_some()
                    && self
                        .resolve_context_field("google_cse", "google_cx", "GOOGLE_CX")
                        .await
                        .is_some()
            }
            "serpapi" | "google_news" => self.resolve_key("serpapi", "SERPAPI_KEY").await.is_some(),
            "brave" => self.resolve_key("brave", "BRAVE_API_KEY").await.is_some(),
            "tavily" => self.resolve_key("tavily", "TAVILY_API_KEY").await.is_some(),
            "exa" => self.resolve_key("exa", "EXA_API_KEY").await.is_some(),
            "serper" => self.resolve_key("serper", "SERPER_API_KEY").await.is_some(),
            "currents" => self
                .resolve_key("currents", "CURRENTS_API_KEY")
                .await
                .is_some(),
            "webz" => self.resolve_key("webz", "WEBZIO_API_KEY").await.is_some(),
            _ => true,
        }
    }

    /// Write one row to the search usage ledger. Never fails the search.
    fn record_usage(
        &self,
        engine: &str,
        query: &str,
        success: bool,
        result_count: Option<i64>,
        duration_ms: i64,
        error_kind: Option<&str>,
    ) {
        if let Some(ref t) = self.telemetry
            && let Err(e) = t.record_search_usage(
                engine,
                query,
                success,
                result_count,
                Some(duration_ms),
                error_kind,
            )
        {
            warn!(engine = %engine, error = %e, "Failed to record search usage");
        }
    }

    /// Plain text of a `ToolCallResult`'s first content block.
    fn result_text(result: &ToolCallResult) -> String {
        result
            .content
            .first()
            .and_then(|c| {
                if let agent_brain_protocol::Content::Text { text } = c {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }

    /// Remove results that are a search engine's own results page
    /// (`ecosia.org/search?q=…`). Returns the result unchanged when nothing is
    /// dropped or the text is not a result array.
    fn drop_search_pages(result: ToolCallResult) -> (ToolCallResult, usize) {
        if result.is_error == Some(true) {
            return (result, 0);
        }
        let Ok(items) = serde_json::from_str::<Vec<Value>>(&Self::result_text(&result)) else {
            return (result, 0);
        };
        let before = items.len();
        let kept: Vec<Value> = items
            .into_iter()
            .filter(|item| {
                let link = item
                    .get("link")
                    .or_else(|| item.get("url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                !is_search_page(link)
            })
            .collect();
        let dropped = before - kept.len();
        if dropped == 0 {
            return (result, 0);
        }
        (ToolCallResult::success_json(kept), dropped)
    }

    /// Number of results in a successful engine response.
    fn result_count(result: &ToolCallResult) -> usize {
        serde_json::from_str::<Vec<Value>>(&Self::result_text(result))
            .map(|v| v.len())
            .unwrap_or(0)
    }

    // ========================================================================
    // Tool Handlers
    // ========================================================================

    /// What the graph ALREADY holds on this query, rendered as a block that
    /// leads every `search_web` result.
    ///
    /// The brain's default reflex was to reach outward. Asked to produce an
    /// OpenAPI doc for Home Assistant, a worker went web-searching for a spec —
    /// while `(:ApiContext {name:'home_assistant'})` sat in the graph with 20
    /// endpoints already ingested, listed in that same conversation two turns
    /// earlier. The same shape costs far more on the research and media paths,
    /// where re-searching a topic the brain studied last week is invisible
    /// waste: no error, no duplicate-detection, just a second set of notes
    /// saying what the first set said.
    ///
    /// Consolidation cleans duplicates up AFTERWARDS. This is the half that
    /// stops them being created — the "wait, do I already know this?" question,
    /// asked automatically so no model has to remember to ask it.
    ///
    /// Deliberately UNCONDITIONAL and in-band. A prompt rule telling an agent to
    /// check the graph first is the weak half: it was ignored in three separate
    /// incidents this month. Putting the answer at position zero of the result
    /// the agent is already reading requires nobody to remember anything.
    ///
    /// Fails open in every direction — a graph error, a missing index, or no
    /// Neo4j at all yields `None` and the search proceeds untouched. This must
    /// never be able to break a search.
    async fn prior_knowledge(&self, query: &str) -> Option<String> {
        let neo4j = self.neo4j.as_ref()?;
        let mut lines: Vec<String> = Vec::new();

        // ── Notes: hybrid (vector + BM25) when a knowledge store is wired,
        // BM25 alone otherwise.
        //
        // Hybrid matters here more than almost anywhere: the stored note
        // rarely repeats the searcher's wording, and a keyword-only probe
        // reports "we know nothing about this" for a paraphrase. It is safe
        // because `search_notes_readonly` degrades to BM25 internally on an
        // embed error, and READONLY because a housekeeping probe must not bump
        // access counts and quietly reshape the spaced-repetition schedule.
        //
        // Activity logs are excluded either way. `chat_session` and
        // `scheduler_dispatch` record that something HAPPENED, not what is
        // KNOWN — and because they quote user questions verbatim they score
        // extremely well on the very query being asked, crowding out real
        // knowledge with transcripts of someone asking about it.
        const LOG_CONTEXTS: [&str; 2] = ["chat_session", "scheduler_dispatch"];
        let mut note_hits: Vec<(String, String, String)> = Vec::new();
        let mut note_total: i64 = 0;

        if let Some(k) = &self.knowledge
            && let Ok(rows) = k.search_notes_readonly(query, 12, 0, None, None).await
            && !rows.is_empty()
        {
            // The hybrid path returns only {id, content}, so date and
            // source_context come from one follow-up lookup rather than by
            // widening a return shape a dozen other callers depend on.
            let ids: Vec<String> = rows
                .iter()
                .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string))
                .collect();
            if let Ok(meta) = neo4j
                .execute(
                    neo4rs::query(
                        "MATCH (n:Note) WHERE n.id IN $ids \
                           AND NOT coalesce(n.source_context, '') IN $logs \
                         RETURN n.id AS id, \
                                substring(toString(n.created_at), 0, 10) AS d, \
                                coalesce(n.source_context, '') AS sc, \
                                left(n.content, 160) AS t",
                    )
                    .param("ids", ids)
                    .param("logs", LOG_CONTEXTS.to_vec()),
                )
                .await
            {
                for row in &meta {
                    note_hits.push((
                        row.get::<String>("d").unwrap_or_default(),
                        row.get::<String>("sc").unwrap_or_default(),
                        row.get::<String>("t").unwrap_or_default(),
                    ));
                }
            }
            note_total = note_hits.len() as i64;
        }

        if note_hits.is_empty() {
            // No store wired, or hybrid found nothing — fall back to the
            // fulltext index directly so the probe still works standalone.
            let lucene = crate::services::knowledge::sanitize_lucene_query(query);
            if let Ok(rows) = neo4j
                .execute(
                    neo4rs::query(
                        "CALL db.index.fulltext.queryNodes('note_content_fulltext', $q, \
                         {limit: 60}) \
                         YIELD node AS n, score \
                         WHERE NOT (n)-[:PART_OF]->() \
                           AND NOT coalesce(n.source_context, '') IN $logs \
                         RETURN count(n) AS total, \
                                collect({ \
                                  t: left(n.content, 100), \
                                  d: substring(toString(n.created_at), 0, 10), \
                                  s: coalesce(n.source_context, '') \
                                })[0..3] AS top",
                    )
                    .param("q", lucene.as_str())
                    .param("logs", LOG_CONTEXTS.to_vec()),
                )
                .await
                && let Some(row) = rows.first()
            {
                note_total = row.get::<i64>("total").unwrap_or(0);
                if let Ok(top) = row.get::<Vec<Value>>("top") {
                    for item in top.iter() {
                        note_hits.push((
                            item.get("d")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            item.get("s")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            item.get("t")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        ));
                    }
                }
            }
        }

        if note_total > 0 {
            lines.push(format!(
                "· {note_total} stored note(s) already match this query. Most relevant:"
            ));
            for (d, sc, t) in note_hits.iter().take(3) {
                let sc = if sc.is_empty() {
                    String::new()
                } else {
                    format!(" [{sc}]")
                };
                // Retrieval labels ("[CLAIM · unverified …]", "[SOURCE RECORD …]")
                // are baked into stored content and would eat the whole preview
                // window, showing the label instead of the note.
                let body = t.replace('\n', " ");
                let body = match (body.find('['), body.find(']')) {
                    (Some(0), Some(end)) => body[end + 1..].trim().to_string(),
                    _ => body.trim().to_string(),
                };
                lines.push(format!(
                    "    ({d}){sc} {}",
                    body.chars().take(100).collect::<String>()
                ));
            }
        }

        // ── APIs: an ingested endpoint graph beats any spec found on the web.
        if let Ok(rows) = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (c:ApiContext) \
                     WHERE any(w IN $words WHERE toLower(c.name) CONTAINS w) \
                     OPTIONAL MATCH (c)-[:EXPOSES]->(e:ApiEndpoint) \
                     WITH c, count(e) AS n WHERE n > 0 \
                     RETURN c.name AS name, n ORDER BY n DESC LIMIT 3",
                )
                .param("words", Self::query_words(query)),
            )
            .await
        {
            for row in &rows {
                lines.push(format!(
                    "· API ALREADY INGESTED: `{}` with {} endpoints — use list_endpoints(\"{}\"), \
                     do NOT search the web for its spec.",
                    row.get::<String>("name").unwrap_or_default(),
                    row.get::<i64>("n").unwrap_or(0),
                    row.get::<String>("name").unwrap_or_default(),
                ));
            }
        }

        // ── Media: "have we watched this already?"
        //
        // TWO matching words minimum. A single word is not evidence: the first
        // build matched "Home Assistant REST API" against three videos on the
        // strength of "home" alone — a SmartThings pricing rant, an Arizona
        // sober-living exposé, and a 9/11 archive piece. A block full of
        // confident false positives is worse than no block, because it trains
        // the reader to skip it.
        if let Ok(rows) = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (m:Media) \
                     WITH m, [w IN $words WHERE toLower(coalesce(m.title, '')) CONTAINS w] AS hits \
                     WHERE size(hits) >= 2 \
                     RETURN m.title AS title, m.url AS url, \
                            substring(toString(m.ingested_at), 0, 10) AS d \
                     ORDER BY m.ingested_at DESC LIMIT 3",
                )
                .param("words", Self::query_words(query)),
            )
            .await
        {
            for row in &rows {
                lines.push(format!(
                    "· ALREADY WATCHED ({}): {} — {}",
                    row.get::<String>("d").unwrap_or_default(),
                    row.get::<String>("title").unwrap_or_default(),
                    row.get::<String>("url").unwrap_or_default(),
                ));
            }
        }

        if lines.is_empty() {
            return None;
        }
        Some(format!(
            "═════ WHAT WE ALREADY KNOW (checked before searching) ═════\n{}\n\
             Read this FIRST. If it answers the question, say so and cite the stored \
             note — do not re-derive it from the web results below, and do not store a \
             second copy of something we already have. Use the web results for what is \
             genuinely NEW or needs updating.\n\
             ═══════════════════════════════════════════════════════════\n",
            lines.join("\n")
        ))
    }

    /// Lowercased, de-noised words from a query, for CONTAINS matching against
    /// names and titles. Short words are dropped — matching on "the" or "a"
    /// would make every API and every video look relevant.
    fn query_words(query: &str) -> Vec<String> {
        // Generic protocol/doc vocabulary is filtered because it appears in
        // almost every technical query — an unfiltered "rest" or "endpoint"
        // makes every ingested API look like a match for every search, which
        // turns the prior-knowledge block into noise nobody reads.
        const STOP: &[&str] = &[
            "the",
            "and",
            "for",
            "with",
            "from",
            "how",
            "what",
            "does",
            "api",
            "apis",
            "spec",
            "specs",
            "docs",
            "documentation",
            "openapi",
            "swagger",
            "about",
            "into",
            "that",
            "this",
            "your",
            "our",
            "rest",
            "http",
            "https",
            "json",
            "yaml",
            "endpoint",
            "endpoints",
            "reference",
            "guide",
            "using",
            "write",
            "search",
            "find",
            "look",
        ];
        let mut out: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|w| w.to_lowercase())
            .filter(|w| w.len() > 3 && !STOP.contains(&w.as_str()))
            .collect();
        out.sort();
        out.dedup();
        out.truncate(6);
        out
    }

    /// Prepend [`prior_knowledge`] to a successful search result.
    async fn with_prior_knowledge(&self, query: &str, result: ToolCallResult) -> ToolCallResult {
        if result.is_error == Some(true) {
            return result;
        }
        let Some(block) = self.prior_knowledge(query).await else {
            return result;
        };
        let body = Self::result_text(&result);
        ToolCallResult {
            content: vec![agent_brain_protocol::Content::text(format!(
                "{block}\n{body}"
            ))],
            is_error: result.is_error,
            bulk: result.bulk,
        }
    }

    async fn handle_search_web(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SearchInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let count = input.count.unwrap_or(5).clamp(1, 20);

        // Parse Google syntax once; each engine renders what it understands.
        let mut spec = QuerySpec::parse(&input.query);
        spec.recency = input.recency.as_deref().and_then(Recency::parse);
        spec.news = input.news;

        // Resolve source list restriction if requested.
        let allowed_domains = if let Some(ref list_name) = input.source_list {
            let domains = self.source_list_domains(list_name).await;
            spec.add_sites(&domains);
            domains
        } else {
            vec![]
        };
        let effective_query = spec.display();

        // Build the failover ladder. A single dead engine must not take the
        // whole chain with it: an exhausted SerpApi free tier previously killed
        // 39 jobs and 38 tasks because `search_web` hard-failed on one engine.
        let ladder = self.engine_ladder(input.engine.as_deref()).await;

        info!(
            query = %effective_query,
            ladder = ?ladder,
            requested_engine = ?input.engine,
            source_list = ?input.source_list,
            "Searching web"
        );

        let mut failures: Vec<String> = Vec::new();
        // "engine (why)" for each rung passed over — the banner names them.
        let mut passed_over: Vec<String> = Vec::new();
        let mut answered_by: Option<String> = None;
        let mut result: Option<ToolCallResult> = None;
        // An engine that answers `200` with zero results is not an answer — it
        // is a silent outage. SearXNG lost DNS on 2026-08-18 and every upstream
        // reported "HTTP connection error", so it returned a well-formed empty
        // result set for three days; because that is not an error the ladder
        // stopped at the first rung and never tried Google/SerpApi/Brave, and
        // every daily news brief since came out empty with nothing logged.
        //
        // Empty now falls through to the next engine. If every engine comes
        // back empty we return that empty result rather than an error: "nobody
        // has anything on this query" is a legitimate outcome, and erroring
        // would burn the job's retries and fail the owning Task through
        // chain-death attribution.
        let mut empty_success: Option<ToolCallResult> = None;

        let today = chrono::Utc::now().date_naive();
        for engine in &ladder {
            if let Some(why) = engine_unsupported(engine, &spec, today) {
                info!(engine = %engine, reason = %why, "Search engine skipped — cannot serve this query");
                passed_over.push(format!("{engine} (skipped: {why})"));
                continue;
            }
            let started = Instant::now();
            let attempt = match engine.as_str() {
                "currents" => self.search_currents(&spec, count).await,
                "webz" => self.search_webz(&spec, count).await,
                "searxng" => self.search_searxng(&spec, count).await,
                "serpapi" => self.search_serpapi(&spec, count).await,
                "google_news" => self.search_serpapi_news(&spec, count).await,
                "brave" => self.search_brave(&spec, count).await,
                "google" => self.search_google(&spec, count).await,
                "tavily" => self.search_tavily(&spec, count).await,
                "exa" => self.search_exa(&spec, count).await,
                "serper" => self.search_serper(&spec, count).await,
                other => ToolCallResult::error(format!("Unsupported search engine: {other}")),
            };
            let duration_ms = started.elapsed().as_millis() as i64;

            if attempt.is_error == Some(true) {
                let error_text = Self::result_text(&attempt);
                let kind = classify_search_error(&error_text);
                warn!(
                    engine = %engine,
                    error_kind = kind,
                    error = %error_text,
                    "Search engine failed — trying next in ladder"
                );
                self.record_usage(
                    engine,
                    &effective_query,
                    false,
                    None,
                    duration_ms,
                    Some(kind),
                );
                failures.push(format!("{engine}: {error_text}"));
                passed_over.push(format!("{engine} ({kind})"));
                continue;
            }

            // Before counting: an engine whose only "hits" are other engines'
            // results pages has not answered, and must fall through like an
            // empty one rather than end the ladder.
            let (attempt, search_pages) = Self::drop_search_pages(attempt);
            if search_pages > 0 {
                info!(
                    engine = %engine,
                    dropped = search_pages,
                    "Dropped search-results pages from engine answer"
                );
            }

            let n = Self::result_count(&attempt);
            self.record_usage(
                engine,
                &effective_query,
                true,
                Some(n as i64),
                duration_ms,
                None,
            );

            if n == 0 {
                warn!(
                    engine = %engine,
                    query = %effective_query,
                    "Search engine returned zero results — trying next in ladder"
                );
                failures.push(format!("{engine}: returned 0 results"));
                passed_over.push(format!("{engine} (0 results)"));
                // Keep the first one so an all-empty ladder still returns `[]`
                // in the engine's own shape rather than an error.
                empty_success.get_or_insert(attempt);
                continue;
            }

            answered_by = Some(engine.clone());
            result = Some(attempt);
            break;
        }

        let result = match result.or(empty_success) {
            Some(r) => r,
            None => {
                // Every engine is down. Surface all of them — knowing that SerpApi
                // is out of quota AND Google CSE is disabled is the difference
                // between a one-line fix and a day of guessing.
                return ToolCallResult::error(format!(
                    "All search engines failed ({} tried).\n{}",
                    ladder.len(),
                    failures.join("\n")
                ));
            }
        };

        if !failures.is_empty() {
            warn!(
                query = %effective_query,
                attempts = ?failures,
                "Search ladder fell through at least one engine"
            );
        }

        let banner = (answered_by.as_deref() == Some("searxng")).then(|| {
            let why =
                if input.engine.as_deref().map(str::to_lowercase).as_deref() == Some("searxng") {
                    "requested explicitly".to_string()
                } else if passed_over.is_empty() {
                    "no keyed search engine is configured".to_string()
                } else {
                    format!("keyed engines unavailable: {}", passed_over.join(", "))
                };
            warn!(query = %effective_query, reason = %why, "Search answered by SearXNG fallback");
            searxng_banner(&why, spec.recency.is_some())
        });

        // Post-filter: drop any result whose URL is not from an approved domain.
        if allowed_domains.is_empty() {
            return self.finish_search(&input, result, banner).await;
        }
        let content_text = result
            .content
            .first()
            .and_then(|c| {
                if let agent_brain_protocol::Content::Text { text } = c {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .unwrap_or("[]");
        let Ok(items) = serde_json::from_str::<Vec<Value>>(content_text) else {
            return self.finish_search(&input, result, banner).await;
        };
        let filtered: Vec<Value> = items
            .into_iter()
            .filter(|item| {
                let url = item
                    .get("link")
                    .or_else(|| item.get("url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Self::url_matches_any(url, &allowed_domains)
            })
            .collect();
        // Source-list path returns here; it must carry the prior-knowledge
        // block too, or a domain-restricted search is the one route that
        // silently skips the "do we already know this?" check.
        self.finish_search(&input, ToolCallResult::success_json(filtered), banner)
            .await
    }

    /// Every return path of `search_web` ends here, so the `prior_knowledge`
    /// opt-out cannot be honoured on one path and forgotten on another.
    ///
    /// The SearXNG banner goes on first so the prior-knowledge block, when
    /// present, still leads: block, banner, results.
    async fn finish_search(
        &self,
        input: &SearchInput,
        result: ToolCallResult,
        banner: Option<String>,
    ) -> ToolCallResult {
        let result = match banner {
            Some(b) if result.is_error != Some(true) => ToolCallResult {
                content: vec![agent_brain_protocol::Content::text(format!(
                    "{b}\n\n{}",
                    Self::result_text(&result)
                ))],
                is_error: result.is_error,
                bulk: result.bulk,
            },
            _ => result,
        };
        if !input.prior_knowledge {
            return result;
        }
        self.with_prior_knowledge(&input.query, result).await
    }

    async fn handle_get_search_usage(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SearchUsageInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let Some(ref t) = self.telemetry else {
            return ToolCallResult::error(
                "get_search_usage is not available: telemetry is not configured \
                 (set TELEMETRY_DB_PATH)",
            );
        };
        // Default to a 30-day window: monthly caps are the ones that bite.
        let window = input.window_hours.or(Some(720));
        match t.get_search_stats(window) {
            Ok(stats) => ToolCallResult::success_json(stats),
            Err(e) => ToolCallResult::error(format!("Failed to read search usage: {e}")),
        }
    }

    /// Self-hosted SearXNG metasearch — the default engine.
    ///
    /// No API key and no quota: it aggregates 70+ upstream engines and runs as a
    /// compose sidecar. Requires `formats: [json]` in the SearXNG `settings.yml`
    /// (the shipped default only enables `html`, which answers 403).
    ///
    /// Results are normalised to the `title`/`link`/`snippet` shape that SerpApi
    /// and Google CSE already emit, so downstream `reason` steps see one schema
    /// regardless of which rung of the ladder answered.
    async fn search_searxng(&self, spec: &QuerySpec, count: u8) -> ToolCallResult {
        // SearXNG fans out to Google, Bing, DuckDuckGo…: they all read `site:`,
        // quotes and `-`, but not `after:`, so dates go in `time_range`.
        let query = spec.google_syntax(false);
        let mut params: Vec<(&str, String)> = vec![
            ("q", query.clone()),
            ("format", "json".into()),
            ("language", "en".into()),
            ("safesearch", "0".into()),
        ];
        // No `time_range`: SearXNG drops every backend that cannot filter by
        // date, which left Naver alone for general queries and 2 results for
        // news (vs 54, 45 dated, without it). The window is enforced below
        // from each result's own `publishedDate` instead.
        if spec.news {
            params.push(("categories", "news".into()));
        }
        let base_url = self
            .resolve_context_field("searxng", "base_url", "SEARXNG_URL")
            .await
            .unwrap_or_else(|| DEFAULT_SEARXNG_URL.to_string());
        let endpoint = format!("{}/search", base_url.trim_end_matches('/'));

        let response = self
            .client
            .get(&endpoint)
            .query(&params)
            .timeout(std::time::Duration::from_secs(SEARXNG_TIMEOUT_SECS))
            .send()
            .await;

        match response {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    return ToolCallResult::error(format!(
                        "SearXNG failed: {} - {}",
                        status,
                        text.chars().take(300).collect::<String>()
                    ));
                }
                match resp.json::<Value>().await {
                    Ok(json) => {
                        let today = chrono::Utc::now().date_naive();
                        let results = searxng_items(&json, count, spec.since(today), spec.before);

                        // SearXNG reports per-upstream failures in
                        // `unresponsive_engines` and still answers 200. Nothing
                        // read that field, which is the entire reason two
                        // multi-day outages were invisible: on 2026-08-18 every
                        // upstream lost DNS, and on 2026-08-31 all four default
                        // engines were bot-blocked (CAPTCHA / too many
                        // requests). Both produced a well-formed empty result
                        // set — indistinguishable, from here, from a genuinely
                        // obscure query. It is the *reasons* that tell them
                        // apart, so they are logged verbatim.
                        let unresponsive = parse_unresponsive_engines(&json);
                        if !unresponsive.is_empty() {
                            let detail = format_unresponsive(&unresponsive);
                            if results.is_empty() {
                                warn!(
                                    query = %query,
                                    failed_engines = %detail,
                                    "SearXNG returned no results AND every upstream engine failed \
                                     — this is an outage, not an empty query. Run \
                                     scripts/searxng_engine_health.py"
                                );
                            } else {
                                warn!(
                                    query = %query,
                                    failed_engines = %detail,
                                    returned = results.len(),
                                    "SearXNG answered with some upstream engines down — results \
                                     are degraded"
                                );
                            }
                        }

                        // An empty result set is a real answer, not an engine
                        // failure — but it is also the shape a misconfigured
                        // SearXNG returns when every upstream engine is blocked,
                        // so it is worth a knowledge-gap marker.
                        if results.is_empty()
                            && let Some(ref t) = self.telemetry
                        {
                            let _ = t.log_knowledge_gap(
                                &query,
                                Some("search_web:searxng"),
                                "missing_info",
                            );
                        }
                        ToolCallResult::success_json(results)
                    }
                    Err(e) => {
                        ToolCallResult::error(format!("Failed to parse SearXNG response: {e}"))
                    }
                }
            }
            Err(e) => ToolCallResult::error(format!("Request failed: {e}")),
        }
    }

    async fn search_serpapi(&self, spec: &QuerySpec, count: u8) -> ToolCallResult {
        let query = spec.google_syntax(true);
        let query = query.as_str();
        let api_key = match self.resolve_key("serpapi", "SERPAPI_KEY").await {
            Some(k) => k,
            None => {
                if let Some(ref t) = self.telemetry {
                    let _ = t.log_knowledge_gap(
                        query,
                        Some("search_web:serpapi"),
                        "missing_tool_config",
                    );
                }
                return ToolCallResult::error(
                    "SerpApi key not configured (set SERPAPI_KEY or define serpapi ApiContext)"
                        .to_string(),
                );
            }
        };

        let response = self
            .client
            .get("https://serpapi.com/search.json")
            .query(&[
                ("api_key", api_key.as_str()),
                ("q", query),
                ("num", &count.to_string()),
                ("engine", "google"),
            ])
            .query(
                &spec
                    .google_tbs()
                    .map(|t| vec![("tbs", t)])
                    .unwrap_or_default(),
            )
            .send()
            .await;

        match response {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if let Some(ref t) = self.telemetry {
                        let _ = t.log_knowledge_gap(query, Some("search_web:serpapi"), "api_error");
                    }
                    return ToolCallResult::error(format!("SerpApi failed: {} - {}", status, text));
                }
                match resp.json::<Value>().await {
                    Ok(json) => {
                        // SerpApi signals quota/rate/service problems as HTTP 200
                        // with an `error` field and NO organic_results. Without
                        // this check the empty result was recorded as a SUCCESSFUL
                        // serpapi call with 0 results, and the ladder fell through
                        // to SearXNG silently — so a real SerpApi outage looked
                        // like "success" in the logs and ledger. Surface the raw
                        // message: the ladder's classify_search_error buckets it
                        // (quota → cooldown), WARNs, and falls through visibly.
                        if let Some(err) = json.get("error").and_then(Value::as_str) {
                            if let Some(ref t) = self.telemetry {
                                let _ = t.log_knowledge_gap(
                                    query,
                                    Some("search_web:serpapi"),
                                    classify_search_error(err),
                                );
                            }
                            return ToolCallResult::error(format!("SerpApi error: {err}"));
                        }
                        let results = json
                            .get("organic_results")
                            .unwrap_or(&json!([]))
                            .as_array()
                            .unwrap_or(&vec![])
                            .iter()
                            .map(|item| {
                                json!({
                                    "title":   item.get("title"),
                                    "link":    item.get("link"),
                                    "snippet": item.get("snippet"),
                                })
                            })
                            .collect::<Vec<_>>();
                        if results.is_empty()
                            && let Some(ref t) = self.telemetry
                        {
                            let _ = t.log_knowledge_gap(
                                query,
                                Some("search_web:serpapi"),
                                "missing_info",
                            );
                        }
                        ToolCallResult::success_json(results)
                    }
                    Err(e) => {
                        ToolCallResult::error(format!("Failed to parse SerpApi response: {}", e))
                    }
                }
            }
            Err(e) => {
                if let Some(ref t) = self.telemetry {
                    let _ = t.log_knowledge_gap(query, Some("search_web:serpapi"), "network_error");
                }
                ToolCallResult::error(format!("Request failed: {}", e))
            }
        }
    }

    /// SerpApi's Google News engine — richer, freshly-dated news stories.
    ///
    /// Shares the SerpApi account (and its monthly quota) with `search_serpapi`,
    /// but `engine=google_news` returns curated news results carrying a source
    /// name and publication date rather than generic organic web results. It is
    /// deliberately NOT in the default ladder: callers opt in with
    /// `engine: "google_news"` (the daily news brief does), which promotes it to
    /// the head and still falls back through SearXNG when the quota is spent.
    ///
    /// Google News ignores `num`; it returns a full page, so the result set is
    /// flattened and capped locally via `flatten_google_news`.
    async fn search_serpapi_news(&self, spec: &QuerySpec, count: u8) -> ToolCallResult {
        // Google News ranks by relevance; without `when:` a "today" query
        // returned stories from March (2026-10-02, 0 of 8 current).
        let mut query = spec.google_syntax(false);
        if let Some(when) = spec.google_news_when() {
            query.push(' ');
            query.push_str(&when);
        }
        let query = query.as_str();
        let api_key = match self.resolve_key("serpapi", "SERPAPI_KEY").await {
            Some(k) => k,
            None => {
                if let Some(ref t) = self.telemetry {
                    let _ = t.log_knowledge_gap(
                        query,
                        Some("search_web:google_news"),
                        "missing_tool_config",
                    );
                }
                return ToolCallResult::error(
                    "SerpApi key not configured (set SERPAPI_KEY or define serpapi ApiContext)"
                        .to_string(),
                );
            }
        };

        let response = self
            .client
            .get("https://serpapi.com/search.json")
            .query(&[
                ("api_key", api_key.as_str()),
                ("q", query),
                ("engine", "google_news"),
                ("gl", "us"),
                ("hl", "en"),
            ])
            .send()
            .await;

        match response {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if let Some(ref t) = self.telemetry {
                        let _ =
                            t.log_knowledge_gap(query, Some("search_web:google_news"), "api_error");
                    }
                    return ToolCallResult::error(format!(
                        "SerpApi Google News failed: {} - {}",
                        status, text
                    ));
                }
                match resp.json::<Value>().await {
                    Ok(json) => {
                        // See search_serpapi: a 200 + `error` body is a real
                        // failure (quota/rate/service), not an empty result.
                        // Surface it so the ladder classifies and falls through
                        // visibly instead of recording a phantom success.
                        if let Some(err) = json.get("error").and_then(Value::as_str) {
                            if let Some(ref t) = self.telemetry {
                                let _ = t.log_knowledge_gap(
                                    query,
                                    Some("search_web:google_news"),
                                    classify_search_error(err),
                                );
                            }
                            return ToolCallResult::error(format!(
                                "SerpApi Google News error: {err}"
                            ));
                        }
                        let empty = vec![];
                        let news = json
                            .get("news_results")
                            .and_then(Value::as_array)
                            .unwrap_or(&empty);
                        let results = flatten_google_news(news, count as usize);
                        if results.is_empty()
                            && let Some(ref t) = self.telemetry
                        {
                            let _ = t.log_knowledge_gap(
                                query,
                                Some("search_web:google_news"),
                                "missing_info",
                            );
                        }
                        ToolCallResult::success_json(results)
                    }
                    Err(e) => ToolCallResult::error(format!(
                        "Failed to parse SerpApi Google News response: {}",
                        e
                    )),
                }
            }
            Err(e) => {
                if let Some(ref t) = self.telemetry {
                    let _ =
                        t.log_knowledge_gap(query, Some("search_web:google_news"), "network_error");
                }
                ToolCallResult::error(format!("Request failed: {}", e))
            }
        }
    }

    async fn search_brave(&self, spec: &QuerySpec, count: u8) -> ToolCallResult {
        let query = spec.google_syntax(false);
        let query = query.as_str();
        let api_key =
            match self.resolve_key("brave", "BRAVE_API_KEY").await {
                Some(k) => k,
                None => return ToolCallResult::error(
                    "Brave API key not configured (set BRAVE_API_KEY or define brave ApiContext)"
                        .to_string(),
                ),
            };

        let response = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("X-Subscription-Token", &api_key)
            .query(&[("q", query), ("count", &count.to_string())])
            .query(
                &spec
                    .brave_freshness()
                    .map(|f| vec![("freshness", f)])
                    .unwrap_or_default(),
            )
            .send()
            .await;

        match response {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    return ToolCallResult::error(format!(
                        "Brave Search failed: {} - {}",
                        status, text
                    ));
                }
                match resp.json::<Value>().await {
                    Ok(json) => {
                        let empty = json!([]);
                        let results = json
                            .get("web")
                            .and_then(|w| w.get("results"))
                            .unwrap_or(&empty);
                        let simplified: Vec<Value> = results
                            .as_array()
                            .unwrap_or(&vec![])
                            .iter()
                            .take(count as usize)
                            .map(|r| {
                                json!({
                                    "title":       r.get("title"),
                                    "url":         r.get("url"),
                                    "description": r.get("description"),
                                    "age":         r.get("age"),
                                })
                            })
                            .collect();
                        ToolCallResult::success_json(simplified)
                    }
                    Err(e) => {
                        ToolCallResult::error(format!("Failed to parse Brave response: {}", e))
                    }
                }
            }
            Err(e) => ToolCallResult::error(format!("Request failed: {}", e)),
        }
    }

    /// POST a JSON search request and return the parsed body, or a tool error
    /// whose text the ladder classifies. A spent allowance is reported with the
    /// word "quota" so `classify_search_error` demotes the engine: the status
    /// codes differ per vendor (Exa 402, Tavily 432/433, Serper 400/403 with
    /// "Not enough credits") and none of them is a 429.
    async fn post_search(
        &self,
        engine: &str,
        url: &str,
        auth: (&str, String),
        body: Value,
    ) -> Result<Value, ToolCallResult> {
        let resp = self
            .client
            .post(url)
            .header(auth.0, auth.1)
            .json(&body)
            .send()
            .await
            .map_err(|e| ToolCallResult::error(format!("Request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let spent = matches!(status.as_u16(), 402 | 432 | 433)
                || text.to_lowercase().contains("credits");
            let prefix = if spent { "quota exhausted — " } else { "" };
            return Err(ToolCallResult::error(format!(
                "{engine} search failed: {prefix}HTTP {status} - {text}"
            )));
        }
        resp.json::<Value>()
            .await
            .map_err(|e| ToolCallResult::error(format!("Failed to parse {engine} response: {e}")))
    }

    /// Tavily: 1,000 searches/month free, refills, no card. `Authorization: Bearer`.
    async fn search_tavily(&self, spec: &QuerySpec, count: u8) -> ToolCallResult {
        let Some(key) = self.resolve_key("tavily", "TAVILY_API_KEY").await else {
            return ToolCallResult::error(
                "Tavily API key not configured (set TAVILY_API_KEY)".to_string(),
            );
        };
        // Tavily reads no operators: domains, dates and news go in parameters.
        let mut body = json!({
            "query": spec.plain_text(true),
            "max_results": count,
            "search_depth": "basic",
            "include_published_date": true,
        });
        if spec.news {
            body["topic"] = json!("news");
        }
        if spec.has_phrases() {
            body["exact_match"] = json!(true);
        }
        if !spec.sites.is_empty() {
            body["include_domains"] = json!(spec.sites);
        }
        if !spec.exclude_sites.is_empty() {
            body["exclude_domains"] = json!(spec.exclude_sites);
        }
        match (spec.recency, spec.tavily_dates()) {
            (Some(r), (_, None)) => body["time_range"] = json!(r.word()),
            (_, (start, end)) => {
                if let Some(s) = start {
                    body["start_date"] = json!(s);
                }
                if let Some(e) = end {
                    body["end_date"] = json!(e);
                }
            }
        }
        match self
            .post_search(
                "Tavily",
                "https://api.tavily.com/search",
                ("Authorization", format!("Bearer {key}")),
                body,
            )
            .await
        {
            Ok(json) => ToolCallResult::success_json(normalize_tavily(&json, count)),
            Err(e) => e,
        }
    }

    /// Exa: $10 of credit/month free, refills, no card. Asks for a short
    /// highlight per result — without `contents` Exa returns no snippet at all.
    async fn search_exa(&self, spec: &QuerySpec, count: u8) -> ToolCallResult {
        let Some(key) = self.resolve_key("exa", "EXA_API_KEY").await else {
            return ToolCallResult::error(
                "Exa API key not configured (set EXA_API_KEY)".to_string(),
            );
        };
        // Exa is a neural ranker and reads no operators — `site:` there was the
        // word "site". Domains, dates, news and a short exact phrase are API
        // parameters. News + a one-day window returned CNBC, Al Jazeera,
        // France24, Politico dated that day (2026-10-02).
        let mut body = json!({
            "query": spec.plain_text(false),
            "numResults": count,
            "type": "auto",
            "contents": { "highlights": { "maxCharacters": EXA_HIGHLIGHT_CHARS } },
        });
        if spec.news {
            body["category"] = json!("news");
        }
        if !spec.sites.is_empty() {
            body["includeDomains"] = json!(spec.sites);
        }
        if !spec.exclude_sites.is_empty() {
            body["excludeDomains"] = json!(spec.exclude_sites);
        }
        let (start, end) = spec.exa_dates();
        if let Some(s) = start {
            body["startPublishedDate"] = json!(s);
        }
        if let Some(e) = end {
            body["endPublishedDate"] = json!(e);
        }
        if let Some(t) = spec.exa_include_text() {
            body["includeText"] = json!([t]);
        }
        if let Some(t) = spec.exa_exclude_text() {
            body["excludeText"] = json!([t]);
        }
        match self
            .post_search("Exa", "https://api.exa.ai/search", ("x-api-key", key), body)
            .await
        {
            Ok(json) => ToolCallResult::success_json(normalize_exa(&json, count)),
            Err(e) => e,
        }
    }

    /// Serper: Google results, 2,500 free queries ONCE (no refill), no card.
    async fn search_serper(&self, spec: &QuerySpec, count: u8) -> ToolCallResult {
        let Some(key) = self.resolve_key("serper", "SERPER_API_KEY").await else {
            return ToolCallResult::error(
                "Serper API key not configured (set SERPER_API_KEY)".to_string(),
            );
        };
        let mut body = json!({ "q": spec.google_syntax(true), "num": count });
        if let Some(tbs) = spec.google_tbs() {
            body["tbs"] = json!(tbs);
        }
        // Serper's /news endpoint returns `news` instead of `organic`.
        let endpoint = if spec.news {
            "https://google.serper.dev/news"
        } else {
            "https://google.serper.dev/search"
        };
        match self
            .post_search("Serper", endpoint, ("X-API-KEY", key), body)
            .await
        {
            Ok(json) => ToolCallResult::success_json(normalize_serper(&json, count)),
            Err(e) => e,
        }
    }

    /// Currents (news). GET with a Bearer key; keywords take quoted phrases,
    /// OR and -exclusion; one include and one exclude domain; a date range
    /// needs both ends and at most 7 days on the free plan.
    async fn search_currents(&self, spec: &QuerySpec, count: u8) -> ToolCallResult {
        let Some(key) = self.resolve_key("currents", "CURRENTS_API_KEY").await else {
            return ToolCallResult::error("Currents API key not configured (set CURRENTS_API_KEY)");
        };
        let now = chrono::Utc::now();
        let params = currents_params(spec, count, now);
        let resp = match self
            .client
            .get("https://api.currentsapi.services/v1/search")
            .bearer_auth(&key)
            .query(&params)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("Request failed: {e}")),
        };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let spent = status.as_u16() == 429 || text.to_lowercase().contains("limit");
            let prefix = if spent { "quota exhausted — " } else { "" };
            return ToolCallResult::error(format!(
                "Currents search failed: {prefix}HTTP {status} - {}",
                redact(&text, &key).chars().take(400).collect::<String>()
            ));
        }
        match serde_json::from_str::<Value>(&text) {
            Ok(json) => ToolCallResult::success_json(normalize_currents(&json, count)),
            Err(e) => ToolCallResult::error(format!("Failed to parse Currents response: {e}")),
        }
    }

    /// Webz news search (full text). The key is a self-serve News API key, so
    /// the endpoint is `/api/news` — `newsApiLite` answers it "Unknown API
    /// token", and `/api/nbd_search` also covers blogs and forums (2026-10-06).
    /// `includeSyndicated=false` asks Webz to drop syndicated copies. GET
    /// with `token`; Lucene-style query with `site:`/`-site:`,
    /// `language:english`, `published:<ms`; the window start goes in `ts`
    /// (epoch ms). Each answer reports the prepaid `balance`, which is logged;
    /// an empty balance reads as quota exhausted. Error bodies echo the token,
    /// so every error text is redacted.
    async fn search_webz(&self, spec: &QuerySpec, count: u8) -> ToolCallResult {
        let Some(key) = self.resolve_key("webz", "WEBZIO_API_KEY").await else {
            return ToolCallResult::error("Webz API key not configured (set WEBZIO_API_KEY)");
        };
        let mut params = webz_params(spec, chrono::Utc::now().date_naive());
        params.push(("token".into(), key.clone()));
        params.push(("size".into(), count.min(WEBZ_MAX_RESULTS).to_string()));
        params.push(("includeSyndicated".into(), "false".into()));
        let resp = match self
            .client
            .get("https://api.webz.io/api/news")
            .query(&params)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return ToolCallResult::error(format!(
                    "Request failed: {}",
                    redact(&e.to_string(), &key)
                ));
            }
        };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let lower = text.to_lowercase();
            let spent = matches!(status.as_u16(), 402 | 429)
                || ["limit", "quota", "balance", "credit"]
                    .iter()
                    .any(|w| lower.contains(w));
            let prefix = if spent { "quota exhausted — " } else { "" };
            return ToolCallResult::error(format!(
                "Webz search failed: {prefix}HTTP {status} - {}",
                redact(&text, &key).chars().take(400).collect::<String>()
            ));
        }
        match serde_json::from_str::<Value>(&text) {
            Ok(json) => {
                let balance = json["balance"].as_f64();
                info!(balance = ?balance, cost = ?json["cost"].as_f64(), "Webz search");
                if balance.is_some_and(|b| b < 0.5) {
                    warn!(balance = ?balance, "Webz prepaid balance is nearly spent");
                }
                ToolCallResult::success_json(normalize_webz(&json, count))
            }
            Err(e) => ToolCallResult::error(format!("Failed to parse Webz response: {e}")),
        }
    }

    async fn search_google(&self, spec: &QuerySpec, count: u8) -> ToolCallResult {
        let query = spec.google_syntax(true);
        let query = query.as_str();
        let api_key = match self.resolve_key("google_cse", "GOOGLE_API_KEY").await {
            Some(k) => k,
            None => return ToolCallResult::error(
                "Google API key not configured (set GOOGLE_API_KEY or define google_cse ApiContext)".to_string()
            ),
        };
        // Google CX is stored as a custom field on the context, not auth
        let cx = match self.resolve_context_field("google_cse", "google_cx", "GOOGLE_CX").await {
            Some(c) => c,
            None => return ToolCallResult::error(
                "Google CX not configured (set GOOGLE_CX or add google_cx field to google_cse ApiContext)".to_string()
            ),
        };

        let response = self
            .client
            .get("https://www.googleapis.com/customsearch/v1")
            .query(&[
                ("key", api_key.as_str()),
                ("cx", cx.as_str()),
                ("q", query),
                ("num", &count.to_string()),
            ])
            .query(
                &spec
                    .cse_date_restrict()
                    .map(|d| vec![("dateRestrict", d)])
                    .unwrap_or_default(),
            )
            .send()
            .await;

        match response {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    return ToolCallResult::error(format!(
                        "Google Search failed: {} - {}",
                        status, text
                    ));
                }
                match resp.json::<Value>().await {
                    Ok(json) => {
                        let items = json
                            .get("items")
                            .unwrap_or(&json!([]))
                            .as_array()
                            .unwrap_or(&vec![])
                            .iter()
                            .map(|item| {
                                json!({
                                    "title":   item.get("title"),
                                    "link":    item.get("link"),
                                    "snippet": item.get("snippet"),
                                })
                            })
                            .collect::<Vec<_>>();
                        ToolCallResult::success_json(items)
                    }
                    Err(e) => {
                        ToolCallResult::error(format!("Failed to parse Google response: {}", e))
                    }
                }
            }
            Err(e) => ToolCallResult::error(format!("Request failed: {}", e)),
        }
    }
}

#[async_trait]
impl Skill for SearchSkill {
    fn name(&self) -> &str {
        "Web Search"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::search_web_def(), Self::get_search_usage_def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "search_web" => Some(self.handle_search_web(arguments).await),
            "get_search_usage" => Some(self.handle_get_search_usage(arguments).await),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SearchUsageInput {
    #[serde(default)]
    window_hours: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SearchInput {
    query: String,
    // Optional filters: `""` means "not specified", not "the engine named ''"
    // or "the source list named ''". Normalized at the boundary so an empty
    // value degrades to the default ladder / unrestricted search rather than a
    // filter that matches nothing. See serde_flex::empty_string_as_none.
    #[serde(
        default,
        deserialize_with = "crate::skills::serde_flex::empty_string_as_none"
    )]
    engine: Option<String>,
    /// Accepts both integer and string values (e.g. `10` or `"10"`) so that
    /// Workflow step definitions stored with quoted counts still work.
    #[serde(default, deserialize_with = "deserialize_optional_count")]
    count: Option<u8>,
    /// `day` / `week` / `month` / `year`: only results from that window, mapped
    /// to each engine's own date filter.
    #[serde(
        default,
        deserialize_with = "crate::skills::serde_flex::empty_string_as_none"
    )]
    recency: Option<String>,
    /// News results only (Exa `category`, Tavily `topic`, SearXNG and Serper
    /// news verticals).
    #[serde(default)]
    news: bool,
    /// Name of a SourceList node in Neo4j. When set, restricts results to approved domains.
    #[serde(
        default,
        deserialize_with = "crate::skills::serde_flex::empty_string_as_none"
    )]
    source_list: Option<String>,
    /// Prefix the result with what the graph already holds (default on).
    /// A claim verifier MUST turn this off: the block quotes our own notes —
    /// often the very claim being verified — and a judge reading it answers
    /// "supports" with the claim as its own source. Observed 2026-09-12..24:
    /// the corroboration rate went 41% → 84%, 169 edges citing a graph note.
    #[serde(default = "default_true")]
    prior_knowledge: bool,
}

fn default_true() -> bool {
    true
}

fn deserialize_optional_count<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None => Ok(None),
        Some(serde_json::Value::Number(n)) => Ok(Some(
            n.as_u64()
                .and_then(|v| u8::try_from(v).ok())
                .ok_or_else(|| D::Error::custom("count must be in range 0-255"))?,
        )),
        Some(serde_json::Value::String(s)) => s.parse::<u8>().map(Some).map_err(D::Error::custom),
        Some(other) => Err(D::Error::custom(format!(
            "invalid type for count: expected integer, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- prior knowledge ------------------------------------------------------

    /// The word filter decides what counts as "do we already know this".
    /// Too loose and every API matches every query; too tight and nothing does.
    #[test]
    fn query_words_drop_noise_and_keep_subjects() {
        let w = SearchSkill::query_words("Home Assistant REST API OpenAPI spec");
        assert!(w.contains(&"home".to_string()) || w.contains(&"assistant".to_string()));
        // Generic doc words would match every ingested API, not this one.
        for noise in ["api", "spec", "openapi", "the", "rest"] {
            assert!(
                !w.contains(&noise.to_string()),
                "{noise} should be filtered"
            );
        }
    }

    #[test]
    fn query_words_are_deduped_and_bounded() {
        let w = SearchSkill::query_words("alpha alpha beta gamma delta epsilon zeta eta theta");
        assert!(w.len() <= 6, "unbounded word list: {w:?}");
        let mut sorted = w.clone();
        sorted.dedup();
        assert_eq!(w.len(), sorted.len(), "duplicates survived");
    }

    /// Short tokens carry no signal and would match almost any title.
    #[test]
    fn query_words_ignore_short_tokens() {
        let w = SearchSkill::query_words("who is he in AI ml");
        assert!(w.is_empty(), "expected nothing usable, got {w:?}");
    }

    /// The graph-first block is on unless a caller opts out — and a verifier's
    /// opt-out must actually parse, or it silently gets the block anyway.
    #[test]
    fn prior_knowledge_defaults_on_and_can_be_turned_off() {
        let on: SearchInput = serde_json::from_value(json!({"query": "q"})).unwrap();
        assert!(on.prior_knowledge);
        let off: SearchInput =
            serde_json::from_value(json!({"query": "q", "prior_knowledge": false})).unwrap();
        assert!(!off.prior_knowledge);
    }

    /// The check must never be able to break a search: no graph, no block, and
    /// the original result passes through untouched.
    #[tokio::test]
    async fn prior_knowledge_fails_open_without_neo4j() {
        let skill = SearchSkill::new(None, None);
        let original = ToolCallResult::success_json(serde_json::json!([{"link": "x"}]));
        let out = skill
            .with_prior_knowledge("anything", original.clone())
            .await;
        assert_eq!(
            SearchSkill::result_text(&out),
            SearchSkill::result_text(&original)
        );
    }

    /// An errored search must not get a "what we already know" preamble — the
    /// caller needs the failure, undecorated.
    #[tokio::test]
    async fn prior_knowledge_is_not_attached_to_an_error() {
        let skill = SearchSkill::new(None, None);
        let err = ToolCallResult::error("all engines failed");
        let out = skill.with_prior_knowledge("anything", err).await;
        assert_eq!(out.is_error, Some(true));
        assert!(SearchSkill::result_text(&out).contains("all engines failed"));
    }

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_ladder_puts_searxng_last_before_the_uncapped_engine() {
        assert_eq!(
            order_engines("", None, &[]),
            v(&[
                "exa", "tavily", "serper", "serpapi", "google", "searxng", "brave"
            ])
        );
    }

    #[test]
    fn engines_that_cannot_serve_a_query_say_why() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 6).unwrap();
        let mut spec = QuerySpec::parse("Sudan El Fasher");
        assert!(engine_unsupported("currents", &spec, today).is_none());
        spec.recency = Some(Recency::Week);
        assert!(
            engine_unsupported("currents", &spec, today).is_none(),
            "a week fits"
        );
        spec.recency = Some(Recency::Month);
        assert!(
            engine_unsupported("currents", &spec, today)
                .unwrap()
                .contains("days")
        );
        assert!(engine_unsupported("webz", &spec, today).is_none());
        spec.recency = Some(Recency::Year);
        assert!(engine_unsupported("webz", &spec, today).is_some());
        let multi = QuerySpec::parse("x site:bbc.co.uk site:reuters.com");
        assert!(
            engine_unsupported("currents", &multi, today)
                .unwrap()
                .contains("one domain")
        );
        assert!(engine_unsupported("webz", &multi, today).is_none());
        assert!(engine_unsupported("exa", &multi, today).is_none());
    }

    #[test]
    fn currents_dates_are_clamped_to_seven_days_and_domains_mapped() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-06T18:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let mut spec =
            QuerySpec::parse(r#""El Fasher" -sports site:bbc.co.uk -site:dailymail.com"#);
        spec.recency = Some(Recency::Week);
        let p = currents_params(&spec, 50, now);
        let get = |k: &str| p.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("keywords"), Some(r#""El Fasher" -sports"#));
        assert_eq!(get("domain"), Some("bbc.co.uk"));
        assert_eq!(get("domain_not"), Some("dailymail.com"));
        assert_eq!(get("page_size"), Some("20"), "free-plan cap");
        assert_eq!(get("end_date"), Some("2026-10-06T18:30:00Z"));
        assert_eq!(
            get("start_date"),
            Some("2026-09-29T18:31:00Z"),
            "7 days minus a minute"
        );
        let plain = currents_params(&QuerySpec::parse("Sudan"), 5, now);
        assert!(
            plain.iter().all(|(k, _)| k != "start_date"),
            "no window, no dates"
        );
    }

    #[test]
    fn webz_queries_carry_sites_language_and_the_window() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 6).unwrap();
        let mut spec =
            QuerySpec::parse(r#""El Fasher" site:bbc.co.uk site:reuters.com -site:dailymail.com"#);
        spec.recency = Some(Recency::Week);
        let p = webz_params(&spec, today);
        let q = &p.iter().find(|(k, _)| k == "q").unwrap().1;
        assert!(q.starts_with(r#""El Fasher""#));
        assert!(q.contains("(site:bbc.co.uk OR site:reuters.com)"));
        assert!(q.contains("-site:dailymail.com") && q.contains("language:english"));
        let ts = &p.iter().find(|(k, _)| k == "ts").unwrap().1;
        assert_eq!(ts, "1790640000000", "2026-09-29T00:00Z in ms");
        spec.recency = Some(Recency::Month);
        let p = webz_params(&spec, today);
        let ts = &p.iter().find(|(k, _)| k == "ts").unwrap().1;
        assert_eq!(
            ts, "1788652800000",
            "a month is clamped to Webz's 30 days: 2026-09-06T00:00Z"
        );
        assert!(
            p.iter().all(|(k, _)| k != "token"),
            "the token is added at call time only"
        );
    }

    #[test]
    fn news_engine_results_normalise() {
        let c = json!({"status": "ok", "news": [
            {"title": "T", "url": "https://a.com/x", "description": "D", "published": "2026-10-06 11:02:31 +0000"},
            {"title": "no url"}]});
        let out = normalize_currents(&c, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["snippet"], "D");
        assert_eq!(out[0]["date"], "2026-10-06 11:02:31 +0000");
        let w = json!({"posts": [{"title": "W", "url": "https://b.com/y", "published": "2026-10-05T10:00:00.000+03:00",
                                  "text": "x".repeat(2000), "thread": {"site": "b.com"}}], "moreResultsAvailable": 0});
        let out = normalize_webz(&w, 10);
        assert_eq!(out[0]["link"], "https://b.com/y");
        assert!(out[0]["snippet"].as_str().unwrap().chars().count() <= WEBZ_SNIPPET_CHARS + 1);
    }

    #[test]
    fn credentials_are_redacted_from_error_text() {
        assert_eq!(
            redact("Unknown API token abcdef123456", "abcdef123456"),
            "Unknown API token <redacted>"
        );
        assert_eq!(
            redact("short", "abc"),
            "short",
            "too short to be a key; left alone"
        );
    }

    #[test]
    fn spent_keyed_engines_fall_behind_searxng_but_brave_stays_last() {
        let ladder = order_engines("", None, &v(&["exa", "tavily"]));
        let pos = |e: &str| ladder.iter().position(|x| x == e).unwrap();
        assert!(pos("serper") < pos("searxng"));
        assert!(pos("searxng") < pos("brave"));
    }

    #[test]
    fn searxng_dates_enforce_the_window_and_keep_undated_results() {
        let json = json!({"results": [
            {"url": "https://a.com/new", "title": "new", "content": "", "publishedDate": "2026-10-05T18:05:55"},
            {"url": "https://b.com/old", "title": "old", "content": "", "publishedDate": "2026-03-01T00:00:00"},
            {"url": "https://c.com/undated", "title": "undated", "content": ""},
        ]});
        let since = NaiveDate::from_ymd_opt(2026, 9, 29);
        let got = searxng_items(&json, 10, since, None);
        let links: Vec<&str> = got.iter().map(|r| r["link"].as_str().unwrap()).collect();
        assert_eq!(links, vec!["https://a.com/new", "https://c.com/undated"]);
        assert_eq!(got[0]["date"], "2026-10-05T18:05:55");
        assert_eq!(searxng_items(&json, 10, None, None).len(), 3);
    }

    #[test]
    fn the_searxng_banner_names_why_and_the_date_caveat() {
        let b = searxng_banner("keyed engines unavailable: exa (quota_exhausted)", true);
        assert!(b.starts_with(SEARXNG_FALLBACK_BANNER));
        assert!(b.contains("exa (quota_exhausted)"));
        assert!(b.contains("undated ones may be old"));
        assert!(!searxng_banner("requested explicitly", false).contains("date window"));
        let text = format!("{b}\n\n[]");
        assert_eq!(
            crate::services::web_result::strip_fallback_banner(&text),
            (true, "[]")
        );
    }

    #[test]
    fn unconfigured_engines_are_dropped_from_the_ladder() {
        // Only searxng + serpapi have keys: google and brave leave the ladder
        // entirely rather than wasting a rung on `not_configured` each call.
        let ladder = order_engines("", None, &[]);
        assert_eq!(
            retain_configured(ladder, &v(&["searxng", "serpapi"])),
            v(&["serpapi", "searxng"])
        );
    }

    #[test]
    fn retain_configured_never_empties_the_ladder() {
        // Nothing configured — keep the original so the caller still gets a real
        // per-engine error instead of silently searching nothing.
        let ladder = order_engines("", None, &[]);
        assert_eq!(retain_configured(ladder.clone(), &[]), ladder);
    }

    #[test]
    fn retain_configured_preserves_order() {
        let ladder = v(&["searxng", "google", "serpapi", "brave"]);
        assert_eq!(
            retain_configured(ladder, &v(&["brave", "searxng"])),
            v(&["searxng", "brave"])
        );
    }

    #[test]
    fn configured_order_overrides_the_default() {
        assert_eq!(
            order_engines("google, serpapi ,searxng", None, &[]),
            v(&["google", "serpapi", "searxng"])
        );
    }

    #[test]
    fn blank_configuration_falls_back_to_the_default() {
        assert_eq!(order_engines("  , ,", None, &[]), v(DEFAULT_ENGINE_ORDER));
    }

    #[test]
    fn requested_engine_is_promoted_without_dropping_the_rest() {
        // The whole point of the ladder: asking for serpapi must not mean
        // "fail if serpapi is down".
        let ladder = order_engines("", Some("serpapi"), &[]);
        assert_eq!(ladder[0], "serpapi");
        assert_eq!(ladder.len(), DEFAULT_ENGINE_ORDER.len());
        assert!(ladder.contains(&"searxng".to_string()));
    }

    #[test]
    fn requested_engine_is_matched_case_insensitively() {
        let ladder = order_engines("", Some("  SerpApi "), &[]);
        assert_eq!(ladder[0], "serpapi");
        // Promotion must not leave a duplicate behind.
        assert_eq!(ladder.iter().filter(|e| *e == "serpapi").count(), 1);
    }

    #[test]
    fn exhausted_engines_are_demoted_not_removed() {
        let ladder = order_engines("", None, &v(&["searxng"]));
        assert_eq!(*ladder.last().unwrap(), "searxng");
        assert_eq!(ladder.len(), DEFAULT_ENGINE_ORDER.len());
    }

    #[test]
    fn exhaustion_outranks_an_explicit_request() {
        // Observed 2026-08-10: SerpApi's free tier was spent, so an explicit
        // engine:"serpapi" step should still be served by a live engine first.
        let ladder = order_engines("", Some("serpapi"), &v(&["serpapi"]));
        assert_eq!(ladder[0], "exa");
        assert_eq!(*ladder.last().unwrap(), "serpapi");
    }

    #[test]
    fn all_engines_exhausted_preserves_order_rather_than_emptying() {
        let ladder = order_engines("", None, &v(DEFAULT_ENGINE_ORDER));
        assert_eq!(ladder, v(DEFAULT_ENGINE_ORDER));
    }

    #[test]
    fn serpapi_exhaustion_is_classified_as_quota() {
        let err = "SerpApi failed: 429 Too Many Requests - {\n  \"error\": \
                   \"Your account has run out of searches.\"\n}";
        assert_eq!(classify_search_error(err), "quota_exhausted");
    }

    #[test]
    fn serpapi_200_quota_error_body_still_classifies_as_quota() {
        // SerpApi reports quota exhaustion as HTTP 200 with an `error` body, not
        // a 429. search_serpapi now surfaces that as `SerpApi error: <msg>`; the
        // ladder must still route it to the quota cooldown, not treat it as a
        // generic api_error (which would keep hammering the exhausted engine).
        let err = "SerpApi error: You've run out of searches for this month.";
        assert_eq!(classify_search_error(err), "quota_exhausted");
    }

    #[test]
    fn serpapi_benign_no_results_is_not_a_quota() {
        // The other 200+error case: a genuinely empty query. It must fall through
        // WITHOUT tripping the 6h quota cooldown — otherwise one obscure query
        // benches SerpApi for everyone.
        let err = "SerpApi error: Google hasn't returned any results for this query.";
        assert_eq!(classify_search_error(err), "api_error");
    }

    #[test]
    fn disabled_google_api_is_not_mistaken_for_a_quota() {
        // A disabled API needs a console click, not a wait for quota reset —
        // conflating the two sends the operator to the wrong place.
        let err = "Google Search failed: 403 - Custom Search API has not been \
                   used in project 125908057711 before or it is disabled.";
        assert_eq!(classify_search_error(err), "api_disabled");
    }

    #[test]
    fn missing_credentials_are_classified_separately() {
        assert_eq!(
            classify_search_error("Brave API key not configured (set BRAVE_API_KEY)"),
            "not_configured"
        );
    }

    #[test]
    fn network_failures_are_classified_separately() {
        assert_eq!(
            classify_search_error("Request failed: error sending request"),
            "network_error"
        );
    }

    #[test]
    fn a_zero_result_answer_does_not_count_as_an_answer() {
        // The signature of the SearXNG DNS outage: HTTP 200, well-formed body,
        // no results. `is_error` is false, so only the count distinguishes it
        // from a working engine — and it must not end the ladder.
        let empty = ToolCallResult::success_text("[]");
        assert_eq!(SearchSkill::result_count(&empty), 0);
        assert_ne!(empty.is_error, Some(true));

        let one = ToolCallResult::success_text(
            r#"[{"title":"t","link":"https://example.com","snippet":"s"}]"#,
        );
        assert_eq!(SearchSkill::result_count(&one), 1);
    }

    #[test]
    fn unparseable_output_counts_as_zero_so_the_ladder_moves_on() {
        // A rung that answers with something that is not a result array is no
        // more useful than one that answers with none.
        let junk = ToolCallResult::success_text("<html>rate limited</html>");
        assert_eq!(SearchSkill::result_count(&junk), 0);
    }

    #[test]
    fn serpapi_quota_demotes_google_news_too_and_vice_versa() {
        // The two engines share one SerpApi account, so exhausting either must
        // move both to the back of the ladder (the sibling expansion happens in
        // engine_ladder; order_engines then demotes both).
        let ladder = order_engines(
            "searxng,google,serpapi,brave",
            Some("google_news"),
            &v(&["serpapi", "google_news"]),
        );
        // Both siblings sink below the unmetered engine even though google_news
        // was explicitly requested.
        assert_eq!(ladder[0], "searxng");
        assert!(
            ladder.iter().position(|e| e == "serpapi") > ladder.iter().position(|e| e == "searxng")
        );
        assert!(
            ladder.iter().position(|e| e == "google_news")
                > ladder.iter().position(|e| e == "searxng")
        );
    }
}

#[cfg(test)]
mod google_news_tests {
    use super::*;

    #[test]
    fn flattens_a_flat_story() {
        let news = vec![json!({
            "title": "Headline",
            "link": "https://example.com/a",
            "snippet": "body",
            "source": {"name": "Example News"},
            "date": "09/04/2026, 07:00 AM, +0000 UTC"
        })];
        let out = flatten_google_news(&news, 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["link"], "https://example.com/a");
        assert_eq!(out[0]["source"], "Example News");
        assert_eq!(out[0]["snippet"], "body");
    }

    #[test]
    fn unrolls_highlight_and_topic_clusters() {
        // A single news_results entry with no top-level story of its own, but a
        // highlight plus a stories cluster, must contribute several headlines.
        let news = vec![json!({
            "highlight": {"title": "Featured", "link": "https://example.com/h"},
            "stories": [
                {"title": "Story 1", "link": "https://example.com/1"},
                {"title": "Story 2", "link": "https://example.com/2"}
            ]
        })];
        let out = flatten_google_news(&news, 5);
        let links: Vec<&str> = out.iter().map(|r| r["link"].as_str().unwrap()).collect();
        assert_eq!(
            links,
            vec![
                "https://example.com/h",
                "https://example.com/1",
                "https://example.com/2"
            ]
        );
    }

    #[test]
    fn drops_group_headers_with_no_link_and_caps_at_count() {
        let news = vec![
            json!({"title": "Topic group with no link of its own"}),
            json!({"title": "Real 1", "link": "https://example.com/1"}),
            json!({"title": "Real 2", "link": "https://example.com/2"}),
            json!({"title": "Real 3", "link": "https://example.com/3"}),
        ];
        let out = flatten_google_news(&news, 2);
        assert_eq!(out.len(), 2);
        // The header (no link) is skipped; the two real stories come through.
        assert_eq!(out[0]["title"], "Real 1");
        assert_eq!(out[1]["title"], "Real 2");
    }

    #[test]
    fn missing_snippet_becomes_empty_not_null() {
        let news = vec![json!({"title": "T", "link": "https://example.com/x"})];
        let out = flatten_google_news(&news, 5);
        assert_eq!(out[0]["snippet"], "");
    }
}

#[cfg(test)]
mod unresponsive_engine_tests {
    use super::*;

    #[test]
    fn parses_the_engine_and_reason_pairs_searxng_actually_sends() {
        // Captured verbatim from the instance on 2026-08-31, mid-outage.
        let json = json!({
            "results": [],
            "unresponsive_engines": [
                ["brave", "too many requests"],
                ["duckduckgo", "CAPTCHA"],
                ["google cse", "too many requests"],
                ["startpage", "CAPTCHA"]
            ]
        });
        let got = parse_unresponsive_engines(&json);
        assert_eq!(got.len(), 4);
        assert_eq!(got[1], ("duckduckgo".into(), "CAPTCHA".into()));
        assert!(format_unresponsive(&got).contains("duckduckgo (CAPTCHA)"));
    }

    #[test]
    fn a_healthy_response_reports_nothing() {
        // Must stay silent on the common path, or the warning becomes noise
        // and stops being read — which is how the outage stayed invisible.
        let json = json!({ "results": [{"title": "x"}], "unresponsive_engines": [] });
        assert!(parse_unresponsive_engines(&json).is_empty());
        assert!(parse_unresponsive_engines(&json!({ "results": [] })).is_empty());
    }

    #[test]
    fn a_single_element_entry_gets_a_placeholder_reason_not_a_guess() {
        let json = json!({ "unresponsive_engines": [["mwmbl"]] });
        assert_eq!(
            parse_unresponsive_engines(&json),
            vec![("mwmbl".to_string(), "unspecified".to_string())]
        );
    }

    #[test]
    fn malformed_entries_are_skipped_rather_than_panicking() {
        // The field is version-dependent; a shape change must degrade to a
        // thinner log line, never take down a search.
        let json = json!({
            "unresponsive_engines": [["ok", "reason"], "not-an-array", [], [42], null]
        });
        assert_eq!(
            parse_unresponsive_engines(&json),
            vec![("ok".to_string(), "reason".to_string())]
        );
    }
}

#[cfg(test)]
mod keyed_engine_tests {
    use super::*;

    #[test]
    fn tavily_results_map_to_link_snippet_date() {
        let j = json!({"results": [
            {"title": "A", "url": "https://apnews.com/a", "content": "snip", "published_date": "2026-09-25"},
            {"title": "no url", "content": "dropped"}
        ]});
        let out = normalize_tavily(&j, 5);
        assert_eq!(out.len(), 1, "a result with no url is not a result");
        assert_eq!(out[0]["link"], "https://apnews.com/a");
        assert_eq!(out[0]["snippet"], "snip");
        assert_eq!(out[0]["date"], "2026-09-25");
    }

    #[test]
    fn tavily_snippets_are_clipped_to_the_budget() {
        let long = "é".repeat(TAVILY_SNIPPET_CHARS + 300);
        let j = json!({"results": [{"title": "T", "url": "https://a.com", "content": long}]});
        let out = normalize_tavily(&j, 5);
        let snip = out[0]["snippet"].as_str().unwrap();
        assert_eq!(
            snip.chars().count(),
            TAVILY_SNIPPET_CHARS + 1,
            "clip + ellipsis"
        );
        assert!(snip.ends_with('…'));
    }

    #[test]
    fn exa_highlights_become_the_snippet() {
        let j = json!({"results": [
            {"title": "B", "url": "https://reuters.com/b", "publishedDate": "2026-09-24T10:00:00Z",
             "highlights": ["first", "second"]},
            {"title": "C", "url": "https://bbc.com/c", "text": "fallback text"}
        ]});
        let out = normalize_exa(&j, 5);
        assert_eq!(out[0]["snippet"], "first … second");
        assert_eq!(out[0]["date"], "2026-09-24T10:00:00Z");
        assert_eq!(out[1]["snippet"], "fallback text");
        assert!(out[1].get("date").is_none());
    }

    #[test]
    fn serper_reads_organic_and_respects_count() {
        let j = json!({"organic": [
            {"title": "1", "link": "https://a.com", "snippet": "s1"},
            {"title": "2", "link": "https://b.com", "snippet": "s2", "date": "2 days ago"}
        ]});
        let out = normalize_serper(&j, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["link"], "https://a.com");
    }

    /// Normalised results must feed the claim verifier's domain count.
    #[test]
    fn normalised_results_count_as_evidence_domains() {
        let out = normalize_serper(
            &json!({"organic": [{"title": "1", "link": "https://www.a.com/x"},
                                 {"title": "2", "link": "https://b.org/y"}]}),
            5,
        );
        let text = serde_json::to_string(&out).unwrap();
        assert_eq!(
            crate::services::claims::evidence_domains(&text),
            vec!["a.com".to_string(), "b.org".to_string()]
        );
    }

    /// Each vendor signals a spent allowance differently; all must demote.
    #[test]
    fn search_pages_are_dropped_and_an_all_echo_answer_counts_as_empty() {
        let mixed = ToolCallResult::success_json(vec![
            json!({"title": "Search X with Ecosia", "link": "https://www.ecosia.org/search?q=x", "snippet": "x"}),
            json!({"title": "Real", "link": "https://www.reuters.com/a", "snippet": "y"}),
        ]);
        let (kept, dropped) = SearchSkill::drop_search_pages(mixed);
        assert_eq!(dropped, 1);
        assert_eq!(SearchSkill::result_count(&kept), 1);

        let echo_only = ToolCallResult::success_json(vec![json!({
            "title": "t", "link": "https://us.search.yahoo.com/search?p=x", "snippet": "x"
        })]);
        let (kept, dropped) = SearchSkill::drop_search_pages(echo_only);
        assert_eq!(dropped, 1);
        assert_eq!(
            SearchSkill::result_count(&kept),
            0,
            "falls through like an empty answer"
        );

        let error = ToolCallResult::error("boom");
        assert_eq!(SearchSkill::drop_search_pages(error).1, 0);
    }

    #[test]
    fn spent_allowances_classify_as_quota_exhausted() {
        for msg in [
            "Exa search failed: quota exhausted — HTTP 402 Payment Required - {}",
            "Tavily search failed: quota exhausted — HTTP 432 <unknown status code> - {}",
            "Serper search failed: quota exhausted — HTTP 400 Bad Request - Not enough credits",
        ] {
            assert_eq!(classify_search_error(msg), "quota_exhausted", "{msg}");
        }
        assert_eq!(
            classify_search_error("Tavily API key not configured (set TAVILY_API_KEY)"),
            "not_configured"
        );
    }
}
