//! Search Skill — web search with credentials loaded from ApiContext nodes.
//!
//! API keys are no longer stored as skill fields. Instead each engine's
//! `ApiContext` node in Neo4j holds the env var name; credentials are
//! resolved at call time via `std::env::var`.  Falls back to the legacy
//! env vars (`SERPAPI_KEY`, `BRAVE_API_KEY`, etc.) when no context is found
//! so existing deployments keep working without a schema migration.

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use crate::repository::{Neo4jClient, TelemetryClient};
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};

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
                "Search the web for information using a search engine (SerpApi, Brave, or Google). \
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
                        "description": "Search engine to use: 'serpapi' (default), 'brave', or 'google'",
                        "enum": ["serpapi", "brave", "google"]
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
    // Tool Handlers
    // ========================================================================

    async fn handle_search_web(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SearchInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let engine = input.engine.unwrap_or_else(|| "serpapi".to_string());
        let count = input.count.unwrap_or(5).clamp(1, 20);

        // Resolve source list restriction if requested.
        let (effective_query, allowed_domains) = if let Some(ref list_name) = input.source_list {
            self.apply_source_list(&input.query, list_name).await
        } else {
            (input.query.clone(), vec![])
        };

        info!(
            query = %effective_query,
            engine = %engine,
            source_list = ?input.source_list,
            "Searching web"
        );

        let result = match engine.as_str() {
            "serpapi" => self.search_serpapi(&effective_query, count).await,
            "brave" => self.search_brave(&effective_query, count).await,
            "google" => self.search_google(&effective_query, count).await,
            _ => return ToolCallResult::error(format!("Unsupported search engine: {}", engine)),
        };

        // Post-filter: drop any result whose URL is not from an approved domain.
        if allowed_domains.is_empty() || result.is_error == Some(true) {
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
        vec![Self::search_web_def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "search_web" => Some(self.handle_search_web(arguments).await),
            _ => None,
        }
    }
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
