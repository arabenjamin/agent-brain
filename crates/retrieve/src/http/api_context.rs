//! `define_api_context`: register one API by hand and probe its endpoints.

use super::{HttpSkill, ParsedEndpoint};
use crate::api_tags;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};
use agent_brain_repository::Neo4jClient;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;

/// An endpoint `define_api_context` is given for an API with no spec.
#[derive(Debug, Clone, Deserialize)]
struct EndpointInput {
    #[serde(default)]
    method: Option<String>,
    path: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    example: Option<String>,
}

/// What path to call to show an endpoint works: only a GET is called (it
/// changes nothing), and a `{placeholder}` path needs a concrete `example`.
fn probe_target(method: &str, path: &str, example: Option<&str>) -> Result<String, String> {
    if method != "GET" {
        return Err(format!("{method} is not called: a probe only reads"));
    }
    match example.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => Ok(e.to_string()),
        None if path.contains('{') => {
            Err("the path has a {placeholder}; pass `example`, a concrete path".into())
        }
        None => Ok(path.to_string()),
    }
}

/// A probe's outcome as stored on the endpoint: `ok 200`, `failed 404`,
/// `failed (no response)` or `not probed`.
fn probe_verdict(p: &Value) -> String {
    if p["probed"] == json!(false) {
        "not probed".to_string()
    } else if p["ok"] == json!(true) {
        format!("ok {}", p["status"])
    } else {
        match p["status"].as_i64() {
            Some(s) => format!("failed {s}"),
            None => "failed (no response)".to_string(),
        }
    }
}

/// A few real values from a response body, for the caller to quote: an
/// object's first scalar fields, or an array's first items (an object item
/// reduced the same way). Strings are cut at 80 chars.
fn body_sample(body: &Value) -> Value {
    fn scalar(v: &Value) -> Option<Value> {
        match v {
            Value::String(s) if s.chars().count() > 80 => Some(Value::String(format!(
                "{}…",
                s.chars().take(80).collect::<String>()
            ))),
            Value::String(_) | Value::Number(_) | Value::Bool(_) => Some(v.clone()),
            _ => None,
        }
    }
    fn fields(m: &serde_json::Map<String, Value>) -> Value {
        Value::Object(
            m.iter()
                .filter_map(|(k, v)| scalar(v).map(|v| (k.clone(), v)))
                .take(6)
                .collect(),
        )
    }
    match body {
        Value::Object(m) => fields(m),
        Value::Array(a) => Value::Array(
            a.iter()
                .take(3)
                .map(|v| match v {
                    Value::Object(m) => fields(m),
                    other => scalar(other).unwrap_or(Value::Null),
                })
                .collect(),
        ),
        other => scalar(other).unwrap_or(Value::Null),
    }
}

/// A response body in a few words: its top-level keys, or its length.
fn body_shape(body: &Value) -> String {
    match body {
        Value::Object(m) => {
            let keys: Vec<&str> = m.keys().take(8).map(String::as_str).collect();
            format!("JSON object with keys: {}", keys.join(", "))
        }
        Value::Array(a) => format!("JSON array of {} item(s)", a.len()),
        Value::String(s) => format!("text, {} chars", s.chars().count()),
        other => format!("JSON {other}"),
    }
}

impl HttpSkill {
    pub(super) fn define_api_context_def() -> ToolDefinition {
        ToolDefinition {
            name: "define_api_context".to_string(),
            description: "Store or update an API context in Neo4j. The context holds \
                          base URL, auth scheme, and the NAME of the environment variable \
                          containing the secret (not the secret itself). \
                          Used by http_request via context_name. For an API with no \
                          OpenAPI spec, pass `endpoints`: each GET one is called before \
                          it is saved, and the result reports which answered."
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
                    },
                    "kind": {
                        "type": "string",
                        "description": "Its data kind, one of the closed list in the graph ((:ApiKind): news-feed, reference-catalog, forecast, dataset, job-postings, device-state, code-hosting, search, advisories). Omit it and pass no topics to let the tagger (nimble) propose tags for an untagged API; \"\" clears."
                    },
                    "topics": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "The (:Topic) names it serves (0..n); [] for none (weather, a model catalog). A `search` API serves none. Replaces the stored set."
                    },
                    "endpoints": {
                        "type": "array",
                        "description": "For an API with no OpenAPI spec: the endpoints you found, saved so list_endpoints shows them. Each GET endpoint is CALLED once before it is saved, and the result says which answered (status, response shape, and `sample`: a few real values from the response, the only values you may quote): that is the proof it works. A path with a {placeholder} needs `example`, a concrete path to call.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "method": { "type": "string", "description": "Default GET" },
                                "path": { "type": "string", "description": "Relative to base_url, e.g. 'catalog.json' or 'by-kind/{kind}.json'" },
                                "summary": { "type": "string", "description": "What it returns" },
                                "example": { "type": "string", "description": "A concrete path to probe when `path` has a {placeholder}, e.g. 'by-kind/CHAT.json'" }
                            },
                            "required": ["path"]
                        }
                    }
                },
                "required": ["name", "base_url"]
            }),
        }
    }

    pub(super) async fn handle_define_api_context(&self, args: Option<Value>) -> ToolCallResult {
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
            #[serde(default)]
            endpoints: Vec<EndpointInput>,
            #[serde(flatten)]
            tags: api_tags::Requested,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {e}")),
        };

        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };
        // Tags are checked before anything is written.
        let vocab = match api_tags::Vocab::load(neo4j).await {
            Ok(v) => v,
            Err(e) => return ToolCallResult::error(e),
        };
        if let Err(e) = api_tags::check(&input.tags, &vocab) {
            return ToolCallResult::error(e);
        }
        let description = input.description.clone().unwrap_or_default();
        // Stored either way; said now if the key it names will not be sent
        // (the owner binds a secret to its hosts in the secrets index).
        let key_not_sent = input
            .auth_env_var
            .as_deref()
            .filter(|n| !n.trim().is_empty())
            .and_then(|n| agent_brain_protocol::secrets::resolve(n, &input.base_url).err());

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
                          c.created_at      = coalesce(c.created_at, datetime()), \
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
                    .param("description", description.clone()),
            )
            .await
        {
            return ToolCallResult::error(format!("Failed to store ApiContext: {e}"));
        }

        let summaries: Vec<String> = input
            .endpoints
            .iter()
            .map(|e| {
                let m = e.method.as_deref().unwrap_or("GET").trim().to_uppercase();
                format!(
                    "{m} {}: {}",
                    e.path.trim(),
                    e.summary.as_deref().unwrap_or_default()
                )
            })
            .collect();
        let text = api_tags::api_text(&input.name, &description, &summaries);
        let tags = api_tags::apply(neo4j, &input.name, &input.tags, &vocab, &text).await;
        if input.endpoints.is_empty() {
            let mut out = json!({
                "stored": true,
                "name": input.name,
                "base_url": input.base_url,
                "tags": tags,
            });
            if let Some(why) = key_not_sent {
                out["key_not_sent"] = json!(why);
            }
            return ToolCallResult::success_json(out);
        }
        self.store_probed_endpoints(
            neo4j,
            &input.name,
            &input.base_url,
            &description,
            &input.endpoints,
            tags,
        )
        .await
    }

    /// Call each GET endpoint once through `http_request` (the context's auth,
    /// its base_url), then save them all, each stamped with what the call got.
    /// The probe is the tool's, not the caller's choice: the result is the proof.
    async fn store_probed_endpoints(
        &self,
        neo4j: &Neo4jClient,
        name: &str,
        base_url: &str,
        description: &str,
        endpoints: &[EndpointInput],
        tags: Value,
    ) -> ToolCallResult {
        let mut parsed = Vec::with_capacity(endpoints.len());
        let mut probes: Vec<Value> = Vec::with_capacity(endpoints.len());
        for ep in endpoints {
            let method = ep.method.as_deref().unwrap_or("GET").trim().to_uppercase();
            let path = ep.path.trim().to_string();
            let probe = match probe_target(&method, &path, ep.example.as_deref()) {
                Err(why) => json!({ "method": method, "path": path, "probed": false, "why": why }),
                Ok(target) => {
                    let r = self
                        .handle_http_request(Some(json!({
                            "method": "GET", "url": target, "context_name": name,
                        })))
                        .await;
                    let text = r.model_text();
                    match (
                        r.is_error == Some(true),
                        serde_json::from_str::<Value>(&text),
                    ) {
                        (false, Ok(v)) => json!({
                            "method": method, "path": path, "probed": true, "called": v["url"],
                            "status": v["status_code"], "ok": v["ok"],
                            "shape": body_shape(&v["body"]),
                            "sample": body_sample(&v["body"]),
                        }),
                        _ => json!({
                            "method": method, "path": path, "probed": true, "ok": false,
                            "error": text.chars().take(300).collect::<String>(),
                        }),
                    }
                }
            };
            parsed.push(ParsedEndpoint {
                method,
                path,
                summary: ep.summary.clone().unwrap_or_default(),
                operation_id: String::new(),
                parameters: "[]".into(),
                security: "[]".into(),
                tags: "[]".into(),
            });
            probes.push(probe);
        }

        let stored = self
            .persist_endpoints(
                neo4j,
                name,
                base_url,
                "",
                description,
                "",
                &parsed,
                "manual_probe",
                None,
                false,
            )
            .await;
        if stored.is_error == Some(true) {
            return stored;
        }

        // Stamp each endpoint with its probe, so a later reader sees whether it
        // was ever shown to work, and when.
        let rows: Vec<neo4rs::BoltType> = probes
            .iter()
            .map(|p| {
                let mut row: HashMap<String, neo4rs::BoltType> = HashMap::new();
                row.insert("method".into(), p["method"].as_str().unwrap_or("").into());
                row.insert("path".into(), p["path"].as_str().unwrap_or("").into());
                row.insert("verdict".into(), probe_verdict(p).into());
                neo4rs::BoltType::from(row)
            })
            .collect();
        let _ = neo4j
            .run(
                neo4rs::query(
                    "UNWIND $rows AS r \
                     MATCH (e:ApiEndpoint {api_name: $name, method: r.method, path: r.path}) \
                     SET e.probe = r.verdict, e.probed_at = datetime()",
                )
                .param("name", name)
                .param("rows", rows),
            )
            .await;

        let ok = probes.iter().filter(|p| p["ok"] == json!(true)).count();
        let probed = probes.iter().filter(|p| p["probed"] == json!(true)).count();
        ToolCallResult::success_json(json!({
            "stored": true,
            "name": name,
            "base_url": base_url,
            "endpoints_stored": parsed.len(),
            "probed": probed,
            "answered_ok": ok,
            "verdict": if probed > 0 && ok == probed {
                format!("all {probed} probed endpoint(s) answered: the API works as registered")
            } else if ok == 0 {
                "NO probed endpoint answered: the API is registered but NOT shown to work".to_string()
            } else {
                format!("{ok} of {probed} probed endpoint(s) answered; the others are marked failed")
            },
            "probes": probes,
            "tags": tags,
        }))
    }
}

#[cfg(test)]
mod ingest_api_tests {
    use super::*;

    #[test]
    fn only_a_concrete_get_is_probed() {
        assert_eq!(
            probe_target("GET", "catalog.json", None).unwrap(),
            "catalog.json"
        );
        assert!(probe_target("GET", "by-kind/{kind}.json", None).is_err());
        assert_eq!(
            probe_target("GET", "by-kind/{kind}.json", Some("by-kind/CHAT.json")).unwrap(),
            "by-kind/CHAT.json"
        );
        assert!(probe_target("POST", "items", None).is_err());
        assert_eq!(
            probe_verdict(&json!({"probed": true, "ok": true, "status": 200})),
            "ok 200"
        );
        assert_eq!(
            probe_verdict(&json!({"probed": true, "ok": false, "status": 404})),
            "failed 404"
        );
        assert_eq!(
            probe_verdict(&json!({"probed": true, "ok": false})),
            "failed (no response)"
        );
        assert_eq!(probe_verdict(&json!({"probed": false})), "not probed");
        assert_eq!(body_shape(&json!([1, 2])), "JSON array of 2 item(s)");
        // The sample is real values, never containers: what a caller may quote.
        assert_eq!(
            body_sample(&json!([45601, 45602, 45603, 45604])),
            json!([45601, 45602, 45603])
        );
        assert_eq!(
            body_sample(&json!({"by": "pg", "id": 1, "kids": [15], "title": "Y Combinator"})),
            json!({"by": "pg", "id": 1, "title": "Y Combinator"})
        );
    }
}
