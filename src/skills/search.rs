//! Search Skill - Provides tools for web searching.

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use crate::mcp::protocol::{ToolCallResult, ToolDefinition};
use crate::repository::TelemetryClient;
use crate::skills::Skill;

/// Search Skill implementation.
pub struct SearchSkill {
    client: Client,
    telemetry: Option<TelemetryClient>,
    brave_api_key: Option<String>,
    google_api_key: Option<String>,
    google_cx: Option<String>,
    serpapi_key: Option<String>,
}

impl SearchSkill {
    /// Create a new search skill.
    pub fn new(
        telemetry: Option<TelemetryClient>,
        brave_api_key: Option<String>,
        google_api_key: Option<String>,
        google_cx: Option<String>,
        serpapi_key: Option<String>,
    ) -> Self {
        Self {
            client: Client::new(),
            telemetry,
            brave_api_key,
            google_api_key,
            google_cx,
            serpapi_key,
        }
    }

    // ========================================================================
    // Tool Definitions
    // ========================================================================

    fn search_web_def() -> ToolDefinition {
        ToolDefinition {
            name: "search_web".to_string(),
            description:
                "Search the web for information using a search engine (SerpApi, Brave, or Google)."
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
                    }
                },
                "required": ["query"]
            }),
        }
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

        info!(query = %input.query, engine = %engine, "Searching web");

        match engine.as_str() {
            "serpapi" => self.search_serpapi(&input.query, count).await,
            "brave" => self.search_brave(&input.query, count).await,
            "google" => self.search_google(&input.query, count).await,
            _ => ToolCallResult::error(format!("Unsupported search engine: {}", engine)),
        }
    }

    async fn search_serpapi(&self, query: &str, count: u8) -> ToolCallResult {
        let api_key = match &self.serpapi_key {
            Some(key) => key,
            None => {
                // Log gap: Missing configuration
                if let Some(telemetry) = &self.telemetry {
                    let _ = telemetry.log_knowledge_gap(
                        query,
                        Some("search_web:serpapi"),
                        "missing_tool_config",
                    );
                }
                return ToolCallResult::error("SerpApi key not configured".to_string());
            }
        };

        let url = "https://serpapi.com/search.json";

        let response = self
            .client
            .get(url)
            .query(&[
                ("api_key", api_key.as_str()),
                ("q", query),
                ("num", &count.to_string()),
                ("engine", "google"), // Default to Google engine via SerpApi
            ])
            .send()
            .await;

        match response {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    // Log gap: API failure
                    if let Some(telemetry) = &self.telemetry {
                        let _ = telemetry.log_knowledge_gap(
                            query,
                            Some("search_web:serpapi"),
                            "api_error",
                        );
                    }
                    return ToolCallResult::error(format!("SerpApi failed: {} - {}", status, text));
                }

                match resp.json::<Value>().await {
                    Ok(json) => {
                        let organic_results = json
                            .get("organic_results")
                            .unwrap_or(&json!([]))
                            .as_array()
                            .unwrap_or(&vec![])
                            .iter()
                            .map(|item| {
                                json!({
                                    "title": item.get("title"),
                                    "link": item.get("link"),
                                    "snippet": item.get("snippet")
                                })
                            })
                            .collect::<Vec<_>>();

                        if organic_results.is_empty() {
                            // Log gap: No results found
                            if let Some(telemetry) = &self.telemetry {
                                let _ = telemetry.log_knowledge_gap(
                                    query,
                                    Some("search_web:serpapi"),
                                    "missing_info",
                                );
                            }
                        }

                        ToolCallResult::success_text(
                            serde_json::to_string_pretty(&organic_results).unwrap(),
                        )
                    }
                    Err(e) => {
                        ToolCallResult::error(format!("Failed to parse SerpApi response: {}", e))
                    }
                }
            }
            Err(e) => {
                // Log gap: Network failure
                if let Some(telemetry) = &self.telemetry {
                    let _ = telemetry.log_knowledge_gap(
                        query,
                        Some("search_web:serpapi"),
                        "network_error",
                    );
                }
                ToolCallResult::error(format!("Request failed: {}", e))
            }
        }
    }

    async fn search_brave(&self, query: &str, count: u8) -> ToolCallResult {
        let api_key = match &self.brave_api_key {
            Some(key) => key,
            None => return ToolCallResult::error("Brave API key not configured".to_string()),
        };

        let url = "https://api.search.brave.com/res/v1/web/search";

        let response = self
            .client
            .get(url)
            .header("X-Subscription-Token", api_key)
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
                        // Extract relevant results
                        let results =
                            if let Some(web) = json.get("web").and_then(|w| w.get("results")) {
                                web
                            } else {
                                &json!([])
                            };

                        // Simplify output to save tokens
                        let simplified: Vec<Value> = results
                            .as_array()
                            .unwrap_or(&vec![])
                            .iter()
                            .take(count as usize)
                            .map(|r| {
                                json!({
                                    "title": r.get("title"),
                                    "url": r.get("url"),
                                    "description": r.get("description"),
                                    "age": r.get("age")
                                })
                            })
                            .collect();

                        ToolCallResult::success_text(
                            serde_json::to_string_pretty(&simplified).unwrap(),
                        )
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
        let api_key = match &self.google_api_key {
            Some(key) => key,
            None => return ToolCallResult::error("Google API key not configured".to_string()),
        };
        let cx = match &self.google_cx {
            Some(cx) => cx,
            None => {
                return ToolCallResult::error(
                    "Google Custom Search Engine ID (CX) not configured".to_string(),
                );
            }
        };

        let url = "https://www.googleapis.com/customsearch/v1";

        let response = self
            .client
            .get(url)
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
                                    "title": item.get("title"),
                                    "link": item.get("link"),
                                    "snippet": item.get("snippet")
                                })
                            })
                            .collect::<Vec<_>>();

                        ToolCallResult::success_text(serde_json::to_string_pretty(&items).unwrap())
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

// Input structs
#[derive(Debug, Deserialize)]
struct SearchInput {
    query: String,
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    count: Option<u8>,
}

fn parse_args<T: for<'de> Deserialize<'de>>(arguments: Option<Value>) -> Result<T, ToolCallResult> {
    let args = arguments.unwrap_or(Value::Object(Default::default()));
    serde_json::from_value(args)
        .map_err(|e| ToolCallResult::error(format!("Invalid arguments: {}", e)))
}
