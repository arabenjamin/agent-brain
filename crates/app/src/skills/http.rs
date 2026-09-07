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

    fn ingest_api_def() -> ToolDefinition {
        ToolDefinition {
            name: "ingest_api".to_string(),
            description: "Ingest an OpenAPI/Swagger specification (JSON or YAML) from a URL and \
                          record its endpoints in the knowledge graph so the brain can find and \
                          call them later. Fetches the spec, parses every operation \
                          (method, path, summary, parameters, security), and stores each as an \
                          (:ApiEndpoint) linked to an (:ApiContext) by [:EXPOSES]. Idempotent — \
                          re-ingesting the same spec updates the endpoints in place. This is the \
                          deterministic, native form of 'understand a REST API'; unlike a \
                          reason-over-the-spec pipeline it does not truncate or hallucinate \
                          endpoints. Query the result with neo4j_query: \
                          MATCH (c:ApiContext {name:$n})-[:EXPOSES]->(e) RETURN e."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "spec_url": {
                        "type": "string",
                        "description": "URL of the OpenAPI/Swagger document (e.g. https://api.example.com/openapi.json)"
                    },
                    "name": {
                        "type": "string",
                        "description": "ApiContext name to store under. Defaults to the spec's info.title (slugged), else the host."
                    },
                    "description": {
                        "type": "string",
                        "description": "Optional human-readable description. Defaults to the spec's info.title."
                    }
                },
                "required": ["spec_url"]
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

    async fn handle_ingest_api(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            spec_url: String,
            #[serde(default)]
            name: Option<String>,
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

        // Fetch the spec. This runs in the brain process (which has egress),
        // never the sandbox.
        let resp = match self.client.get(&input.spec_url).send().await {
            Ok(r) => r,
            Err(e) => {
                return ToolCallResult::error(format!("Failed to fetch spec: {}", e));
            }
        };
        if !resp.status().is_success() {
            return ToolCallResult::error(format!(
                "Spec URL returned {} — expected a fetchable OpenAPI/Swagger document",
                resp.status()
            ));
        }
        let body = match resp.text().await {
            Ok(t) => t,
            Err(e) => return ToolCallResult::error(format!("Failed to read spec body: {}", e)),
        };
        // Size guard — a spec is metadata, not a payload.
        const MAX_SPEC_BYTES: usize = 8 * 1024 * 1024;
        if body.len() > MAX_SPEC_BYTES {
            return ToolCallResult::error(format!(
                "Spec is {} bytes, over the {} byte limit",
                body.len(),
                MAX_SPEC_BYTES
            ));
        }

        // Parse JSON first, then YAML (many specs are served as .yaml).
        let spec: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => match serde_yaml::from_str(&body) {
                Ok(v) => v,
                Err(e) => {
                    return ToolCallResult::error(format!(
                        "Spec is neither valid JSON nor YAML: {}",
                        e
                    ));
                }
            },
        };

        let parsed = match parse_api_spec(&spec, &input.spec_url) {
            Ok(p) => p,
            Err(e) => return ToolCallResult::error(e),
        };

        let name = input
            .name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| derive_api_name(&spec, &input.spec_url));
        let description = input
            .description
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                spec.get("info")
                    .and_then(|i| i.get("title"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .unwrap_or_default();

        // Build the endpoint batch for a single UNWIND round-trip (the same
        // atomic-write pattern the dynamic-tool fix uses: one query MERGEs the
        // context and every endpoint together — no cross-connection MATCH — and
        // a RETURN count confirms the write landed rather than trusting Ok(())).
        let ts = chrono::Utc::now().to_rfc3339();
        let mut batch: Vec<neo4rs::BoltType> = Vec::with_capacity(parsed.endpoints.len());
        for ep in &parsed.endpoints {
            let mut row: HashMap<String, neo4rs::BoltType> = HashMap::new();
            row.insert("method".into(), ep.method.clone().into());
            row.insert("path".into(), ep.path.clone().into());
            row.insert("summary".into(), ep.summary.clone().into());
            row.insert("operation_id".into(), ep.operation_id.clone().into());
            row.insert("parameters".into(), ep.parameters.clone().into());
            row.insert("security".into(), ep.security.clone().into());
            row.insert("tags".into(), ep.tags.clone().into());
            batch.push(neo4rs::BoltType::from(row));
        }

        let cypher = r#"
        MERGE (c:ApiContext {name: $name})
          SET c.base_url     = $base_url,
              c.description   = $description,
              c.spec_url      = $spec_url,
              c.spec_version  = $spec_version,
              c.updated_at    = datetime($ts)
        WITH c
        UNWIND $endpoints AS ep
        MERGE (e:ApiEndpoint {api_name: $name, method: ep.method, path: ep.path})
          SET e.summary      = ep.summary,
              e.operation_id  = ep.operation_id,
              e.parameters    = ep.parameters,
              e.security      = ep.security,
              e.tags          = ep.tags,
              e.updated_at    = datetime($ts)
        MERGE (c)-[:EXPOSES]->(e)
        RETURN count(e) AS n
        "#;

        let rows = match neo4j
            .execute(
                neo4rs::query(cypher)
                    .param("name", name.clone())
                    .param("base_url", parsed.base_url.clone())
                    .param("description", description.clone())
                    .param("spec_url", input.spec_url.clone())
                    .param("spec_version", parsed.version.clone())
                    .param("ts", ts)
                    .param("endpoints", batch),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("Failed to store endpoints: {}", e)),
        };

        let stored = rows
            .first()
            .and_then(|r| r.get::<i64>("n").ok())
            .unwrap_or(0);
        if stored == 0 {
            return ToolCallResult::error(format!(
                "Parsed {} endpoints from '{}' but none persisted",
                parsed.endpoints.len(),
                name
            ));
        }

        // A small sample so the caller can see the shape without a follow-up query.
        let sample: Vec<Value> = parsed
            .endpoints
            .iter()
            .take(8)
            .map(|e| json!({ "method": e.method, "path": e.path, "summary": e.summary }))
            .collect();

        ToolCallResult::success_json(json!({
            "ingested": true,
            "api_name": name,
            "base_url": parsed.base_url,
            "spec_version": parsed.version,
            "endpoints_stored": stored,
            "sample": sample,
            "query_hint": format!(
                "MATCH (c:ApiContext {{name:'{}'}})-[:EXPOSES]->(e:ApiEndpoint) RETURN e.method, e.path ORDER BY e.path",
                name
            ),
        }))
    }
}

/// One parsed operation from an OpenAPI/Swagger spec. String-valued so it maps
/// straight onto Neo4j properties (nested params/security/tags are JSON strings,
/// the same convention `default_headers`/`steps` use elsewhere).
struct ParsedEndpoint {
    method: String,
    path: String,
    summary: String,
    operation_id: String,
    parameters: String,
    security: String,
    tags: String,
}

struct ParsedSpec {
    version: String,
    base_url: String,
    endpoints: Vec<ParsedEndpoint>,
}

/// HTTP operation keys an OpenAPI path item may carry.
const OPENAPI_METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "patch", "head", "options", "trace",
];

/// Parse an OpenAPI 3.x or Swagger 2.0 document into a flat endpoint list.
/// Deterministic — this is the whole point of the native tool over a
/// reason-over-the-spec pipeline that truncates or invents endpoints.
fn parse_api_spec(spec: &Value, spec_url: &str) -> Result<ParsedSpec, String> {
    let paths = spec
        .get("paths")
        .and_then(|p| p.as_object())
        .ok_or_else(|| {
            "spec has no `paths` object — not a recognisable OpenAPI/Swagger document".to_string()
        })?;

    let version = if let Some(v) = spec.get("openapi").and_then(|v| v.as_str()) {
        format!("openapi {v}")
    } else if let Some(v) = spec.get("swagger").and_then(|v| v.as_str()) {
        format!("swagger {v}")
    } else {
        "unknown".to_string()
    };

    let base_url = derive_base_url(spec, spec_url);
    let global_security = extract_security(spec.get("security"));

    let mut endpoints = Vec::new();
    for (path, item) in paths {
        let Some(item_obj) = item.as_object() else {
            continue;
        };
        // Parameters declared at the path level apply to every operation on it.
        let path_params = item_obj.get("parameters");
        for method in OPENAPI_METHODS {
            let Some(op) = item_obj.get(method).and_then(|o| o.as_object()) else {
                continue;
            };
            let summary = op
                .get("summary")
                .and_then(|v| v.as_str())
                .or_else(|| op.get("description").and_then(|v| v.as_str()))
                .unwrap_or("")
                .chars()
                .take(300)
                .collect::<String>();
            let operation_id = op
                .get("operationId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let mut params = parse_parameters(path_params);
            params.extend(parse_parameters(op.get("parameters")));

            let mut security = extract_security(op.get("security"));
            if security.is_empty() {
                security = global_security.clone();
            }

            let tags: Vec<String> = op
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect())
                .unwrap_or_default();

            endpoints.push(ParsedEndpoint {
                method: method.to_uppercase(),
                path: path.clone(),
                summary,
                operation_id,
                parameters: serde_json::to_string(&params).unwrap_or_else(|_| "[]".into()),
                security: serde_json::to_string(&security).unwrap_or_else(|_| "[]".into()),
                tags: serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into()),
            });
        }
    }

    if endpoints.is_empty() {
        return Err("spec parsed but contained no operations under `paths`".to_string());
    }
    Ok(ParsedSpec {
        version,
        base_url,
        endpoints,
    })
}

/// Compact each parameter to {name, in, required, type}, tolerating both 3.x
/// (`schema.type`) and 2.0 (`type`) shapes.
fn parse_parameters(params: Option<&Value>) -> Vec<Value> {
    let Some(arr) = params.and_then(|p| p.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|p| {
            let name = p.get("name").and_then(|v| v.as_str())?;
            let loc = p.get("in").and_then(|v| v.as_str()).unwrap_or("");
            let required = p.get("required").and_then(|v| v.as_bool()).unwrap_or(false);
            let ptype = p
                .get("schema")
                .and_then(|s| s.get("type"))
                .and_then(|v| v.as_str())
                .or_else(|| p.get("type").and_then(|v| v.as_str()))
                .unwrap_or("");
            Some(json!({ "name": name, "in": loc, "required": required, "type": ptype }))
        })
        .collect()
}

/// A `security` requirement is an array of `{schemeName: [scopes]}` maps; we keep
/// the scheme names referenced.
fn extract_security(sec: Option<&Value>) -> Vec<String> {
    let Some(arr) = sec.and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for req in arr {
        if let Some(obj) = req.as_object() {
            for k in obj.keys() {
                if !names.contains(k) {
                    names.push(k.clone());
                }
            }
        }
    }
    names
}

/// Resolve the API base URL: 3.x `servers[0].url` (absolute, or relative to the
/// spec's origin), else 2.0 `schemes[0]://host+basePath`, else the spec URL's
/// origin.
fn derive_base_url(spec: &Value, spec_url: &str) -> String {
    let origin = url::Url::parse(spec_url)
        .ok()
        .map(|u| u.origin().ascii_serialization());

    if let Some(server) = spec
        .get("servers")
        .and_then(|s| s.as_array())
        .and_then(|a| a.first())
        .and_then(|s| s.get("url"))
        .and_then(|u| u.as_str())
        .filter(|s| !s.is_empty())
    {
        if server.starts_with("http://") || server.starts_with("https://") {
            return server.to_string();
        }
        if let Some(o) = &origin {
            return format!("{}{}", o.trim_end_matches('/'), server);
        }
        return server.to_string();
    }

    if let Some(host) = spec.get("host").and_then(|v| v.as_str()) {
        let scheme = spec
            .get("schemes")
            .and_then(|s| s.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("https");
        let base_path = spec.get("basePath").and_then(|v| v.as_str()).unwrap_or("");
        return format!("{scheme}://{host}{base_path}");
    }

    origin.unwrap_or_else(|| spec_url.to_string())
}

/// Default ApiContext name: the spec's `info.title` slugged, else the host.
fn derive_api_name(spec: &Value, spec_url: &str) -> String {
    if let Some(title) = spec
        .get("info")
        .and_then(|i| i.get("title"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        let slug: String = title
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let slug = slug.trim_matches('_').to_string();
        if !slug.is_empty() {
            return slug;
        }
    }
    url::Url::parse(spec_url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| "ingested_api".to_string())
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
            tools.push(Self::ingest_api_def());
        }
        tools
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "http_request" => Some(self.handle_http_request(args).await),
            "define_api_context" => Some(self.handle_define_api_context(args).await),
            "ingest_api" => Some(self.handle_ingest_api(args).await),
            _ => None,
        }
    }
}

#[cfg(test)]
mod ingest_api_tests {
    use super::*;

    fn find<'a>(spec: &'a ParsedSpec, method: &str, path: &str) -> &'a ParsedEndpoint {
        spec.endpoints
            .iter()
            .find(|e| e.method == method && e.path == path)
            .unwrap_or_else(|| panic!("no {method} {path} in parsed spec"))
    }

    #[test]
    fn parses_openapi_3x_endpoints_params_and_security() {
        let spec = json!({
            "openapi": "3.0.1",
            "info": { "title": "Pet Store" },
            "servers": [{ "url": "https://api.pets.example.com/v3" }],
            "paths": {
                "/pet/{petId}": {
                    "parameters": [
                        { "name": "petId", "in": "path", "required": true,
                          "schema": { "type": "integer" } }
                    ],
                    "get": {
                        "operationId": "getPet",
                        "summary": "Find pet by ID",
                        "security": [{ "api_key": [] }],
                        "tags": ["pet"],
                        "parameters": [
                            { "name": "verbose", "in": "query", "required": false,
                              "schema": { "type": "boolean" } }
                        ]
                    }
                }
            }
        });

        let parsed = parse_api_spec(&spec, "https://api.pets.example.com/openapi.json").unwrap();
        assert_eq!(parsed.base_url, "https://api.pets.example.com/v3");
        assert!(parsed.version.starts_with("openapi"));
        assert_eq!(parsed.endpoints.len(), 1);

        let ep = find(&parsed, "GET", "/pet/{petId}");
        assert_eq!(ep.operation_id, "getPet");
        assert_eq!(ep.summary, "Find pet by ID");
        // Path-level param merged with the operation param.
        assert!(ep.parameters.contains("petId"), "params: {}", ep.parameters);
        assert!(ep.parameters.contains("verbose"), "params: {}", ep.parameters);
        assert!(ep.parameters.contains("\"required\":true"));
        assert!(ep.security.contains("api_key"), "security: {}", ep.security);
        assert!(ep.tags.contains("pet"));
    }

    #[test]
    fn parses_swagger_2_0_base_url_from_host_and_basepath() {
        let spec = json!({
            "swagger": "2.0",
            "host": "petstore.swagger.io",
            "basePath": "/v2",
            "schemes": ["https", "http"],
            "paths": {
                "/store/inventory": { "get": { "operationId": "getInventory" } }
            }
        });
        let parsed = parse_api_spec(&spec, "https://petstore.swagger.io/v2/swagger.json").unwrap();
        assert_eq!(parsed.base_url, "https://petstore.swagger.io/v2");
        assert!(parsed.version.starts_with("swagger"));
        find(&parsed, "GET", "/store/inventory");
    }

    #[test]
    fn relative_server_url_resolves_against_spec_origin() {
        let spec = json!({
            "openapi": "3.1.0",
            "servers": [{ "url": "/api" }],
            "paths": { "/ping": { "get": {} } }
        });
        let parsed = parse_api_spec(&spec, "https://svc.example.com/docs/openapi.json").unwrap();
        assert_eq!(parsed.base_url, "https://svc.example.com/api");
    }

    #[test]
    fn a_document_without_paths_is_rejected() {
        let spec = json!({ "openapi": "3.0.0", "info": { "title": "Empty" } });
        assert!(parse_api_spec(&spec, "https://x.example.com/o.json").is_err());
    }

    #[test]
    fn multiple_methods_on_one_path_become_separate_endpoints() {
        let spec = json!({
            "openapi": "3.0.0",
            "paths": {
                "/pet": {
                    "post": { "operationId": "addPet" },
                    "put":  { "operationId": "updatePet" }
                }
            }
        });
        let parsed = parse_api_spec(&spec, "https://x.example.com/o.json").unwrap();
        assert_eq!(parsed.endpoints.len(), 2);
        find(&parsed, "POST", "/pet");
        find(&parsed, "PUT", "/pet");
    }

    #[test]
    fn derive_name_slugs_the_title_then_falls_back_to_host() {
        let titled = json!({ "info": { "title": "My Cool API!" } });
        assert_eq!(derive_api_name(&titled, "https://h.example.com/o.json"), "my_cool_api");
        let untitled = json!({ "paths": {} });
        assert_eq!(derive_api_name(&untitled, "https://h.example.com/o.json"), "h.example.com");
    }
}
