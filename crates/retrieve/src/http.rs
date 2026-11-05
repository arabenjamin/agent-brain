//! HttpSkill — generic HTTP request primitive with ApiContext-based auth injection.
//!
//! `http_request` executes arbitrary outbound HTTP calls. When `context_name` is
//! supplied the matching `ApiContext` node is loaded from Neo4j and credentials
//! are resolved from the environment variable named in `auth_env_var` — secrets
//! never touch the database.
//!
//! `define_api_context` manages the `ApiContext` nodes that power auth injection.
//! Use `neo4j_query` to inspect contexts: MATCH (c:ApiContext) RETURN c
//!
//! This file holds `http_request` and the tool dispatch. `http/ingest.rs` holds
//! `ingest_api`, `http/api_context.rs` holds `define_api_context`.

use agent_brain_protocol::net_policy;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use agent_brain_llm::LlmProvider;
use agent_brain_protocol::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};
use agent_brain_repository::Neo4jClient;

mod api_context;
mod ingest;
use ingest::*;

// ── Response field projection (http_request `pick`) ─────────────────────────
//
// Keep only the requested paths from a JSON body, dropping everything else, so a
// large payload (e.g. a job board's full descriptions) can be handed to a local
// LLM step without overflowing its context. Missing paths are skipped silently;
// projection never errors.

/// Read a value at a dotted object path (`a.b.c`). Returns None if any segment is absent.
fn get_path<'a>(v: &'a Value, segs: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for s in segs {
        cur = cur.get(*s)?;
    }
    Some(cur)
}

/// Insert `val` at a dotted object path, creating intermediate objects as needed.
fn set_path(target: &mut Value, segs: &[&str], val: Value) {
    if segs.is_empty() {
        return;
    }
    if !target.is_object() {
        *target = json!({});
    }
    let obj = target.as_object_mut().expect("just ensured object");
    if segs.len() == 1 {
        obj.insert(segs[0].to_string(), val);
    } else {
        let entry = obj.entry(segs[0].to_string()).or_insert_with(|| json!({}));
        set_path(entry, &segs[1..], val);
    }
}

/// Project a JSON body down to the given pick paths. See the `pick` tool-schema
/// docs for the path grammar (`field`, `a.b`, `arr[].field`, `arr[]`, `[].field`).
fn project_body(body: &Value, picks: &[String]) -> Value {
    // Split picks into array-groups (share an `arr[]` prefix) and plain paths.
    let mut array_groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut plain: Vec<&String> = Vec::new();
    for p in picks {
        if let Some(idx) = p.find("[].") {
            let prefix = p[..idx].to_string();
            let sub = p[idx + 3..].to_string();
            match array_groups.iter_mut().find(|(pre, _)| *pre == prefix) {
                Some(g) => g.1.push(sub),
                None => array_groups.push((prefix, vec![sub])),
            }
        } else if let Some(prefix) = p.strip_suffix("[]") {
            // Keep each element whole (empty sub-list is the sentinel for that).
            array_groups.push((prefix.to_string(), Vec::new()));
        } else {
            plain.push(p);
        }
    }

    let mut out = json!({});

    for p in &plain {
        let segs: Vec<&str> = p.split('.').collect();
        if let Some(val) = get_path(body, &segs) {
            set_path(&mut out, &segs, val.clone());
        }
    }

    for (prefix, subs) in &array_groups {
        let pre_segs: Vec<&str> = prefix.split('.').collect();
        // An empty prefix means the body itself is the array.
        let arr_val = if prefix.is_empty() {
            Some(body)
        } else {
            get_path(body, &pre_segs)
        };
        let Some(arr) = arr_val.and_then(|v| v.as_array()) else {
            continue;
        };
        let projected: Vec<Value> = arr
            .iter()
            .map(|el| {
                if subs.is_empty() {
                    el.clone()
                } else {
                    let mut eo = json!({});
                    for sub in subs {
                        let sub_segs: Vec<&str> = sub.split('.').collect();
                        if let Some(sv) = get_path(el, &sub_segs) {
                            set_path(&mut eo, &sub_segs, sv.clone());
                        }
                    }
                    eo
                }
            })
            .collect();
        if prefix.is_empty() {
            out = Value::Array(projected);
        } else {
            set_path(&mut out, &pre_segs, Value::Array(projected));
        }
    }

    out
}

pub struct HttpSkill {
    neo4j: Option<Neo4jClient>,
    client: reqwest::Client,
    /// Used only by `ingest_api`'s HTML-with-no-spec fallback (extract endpoints
    /// from a docs page's text). `None` disables that fallback cleanly. Every
    /// deterministic path (spec_url, docs_url with a linked spec, codebase_path)
    /// works without it.
    llm: Option<Arc<dyn LlmProvider>>,
}

impl HttpSkill {
    pub fn new(neo4j: Option<Neo4jClient>, llm: Option<Arc<dyn LlmProvider>>) -> Self {
        Self {
            neo4j,
            // Under the network policy: a model's URL reaches public hosts only
            // (agent_brain_protocol::net_policy).
            client: net_policy::guard(reqwest::Client::builder())
                .timeout(Duration::from_secs(30))
                .user_agent("agent-brain/1.0")
                .build()
                .expect("Failed to build HTTP client"),
            llm,
        }
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    fn http_request_def() -> ToolDefinition {
        ToolDefinition {
            name: "http_request".to_string(),
            description: "Execute an HTTP request. Pass context_name to use a stored \
                          ApiContext: its auth is injected, and a relative url (e.g. \
                          'by-kind/CHAT.json') is resolved against its base_url. \
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
                        "description": "Full URL, or a path relative to the context_name API's base_url"
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
                        "description": "Name of a stored ApiContext (list_apis): injects its auth and resolves a relative url against its base_url"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Request timeout in milliseconds (default: 10000)"
                    },
                    "pick": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of field paths to KEEP from a JSON response body, \
                                        projecting away everything else before returning. Shrinks a large \
                                        payload so a downstream local LLM step does not overflow its context. \
                                        Path forms: `field` or `a.b` (a top-level/nested value); \
                                        `arr[].field` or `arr[].a.b` (keep those fields on each element of the \
                                        array at `arr`); `arr[]` (keep each element whole); `[].field` (the \
                                        response body is itself an array). Missing paths are skipped silently. \
                                        Non-JSON bodies are returned unchanged. e.g. \
                                        [\"total\",\"jobs[].title\",\"jobs[].company_name\",\"jobs[].apply_url\"]."
                    }
                },
                "required": ["method", "url"]
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
        // Every call through a context stamps it: `last_used_at` is what the
        // retirement check (api-retire-check, API_USE_PLAN Phase 4) reads.
        let cypher = "MATCH (c:ApiContext {name: $name}) \
                      SET c.last_used_at = datetime() \
                      RETURN c.base_url AS base_url, c.auth_scheme AS auth_scheme, \
                             c.auth_param AS auth_param, c.auth_env_var AS auth_env_var, \
                             c.default_headers AS default_headers";
        let rows = neo4j
            .execute(neo4rs::query(cypher).param("name", name))
            .await
            .ok()?;
        let row = rows.first()?;
        Some(ApiContext {
            base_url: row
                .get::<String>("base_url")
                .ok()
                .filter(|s| !s.trim().is_empty()),
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
            #[serde(default)]
            pick: Option<Vec<String>>,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {e}")),
        };

        let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(10_000));

        // Load ApiContext if requested. A named context that does not exist is
        // an error, not a silent unauthenticated call.
        let ctx = match input.context_name.as_deref() {
            Some(name) => match self.fetch_context(name).await {
                Some(c) => Some(c),
                None => {
                    return ToolCallResult::error(format!(
                        "No ApiContext named '{name}'; list_apis shows the stored ones. \
                         Nothing was requested."
                    ));
                }
            },
            None => None,
        };
        let url = match request_url(
            &input.url,
            ctx.as_ref().and_then(|c| c.base_url.as_deref()),
            input.context_name.as_deref(),
        ) {
            Ok(u) => u,
            Err(e) => return ToolCallResult::error(e),
        };
        // An IP-literal host never reaches the guarded resolver; judge it here.
        if let Err(why) = net_policy::check_url_str(&url) {
            return ToolCallResult::error(format!("Not requested: {why}"));
        }
        // The context's secret goes only to a host the owner bound it to, and
        // is judged on the URL actually requested: an absolute `url` with a
        // context named would otherwise carry the key anywhere.
        let secret = match ctx
            .as_ref()
            .and_then(|c| c.auth_env_var.as_deref())
            .filter(|n| !n.trim().is_empty())
        {
            Some(name) => match agent_brain_protocol::secrets::resolve(name, &url) {
                Ok(v) => v,
                Err(why) => return ToolCallResult::error(format!("Not requested: {why}")),
            },
            None => None,
        };
        // A keyed request never follows a redirect off its host.
        let builder = if secret.is_some() {
            net_policy::guard_pinned(reqwest::Client::builder())
        } else {
            net_policy::guard(reqwest::Client::builder())
        };
        let client = builder
            .timeout(timeout)
            .user_agent("agent-brain/1.0")
            .build()
            .unwrap_or_else(|_| self.client.clone());

        // Build the request
        let method = match input.method.to_uppercase().as_str() {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            "PUT" => reqwest::Method::PUT,
            "PATCH" => reqwest::Method::PATCH,
            "DELETE" => reqwest::Method::DELETE,
            "HEAD" => reqwest::Method::HEAD,
            other => return ToolCallResult::error(format!("Unknown method: {other}")),
        };

        let mut req = client.request(method, &url);

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
            && let Some(secret) = secret
        {
            match c.auth_scheme.as_str() {
                "bearer" => {
                    req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {secret}"));
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

        debug!(url = %url, method = %input.method, "http_request executing");
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

                // Optional field projection: keep only the requested paths so a
                // large JSON payload does not overflow a downstream local LLM's
                // context. Non-JSON bodies and empty pick lists pass through.
                let projected = matches!(&input.pick, Some(p) if !p.is_empty())
                    && (body_val.is_object() || body_val.is_array());
                let body_out = if projected {
                    project_body(&body_val, input.pick.as_deref().unwrap_or(&[]))
                } else {
                    body_val
                };

                let response = json!({
                    "status_code": status,
                    "ok": (200..300).contains(&status),
                    "duration_ms": duration_ms,
                    "headers": resp_headers,
                    "body": body_out,
                    "body_projected": projected,
                    "context_used": input.context_name,
                    "url": url,
                });
                ToolCallResult::success_json(response)
            }
            Err(e) => match net_policy::refusal_in(&e) {
                Some(why) => ToolCallResult::error(format!("Not requested: {why}")),
                None => ToolCallResult::error(format!("HTTP request to {url} failed: {e}")),
            },
        }
    }
}

// Internal type used during auth injection
struct ApiContext {
    base_url: Option<String>,
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
mod projection_tests {
    use super::project_body;
    use serde_json::json;

    #[test]
    fn projects_array_fields_and_top_level_scalars() {
        let body = json!({
            "total": 2,
            "page": 1,
            "jobs": [
                {"title": "SWE", "company_name": "Acme", "description": "x".repeat(5000), "apply_url": "u1"},
                {"title": "MLE", "company_name": "Beta", "description": "y".repeat(5000), "apply_url": "u2"}
            ]
        });
        let out = project_body(
            &body,
            &[
                "total".into(),
                "jobs[].title".into(),
                "jobs[].apply_url".into(),
            ],
        );
        assert_eq!(out["total"], json!(2));
        assert!(out.get("page").is_none(), "page should be dropped");
        let jobs = out["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0]["title"], json!("SWE"));
        assert_eq!(jobs[0]["apply_url"], json!("u1"));
        assert!(jobs[0].get("description").is_none(), "description dropped");
        assert!(
            jobs[0].get("company_name").is_none(),
            "company_name not picked"
        );
    }

    #[test]
    fn missing_paths_are_skipped_not_errored() {
        let body = json!({"jobs": [{"title": "A"}]});
        let out = project_body(
            &body,
            &[
                "nope".into(),
                "jobs[].missing".into(),
                "jobs[].title".into(),
            ],
        );
        assert!(out.get("nope").is_none());
        let jobs = out["jobs"].as_array().unwrap();
        assert_eq!(jobs[0]["title"], json!("A"));
        assert!(jobs[0].get("missing").is_none());
    }

    #[test]
    fn nested_subpath_and_whole_element() {
        let body = json!({"items": [{"a": {"b": 1, "c": 2}, "keep": true}]});
        let sub = project_body(&body, &["items[].a.b".into()]);
        assert_eq!(sub["items"][0]["a"]["b"], json!(1));
        assert!(sub["items"][0]["a"].get("c").is_none());
        // `arr[]` keeps each element whole.
        let whole = project_body(&body, &["items[]".into()]);
        assert_eq!(whole["items"][0]["keep"], json!(true));
        assert_eq!(whole["items"][0]["a"]["c"], json!(2));
    }

    #[test]
    fn top_level_array_body() {
        let body = json!([{"t": "x", "d": "drop"}, {"t": "y", "d": "drop"}]);
        let out = project_body(&body, &["[].t".into()]);
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["t"], json!("x"));
        assert!(arr[0].get("d").is_none());
    }
}
