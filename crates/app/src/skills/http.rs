//! HttpSkill — generic HTTP request primitive with ApiContext-based auth injection.
//!
//! `http_request` executes arbitrary outbound HTTP calls. When `context_name` is
//! supplied the matching `ApiContext` node is loaded from Neo4j and credentials
//! are resolved from the environment variable named in `auth_env_var` — secrets
//! never touch the database.
//!
//! `define_api_context` manages the `ApiContext` nodes that power auth injection.
//! Use `neo4j_query` to inspect contexts: MATCH (c:ApiContext) RETURN c

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use crate::repository::Neo4jClient;
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

pub struct HttpSkill {
    neo4j: Option<Neo4jClient>,
    client: reqwest::Client,
}

impl HttpSkill {
    pub fn new(neo4j: Option<Neo4jClient>) -> Self {
        Self {
            neo4j,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent("agent-brain/1.0")
                .build()
                .expect("Failed to build HTTP client"),
        }
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    fn http_request_def() -> ToolDefinition {
        ToolDefinition {
            name: "http_request".to_string(),
            description: "Execute an HTTP request. Pass context_name to auto-inject \
                          authentication from a stored ApiContext. \
                          Use RETURN n.field AS field syntax for Neo4j queries."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"],
                        "description": "HTTP method"
                    },
                    "url": {
                        "type": "string",
                        "description": "Full URL to request"
                    },
                    "headers": {
                        "type": "object",
                        "description": "Additional HTTP headers as key-value string pairs"
                    },
                    "body": {
                        "type": "object",
                        "description": "Request body (sent as JSON for POST/PUT/PATCH)"
                    },
                    "query_params": {
                        "type": "object",
                        "description": "URL query parameters as key-value string pairs"
                    },
                    "context_name": {
                        "type": "string",
                        "description": "Name of a stored ApiContext for automatic auth injection"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Request timeout in milliseconds (default: 10000)"
                    }
                },
                "required": ["method", "url"]
            }),
        }
    }

    fn define_api_context_def() -> ToolDefinition {
        ToolDefinition {
            name: "define_api_context".to_string(),
            description: "Store or update an API context in Neo4j. The context holds \
                          base URL, auth scheme, and the NAME of the environment variable \
                          containing the secret (not the secret itself). \
                          Used by http_request via context_name."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Unique context name (e.g. 'github', 'serpapi')"
                    },
                    "base_url": {
                        "type": "string",
                        "description": "Base URL for the API (e.g. 'https://api.github.com')"
                    },
                    "auth_scheme": {
                        "type": "string",
                        "enum": ["bearer", "query_param", "header", "none"],
                        "description": "How authentication is sent"
                    },
                    "auth_param": {
                        "type": "string",
                        "description": "Header name (for bearer/header) or query param name (for query_param)"
                    },
                    "auth_env_var": {
                        "type": "string",
                        "description": "Environment variable name holding the secret (not the value)"
                    },
                    "default_headers": {
                        "type": "object",
                        "description": "Headers always sent with requests using this context"
                    },
                    "description": {
                        "type": "string",
                        "description": "Human-readable description of what this context is for"
                    }
                },
                "required": ["name", "base_url"]
            }),
        }
    }

    // list_api_contexts is served by GET /api/http-contexts (REST API)

    // =========================================================================
    // Helpers
    // =========================================================================

    /// Load an ApiContext from Neo4j by name. Returns None if not found or no neo4j.
    async fn fetch_context(&self, name: &str) -> Option<ApiContext> {
        let neo4j = self.neo4j.as_ref()?;
        let cypher = "MATCH (c:ApiContext {name: $name}) \
                      RETURN c.base_url AS base_url, c.auth_scheme AS auth_scheme, \
                             c.auth_param AS auth_param, c.auth_env_var AS auth_env_var, \
                             c.default_headers AS default_headers";
        let rows = neo4j
            .execute(neo4rs::query(cypher).param("name", name))
            .await
            .ok()?;
        let row = rows.first()?;
        Some(ApiContext {
            base_url: row.get::<String>("base_url").unwrap_or_default(),
            auth_scheme: row
                .get::<String>("auth_scheme")
                .unwrap_or_else(|_| "none".into()),
            auth_param: row.get::<String>("auth_param").ok(),
            auth_env_var: row.get::<String>("auth_env_var").ok(),
            default_headers: row
                .get::<String>("default_headers")
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default(),
        })
    }

    // =========================================================================
    // Handlers
    // =========================================================================

    async fn handle_http_request(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            method: String,
            url: String,
            #[serde(default)]
            headers: Option<HashMap<String, String>>,
            #[serde(default)]
            body: Option<Value>,
            #[serde(default)]
            query_params: Option<HashMap<String, String>>,
            #[serde(default)]
            context_name: Option<String>,
            #[serde(default)]
            timeout_ms: Option<u64>,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(10_000));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent("agent-brain/1.0")
            .build()
            .unwrap_or_else(|_| self.client.clone());

        // Load ApiContext if requested
        let ctx = if let Some(ref name) = input.context_name {
            self.fetch_context(name).await
        } else {
            None
        };

        // Build the request
        let method = match input.method.to_uppercase().as_str() {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            "PUT" => reqwest::Method::PUT,
            "PATCH" => reqwest::Method::PATCH,
            "DELETE" => reqwest::Method::DELETE,
            "HEAD" => reqwest::Method::HEAD,
            other => return ToolCallResult::error(format!("Unknown method: {}", other)),
        };

        let mut req = client.request(method, &input.url);

        // Inject default headers from context
        if let Some(ref c) = ctx {
            for (k, v) in &c.default_headers {
                req = req.header(k, v);
            }
        }

        // Caller-supplied headers (override context defaults)
        if let Some(ref hdrs) = input.headers {
            for (k, v) in hdrs {
                req = req.header(k, v);
            }
        }

        // Query params
        if let Some(ref qp) = input.query_params {
            req = req.query(qp);
        }

        // Auth injection
        if let Some(ref c) = ctx
            && let Some(ref env_var) = c.auth_env_var
            && let Ok(secret) = std::env::var(env_var)
        {
            match c.auth_scheme.as_str() {
                "bearer" => {
                    req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {}", secret));
                }
                "header" => {
                    if let Some(ref param) = c.auth_param {
                        req = req.header(param.as_str(), secret);
                    }
                }
                "query_param" => {
                    if let Some(ref param) = c.auth_param {
                        req = req.query(&[(param.as_str(), secret.as_str())]);
                    }
                }
                _ => {}
            }
        }

        // Body
        if let Some(ref body) = input.body {
            req = req.json(body);
        }

        debug!(url = %input.url, method = %input.method, "http_request executing");
        let start = Instant::now();

        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let duration_ms = start.elapsed().as_millis() as u64;
                let resp_headers: HashMap<String, String> = resp
                    .headers()
                    .iter()
                    .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.to_string(), s.to_string())))
                    .collect();

                let body_text = resp.text().await.unwrap_or_default();
                let body_val: Value =
                    serde_json::from_str(&body_text).unwrap_or(Value::String(body_text));

                let response = json!({
                    "status_code": status,
                    "ok": (200..300).contains(&status),
                    "duration_ms": duration_ms,
                    "headers": resp_headers,
                    "body": body_val,
                    "context_used": input.context_name,
                });
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("HTTP request failed: {}", e)),
        }
    }

    async fn handle_define_api_context(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            name: String,
            base_url: String,
            #[serde(default)]
            auth_scheme: Option<String>,
            #[serde(default)]
            auth_param: Option<String>,
            #[serde(default)]
            auth_env_var: Option<String>,
            #[serde(default)]
            default_headers: Option<serde_json::Map<String, Value>>,
            #[serde(default)]
            description: Option<String>,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };

        let default_headers_json =
            serde_json::to_string(&input.default_headers.unwrap_or_default())
                .unwrap_or_else(|_| "{}".to_string());

        let cypher = "MERGE (c:ApiContext {name: $name}) \
                      SET c.base_url        = $base_url, \
                          c.auth_scheme     = $auth_scheme, \
                          c.auth_param      = $auth_param, \
                          c.auth_env_var    = $auth_env_var, \
                          c.default_headers = $default_headers, \
                          c.description     = $description, \
                          c.updated_at      = datetime()";

        if let Err(e) = neo4j
            .run(
                neo4rs::query(cypher)
                    .param("name", input.name.clone())
                    .param("base_url", input.base_url.clone())
                    .param(
                        "auth_scheme",
                        input.auth_scheme.unwrap_or_else(|| "none".into()),
                    )
                    .param("auth_param", input.auth_param.unwrap_or_default())
                    .param("auth_env_var", input.auth_env_var.unwrap_or_default())
                    .param("default_headers", default_headers_json)
                    .param("description", input.description.unwrap_or_default()),
            )
            .await
        {
            return ToolCallResult::error(format!("Failed to store ApiContext: {}", e));
        }

        ToolCallResult::success_json(json!({
            "stored": true,
            "name": input.name,
            "base_url": input.base_url,
        }))
    }
}

// Internal type used during auth injection
struct ApiContext {
    #[allow(dead_code)]
    base_url: String,
    auth_scheme: String,
    auth_param: Option<String>,
    auth_env_var: Option<String>,
    default_headers: HashMap<String, String>,
}

#[async_trait]
impl Skill for HttpSkill {
    fn name(&self) -> &str {
        "HTTP"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        let mut tools = vec![Self::http_request_def()];
        if self.neo4j.is_some() {
            tools.push(Self::define_api_context_def());
        }
        tools
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "http_request" => Some(self.handle_http_request(args).await),
            "define_api_context" => Some(self.handle_define_api_context(args).await),
            _ => None,
        }
    }
}
