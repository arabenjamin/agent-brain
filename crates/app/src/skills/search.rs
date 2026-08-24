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
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};

/// Failover order when `SEARCH_ENGINE_ORDER` is unset.
///
/// SearXNG leads deliberately: it is self-hosted and unmetered, so it cannot
/// produce the "account has run out of searches" failure that the keyed engines
/// below it can. The keyed engines are backstops for when the sidecar is down.
const DEFAULT_ENGINE_ORDER: &[&str] = &["searxng", "google", "serpapi", "brave"];

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

/// Classify an engine failure for the usage ledger.
///
/// `quota_exhausted` is the load-bearing case — it is what turns "the brain
/// silently stopped learning" into a queryable, alertable event.
fn classify_search_error(error_text: &str) -> &'static str {
    let lower = error_text.to_lowercase();
    if lower.contains("run out of searches")
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

pub struct SearchSkill {
    client: Client,
    telemetry: Option<TelemetryClient>,
    neo4j: Option<Neo4jClient>,
}

impl SearchSkill {
    pub fn new(telemetry: Option<TelemetryClient>, neo4j: Option<Neo4jClient>) -> Self {
        Self {
            client: Client::new(),
            telemetry,
            neo4j,
        }
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
                "Search the web. Tries engines in a failover ladder (self-hosted SearXNG first, \
                 then Google CSE, SerpApi, Brave) so one exhausted quota does not fail the call. \
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
                                        ladder (default: searxng → google → serpapi → brave). \
                                        Remaining engines are still used as fallbacks.",
                        "enum": ["searxng", "serpapi", "brave", "google"]
                    },
                    "count": {
                        "type": "integer",
                        "description": "Number of results to return (default: 5, max: 20)"
                    },
                    "source_list": {
                        "type": "string",
                        "description": "Name of a SourceList node in Neo4j (e.g. 'news'). \
                                        When set, restricts results to approved domains only — \
                                        adds site: operators to the query and post-filters results."
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
                "Report web-search usage from the telemetry ledger: per-engine totals, failures, \
                 quota-exhaustion counts, and a per-day breakdown. Use this to check burn rate \
                 against a provider's free-tier cap before it runs out."
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

    /// Fetch approved domains from a SourceList node, then build a site: restriction suffix.
    /// Returns (effective_query, allowed_domains) where allowed_domains is used for post-filtering.
    async fn apply_source_list(
        &self,
        query: &str,
        source_list_name: &str,
    ) -> (String, Vec<String>) {
        let Some(ref neo4j) = self.neo4j else {
            return (query.to_string(), vec![]);
        };
        let domains = neo4j
            .get_source_list(source_list_name)
            .await
            .unwrap_or_default();
        if domains.is_empty() {
            return (query.to_string(), vec![]);
        }
        // Use up to 15 domains in the site: restriction (Google query length limit).
        let site_clause = domains
            .iter()
            .take(15)
            .map(|d| format!("site:{d}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let effective_query = format!("{query} ({site_clause})");
        (effective_query, domains)
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
        let exhausted = self
            .telemetry
            .as_ref()
            .and_then(|t| {
                t.search_engines_with_recent_errors("quota_exhausted", QUOTA_COOLDOWN_HOURS)
                    .ok()
            })
            .unwrap_or_default();
        order_engines(&configured, requested, &exhausted)
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

    /// Number of results in a successful engine response.
    fn result_count(result: &ToolCallResult) -> usize {
        serde_json::from_str::<Vec<Value>>(&Self::result_text(result))
            .map(|v| v.len())
            .unwrap_or(0)
    }

    // ========================================================================
    // Tool Handlers
    // ========================================================================

    async fn handle_search_web(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SearchInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let count = input.count.unwrap_or(5).clamp(1, 20);

        // Resolve source list restriction if requested.
        let (effective_query, allowed_domains) = if let Some(ref list_name) = input.source_list {
            self.apply_source_list(&input.query, list_name).await
        } else {
            (input.query.clone(), vec![])
        };

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

        for engine in &ladder {
            let started = Instant::now();
            let attempt = match engine.as_str() {
                "searxng" => self.search_searxng(&effective_query, count).await,
                "serpapi" => self.search_serpapi(&effective_query, count).await,
                "brave" => self.search_brave(&effective_query, count).await,
                "google" => self.search_google(&effective_query, count).await,
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
                continue;
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
                // Keep the first one so an all-empty ladder still returns `[]`
                // in the engine's own shape rather than an error.
                empty_success.get_or_insert(attempt);
                continue;
            }

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

        // Post-filter: drop any result whose URL is not from an approved domain.
        if allowed_domains.is_empty() {
            return result;
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
            return result;
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
        ToolCallResult::success_json(filtered)
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
    async fn search_searxng(&self, query: &str, count: u8) -> ToolCallResult {
        let base_url = self
            .resolve_context_field("searxng", "base_url", "SEARXNG_URL")
            .await
            .unwrap_or_else(|| DEFAULT_SEARXNG_URL.to_string());
        let endpoint = format!("{}/search", base_url.trim_end_matches('/'));

        let response = self
            .client
            .get(&endpoint)
            .query(&[
                ("q", query),
                ("format", "json"),
                ("language", "en"),
                ("safesearch", "0"),
            ])
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
                        let empty = vec![];
                        let results = json
                            .get("results")
                            .and_then(|r| r.as_array())
                            .unwrap_or(&empty)
                            .iter()
                            .take(count as usize)
                            .map(|item| {
                                json!({
                                    "title":   item.get("title"),
                                    "link":    item.get("url"),
                                    "snippet": item.get("content"),
                                })
                            })
                            .collect::<Vec<_>>();
                        // An empty result set is a real answer, not an engine
                        // failure — but it is also the shape a misconfigured
                        // SearXNG returns when every upstream engine is blocked,
                        // so it is worth a knowledge-gap marker.
                        if results.is_empty()
                            && let Some(ref t) = self.telemetry
                        {
                            let _ = t.log_knowledge_gap(
                                query,
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

    async fn search_serpapi(&self, query: &str, count: u8) -> ToolCallResult {
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

    async fn search_brave(&self, query: &str, count: u8) -> ToolCallResult {
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

    async fn search_google(&self, query: &str, count: u8) -> ToolCallResult {
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
    #[serde(default)]
    engine: Option<String>,
    /// Accepts both integer and string values (e.g. `10` or `"10"`) so that
    /// ScheduledTask step definitions stored with quoted counts still work.
    #[serde(default, deserialize_with = "deserialize_optional_count")]
    count: Option<u8>,
    /// Name of a SourceList node in Neo4j. When set, restricts results to approved domains.
    #[serde(default)]
    source_list: Option<String>,
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

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_ladder_leads_with_the_unmetered_engine() {
        assert_eq!(
            order_engines("", None, &[]),
            v(&["searxng", "google", "serpapi", "brave"])
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
        assert_eq!(ladder[0], "searxng");
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
}
