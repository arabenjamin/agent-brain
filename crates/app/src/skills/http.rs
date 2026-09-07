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
use std::path::{Path, PathBuf};
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
            description:
                "Ingest a REST API's endpoints into the knowledge graph from ONE of three \
                          sources (pass exactly one): `spec_url` — a direct OpenAPI/Swagger \
                          document (JSON or YAML); `docs_url` — an HTML docs page (Swagger UI, \
                          Redoc, springdoc, …), from which the linked spec is auto-detected and \
                          fetched; or `codebase_path` — a local directory searched for an \
                          openapi/swagger spec file. Parses every operation deterministically \
                          (method, path, summary, parameters, security) and stores each as an \
                          (:ApiEndpoint) linked to an (:ApiContext) by [:EXPOSES]; the context \
                          records how it was ingested (`source`). Idempotent — re-ingesting \
                          updates in place. Deterministic (no LLM), so no truncation or \
                          hallucinated endpoints. Note: an HTML page with NO embedded spec, or a \
                          codebase with no spec file, is not yet supported (LLM extraction / \
                          build-from-routes are deferred) and returns a clear error. Query the \
                          result with neo4j_query: \
                          MATCH (c:ApiContext {name:$n})-[:EXPOSES]->(e) RETURN e."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "spec_url": {
                        "type": "string",
                        "description": "URL of an OpenAPI/Swagger document (e.g. https://api.example.com/openapi.json)"
                    },
                    "docs_url": {
                        "type": "string",
                        "description": "URL of an HTML API-docs page (Swagger UI / Redoc / springdoc). The linked spec is auto-detected and fetched."
                    },
                    "codebase_path": {
                        "type": "string",
                        "description": "Local directory to search for an existing openapi/swagger spec file (openapi.json/.yaml, swagger.json/.yaml, api-docs.json)."
                    },
                    "name": {
                        "type": "string",
                        "description": "ApiContext name to store under. Defaults to the spec's info.title (slugged), else the host."
                    },
                    "description": {
                        "type": "string",
                        "description": "Optional human-readable description. Defaults to the spec's info.title."
                    }
                }
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

    /// Fetch a URL as text with a status check and size guard. Runs in the brain
    /// process (which has egress), never the sandbox.
    async fn fetch_text(&self, url: &str) -> Result<String, String> {
        const MAX_BYTES: usize = 8 * 1024 * 1024;
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("Failed to fetch {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{url} returned {}", resp.status()));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read body from {url}: {e}"))?;
        if body.len() > MAX_BYTES {
            return Err(format!(
                "{url} is {} bytes, over the {MAX_BYTES} byte limit",
                body.len()
            ));
        }
        Ok(body)
    }

    /// Find the spec URL an HTML docs page references. Two passes: first scan the
    /// HTML directly, then — because modern Swagger UI keeps the `url:` in a
    /// separate `swagger-initializer.js` — scan the referenced `.js` files
    /// (initializer-like first), bounded to a few fetches.
    async fn resolve_spec_url_from_docs(&self, docs_url: &str) -> Result<String, String> {
        let html = self.fetch_text(docs_url).await?;
        if let Some(u) = detect_spec_url_in_html(&html, docs_url) {
            return Ok(u);
        }
        for js in find_js_refs(&html, docs_url).into_iter().take(5) {
            if let Ok(js_body) = self.fetch_text(&js).await
                && let Some(u) = detect_spec_url_in_html(&js_body, &js)
            {
                return Ok(u);
            }
        }
        Err(format!(
            "No OpenAPI/Swagger spec is linked from {docs_url} (checked the page and its \
             scripts). Hand-authored HTML docs with no embedded spec need LLM extraction, which \
             is not yet wired — pass a spec_url directly, or a docs page rendered by Swagger UI \
             / Redoc / springdoc."
        ))
    }

    async fn handle_ingest_api(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            #[serde(default)]
            spec_url: Option<String>,
            #[serde(default)]
            docs_url: Option<String>,
            #[serde(default)]
            codebase_path: Option<String>,
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

        // Exactly one source.
        let set = [
            input.spec_url.is_some(),
            input.docs_url.is_some(),
            input.codebase_path.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count();
        if set != 1 {
            return ToolCallResult::error(
                "Provide exactly ONE of: spec_url (a spec document), docs_url (an HTML docs \
                 page), or codebase_path (a local repo dir)."
                    .to_string(),
            );
        }

        // Resolve the source to a parsed spec Value + provenance.
        // (spec, origin_url used for base_url resolution, source label, discovered_from).
        let (spec, origin_url, source, discovered_from): (Value, String, &str, Option<String>) =
            if let Some(spec_url) = input.spec_url.clone() {
                match self
                    .fetch_text(&spec_url)
                    .await
                    .and_then(|b| parse_spec_text(&b))
                {
                    Ok(spec) => (spec, spec_url, "openapi_spec", None),
                    Err(e) => return ToolCallResult::error(e),
                }
            } else if let Some(docs_url) = input.docs_url.clone() {
                let spec_url = match self.resolve_spec_url_from_docs(&docs_url).await {
                    Ok(u) => u,
                    Err(e) => return ToolCallResult::error(e),
                };
                match self
                    .fetch_text(&spec_url)
                    .await
                    .and_then(|b| parse_spec_text(&b))
                {
                    Ok(spec) => (spec, spec_url, "html_spec", Some(docs_url)),
                    Err(e) => {
                        return ToolCallResult::error(format!(
                            "Found a spec link ({spec_url}) in {docs_url} but could not load it: {e}"
                        ));
                    }
                }
            } else {
                let path = input.codebase_path.clone().unwrap_or_default();
                let Some(spec_file) = find_spec_file(Path::new(&path)) else {
                    return ToolCallResult::error(format!(
                        "No OpenAPI/Swagger spec file (openapi/swagger .json/.yaml, api-docs.json) \
                         found under {path}. Building a spec from route definitions is not yet \
                         supported — add a spec file to the repo, or use spec_url / docs_url."
                    ));
                };
                match std::fs::read_to_string(&spec_file)
                    .map_err(|e| format!("Failed to read {}: {e}", spec_file.display()))
                    .and_then(|b| parse_spec_text(&b))
                {
                    Ok(spec) => (
                        spec,
                        spec_file.display().to_string(),
                        "codebase_spec",
                        Some(path),
                    ),
                    Err(e) => return ToolCallResult::error(e),
                }
            };

        self.persist_spec(
            neo4j,
            &spec,
            &origin_url,
            input.name,
            input.description,
            source,
            discovered_from,
        )
        .await
    }

    /// Parse a spec Value and persist its endpoints. Shared by all three sources;
    /// `source`/`discovered_from` record the provenance on the ApiContext.
    #[allow(clippy::too_many_arguments)]
    async fn persist_spec(
        &self,
        neo4j: &Neo4jClient,
        spec: &Value,
        origin_url: &str,
        name_override: Option<String>,
        description_override: Option<String>,
        source: &str,
        discovered_from: Option<String>,
    ) -> ToolCallResult {
        let parsed = match parse_api_spec(spec, origin_url) {
            Ok(p) => p,
            Err(e) => return ToolCallResult::error(e),
        };

        let name = name_override
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| derive_api_name(spec, origin_url));
        let description = description_override
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                spec.get("info")
                    .and_then(|i| i.get("title"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .unwrap_or_default();

        // One UNWIND round-trip: the atomic-write pattern (one query MERGEs the
        // context and every endpoint together — no cross-connection MATCH — and a
        // RETURN count confirms the write landed rather than trusting Ok(())).
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
          SET c.base_url        = $base_url,
              c.description      = $description,
              c.spec_url         = $spec_url,
              c.spec_version     = $spec_version,
              c.source           = $source,
              c.discovered_from  = $discovered_from,
              c.updated_at       = datetime($ts)
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
                    .param("spec_url", origin_url.to_string())
                    .param("spec_version", parsed.version.clone())
                    .param("source", source.to_string())
                    .param(
                        "discovered_from",
                        discovered_from.clone().unwrap_or_default(),
                    )
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
            "source": source,
            "discovered_from": discovered_from,
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
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.as_str().map(String::from))
                        .collect()
                })
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

/// Parse spec text as JSON, then YAML (many specs are served as .yaml).
fn parse_spec_text(body: &str) -> Result<Value, String> {
    serde_json::from_str(body)
        .or_else(|_| serde_yaml::from_str(body))
        .map_err(|e| format!("Spec is neither valid JSON nor YAML: {e}"))
}

/// Find the OpenAPI/Swagger spec URL an HTML docs page references. Handles the
/// common renderers: Swagger UI (`SwaggerUIBundle({url: "…"})`), Redoc
/// (`spec-url="…"`), springdoc (`/v3/api-docs`), and any `<script>/<link>` that
/// points at an openapi/swagger `.json`/`.yaml`. Deterministic token scan — no
/// LLM. When a page advertises several specs (e.g. the Swagger demo's dropdown),
/// prefers one on the SAME host as the page — a docs page's own spec is the
/// intended one — else the first found. Resolved absolute against `base_url`.
fn detect_spec_url_in_html(html: &str, base_url: &str) -> Option<String> {
    let is_specish = |tok: &str| -> bool {
        let t = tok.to_lowercase();
        let looks_pathy = t.starts_with("http") || t.contains('/');
        if !looks_pathy {
            return false;
        }
        let refs_spec = t.contains("openapi") || t.contains("swagger") || t.contains("api-docs");
        let doc_ext = t.ends_with(".json") || t.ends_with(".yaml") || t.ends_with(".yml");
        // A spec URL either ends in a doc extension while naming openapi/swagger,
        // or is a springdoc-style api-docs endpoint.
        (refs_spec && doc_ext) || t.contains("api-docs")
    };
    // Split on quotes/brackets/whitespace to get candidate tokens (URLs and
    // attribute values both fall out cleanly this way). Collect all, then rank.
    let mut candidates: Vec<String> = Vec::new();
    for tok in html.split(|c: char| {
        matches!(
            c,
            '"' | '\'' | '`' | '<' | '>' | '(' | ')' | ' ' | '\n' | '\r' | '\t' | ','
        )
    }) {
        let tok = tok.trim();
        if tok.is_empty() || tok.len() > 512 {
            continue;
        }
        // Initializers often write `host=https://…/openapi.json` (a key=value
        // map entry). Slice from an embedded absolute URL so the key doesn't
        // corrupt the token; relative tokens (no scheme) pass through unchanged.
        let tok = match tok.find("https://").or_else(|| tok.find("http://")) {
            Some(pos) => &tok[pos..],
            None => tok,
        };
        if is_specish(tok) {
            let resolved = resolve_url(tok, base_url);
            if !candidates.contains(&resolved) {
                candidates.push(resolved);
            }
        }
    }
    if candidates.is_empty() {
        return None;
    }
    // Prefer a candidate on the same host as the docs page.
    if let Some(host) = url::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        && let Some(same) = candidates.iter().find(|c| {
            url::Url::parse(c)
                .ok()
                .and_then(|u| u.host_str().map(String::from))
                .as_deref()
                == Some(host.as_str())
        })
    {
        return Some(same.clone());
    }
    Some(candidates.remove(0))
}

/// Collect `.js` URLs referenced by an HTML page (script srcs and any other `.js`
/// token), resolved absolute, initializer-like ones first. Token-based like
/// `detect_spec_url_in_html` — robust without full HTML parsing.
fn find_js_refs(html: &str, base_url: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tok in html.split(|c: char| {
        matches!(
            c,
            '"' | '\'' | '`' | '<' | '>' | '(' | ')' | ' ' | '\n' | '\r' | '\t' | ','
        )
    }) {
        let t = tok.trim();
        if t.is_empty() || t.len() > 512 {
            continue;
        }
        if t.to_lowercase().ends_with(".js") {
            let url = resolve_url(t, base_url);
            if !out.contains(&url) {
                out.push(url);
            }
        }
    }
    out.sort_by_key(|s| {
        let sl = s.to_lowercase();
        if sl.contains("initializer") || sl.contains("swagger-ui-init") {
            0
        } else if sl.contains("swagger") || sl.contains("redoc") {
            1
        } else {
            2
        }
    });
    out
}

/// Resolve a possibly-relative URL against a base page URL.
fn resolve_url(candidate: &str, base_url: &str) -> String {
    if candidate.starts_with("http://") || candidate.starts_with("https://") {
        return candidate.to_string();
    }
    url::Url::parse(base_url)
        .ok()
        .and_then(|b| b.join(candidate).ok())
        .map(|u| u.to_string())
        .unwrap_or_else(|| candidate.to_string())
}

/// Names an OpenAPI/Swagger spec file may take at the root of a project.
const SPEC_FILE_NAMES: [&str; 7] = [
    "openapi.json",
    "openapi.yaml",
    "openapi.yml",
    "swagger.json",
    "swagger.yaml",
    "swagger.yml",
    "api-docs.json",
];

/// Search a directory tree for an existing spec file (bounded; skips vendor/noise
/// dirs). If `root` is a file, it is returned directly.
fn find_spec_file(root: &Path) -> Option<PathBuf> {
    if root.is_file() {
        return Some(root.to_path_buf());
    }
    const MAX_ENTRIES: usize = 20_000;
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_ENTRIES {
                return None;
            }
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if matches!(
                    name,
                    ".git"
                        | "node_modules"
                        | "target"
                        | "venv"
                        | ".venv"
                        | "dist"
                        | "build"
                        | ".tox"
                ) {
                    continue;
                }
                stack.push(path);
            } else if let Some(fname) = path.file_name().and_then(|s| s.to_str())
                && SPEC_FILE_NAMES
                    .iter()
                    .any(|s| fname.eq_ignore_ascii_case(s))
            {
                return Some(path);
            }
        }
    }
    None
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
        assert!(
            ep.parameters.contains("verbose"),
            "params: {}",
            ep.parameters
        );
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
        assert_eq!(
            derive_api_name(&titled, "https://h.example.com/o.json"),
            "my_cool_api"
        );
        let untitled = json!({ "paths": {} });
        assert_eq!(
            derive_api_name(&untitled, "https://h.example.com/o.json"),
            "h.example.com"
        );
    }

    #[test]
    fn detects_swagger_ui_and_redoc_and_springdoc_spec_links() {
        // Swagger UI inline config with an absolute URL.
        let swui =
            r#"<script>SwaggerUIBundle({ url: "https://api.x.com/v3/openapi.json" })</script>"#;
        assert_eq!(
            detect_spec_url_in_html(swui, "https://api.x.com/docs"),
            Some("https://api.x.com/v3/openapi.json".to_string())
        );
        // Redoc with a relative spec-url — resolved against the page.
        let redoc = r#"<redoc spec-url="/static/swagger.yaml"></redoc>"#;
        assert_eq!(
            detect_spec_url_in_html(redoc, "https://api.x.com/docs/index.html"),
            Some("https://api.x.com/static/swagger.yaml".to_string())
        );
        // springdoc api-docs endpoint.
        let springdoc = r#"<a href="/v3/api-docs">spec</a>"#;
        assert_eq!(
            detect_spec_url_in_html(springdoc, "https://api.x.com/swagger-ui.html"),
            Some("https://api.x.com/v3/api-docs".to_string())
        );
    }

    #[test]
    fn prefers_same_host_spec_and_unwraps_key_value_tokens() {
        // A Swagger-demo-style initializer: a key=value map of several specs, one
        // relative to the page's own host. The same-host one must win, and the
        // `host=https://…` wrapping must not corrupt the URL.
        let js = "const urls=`petstore3.swagger.io=https://petstore3.swagger.io/api/v3/openapi.json,\
                  petstore.swagger.io=https://petstore.swagger.io/v2/swagger.json`;";
        assert_eq!(
            detect_spec_url_in_html(js, "https://petstore3.swagger.io/swagger-initializer.js"),
            Some("https://petstore3.swagger.io/api/v3/openapi.json".to_string())
        );
    }

    #[test]
    fn returns_none_when_html_has_no_spec_link() {
        let plain = "<html><body><h1>Our API</h1><p>GET /users returns users.</p></body></html>";
        assert_eq!(detect_spec_url_in_html(plain, "https://x.com/docs"), None);
    }

    #[test]
    fn find_js_refs_resolves_and_prioritizes_initializer() {
        let html = r#"<script src="https://cdn/swagger-ui-bundle.js"></script>
                      <script src="./swagger-initializer.js"></script>"#;
        let refs = find_js_refs(html, "https://api.x.com/docs/");
        // Initializer-like comes first even though it appears second in the HTML.
        assert_eq!(
            refs.first().unwrap(),
            "https://api.x.com/docs/swagger-initializer.js"
        );
        assert!(refs.iter().any(|r| r.contains("swagger-ui-bundle.js")));
    }

    #[test]
    fn parse_spec_text_accepts_json_and_yaml() {
        assert!(parse_spec_text(r#"{"openapi":"3.0.0","paths":{}}"#).is_ok());
        assert!(parse_spec_text("openapi: 3.0.0\npaths: {}\n").is_ok());
        // An unclosed flow mapping is invalid as both JSON and YAML.
        assert!(parse_spec_text("{ unclosed: ").is_err());
    }

    #[test]
    fn find_spec_file_locates_a_spec_and_skips_noise_dirs() {
        let dir = tempfile::tempdir().unwrap();
        // Noise dir that must be skipped even though it contains a spec-named file.
        let nm = dir.path().join("node_modules");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join("openapi.json"), "{}").unwrap();
        // The real spec, one level down in source.
        let src = dir.path().join("api");
        std::fs::create_dir_all(&src).unwrap();
        let spec = src.join("openapi.yaml");
        std::fs::write(&spec, "openapi: 3.0.0").unwrap();

        let found = find_spec_file(dir.path()).expect("should find a spec");
        assert!(
            found.ends_with("api/openapi.yaml"),
            "found: {}",
            found.display()
        );

        // An empty tree yields nothing.
        let empty = tempfile::tempdir().unwrap();
        assert!(find_spec_file(empty.path()).is_none());
    }
}
