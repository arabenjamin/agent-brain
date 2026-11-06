//! The `apis` plugin's skill: the four tools, sharing one graph, LLM and
//! HTTP client. `ingest_api` lives in `ingest.rs`, `define_api_context` in
//! `context.rs`, the tags both apply in `tags.rs`; the two read tools are
//! here. Each tool's input schema is the bundle's `schemas/tools/<tool>.json`.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use agent_brain_plugin_api::protocol::{net_policy, tool_schema};
use agent_brain_plugin_api::{
    Graph, Http, Llm, Skill, ToolCallResult, ToolDefinition, async_trait,
};

pub struct ApisSkill {
    pub(crate) graph: Arc<dyn Graph>,
    /// Fetches specs and docs pages: under the network policy, no auth.
    pub(crate) client: reqwest::Client,
    /// Only `ingest_api`'s fallbacks (a docs page or a source tree with no
    /// spec) use it; `None` turns them off cleanly.
    pub(crate) llm: Option<Arc<dyn Llm>>,
    /// The core's `http_request`, for `define_api_context`'s probe.
    pub(crate) http: Option<Arc<dyn Http>>,
}

impl ApisSkill {
    pub fn new(
        graph: Arc<dyn Graph>,
        llm: Option<Arc<dyn Llm>>,
        http: Option<Arc<dyn Http>>,
    ) -> Self {
        Self {
            graph,
            // Under the network policy: a model's URL reaches public hosts only.
            client: net_policy::guard(reqwest::Client::builder())
                .timeout(Duration::from_secs(30))
                .user_agent("agent-brain/1.0")
                .build()
                .expect("Failed to build HTTP client"),
            llm,
            http,
        }
    }

    fn list_apis_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_apis".to_string(),
            description: "Every API known to the graph: each (:ApiContext) with its base_url, \
                          provenance, how many (:ApiEndpoint)s have been ingested via \
                          [:EXPOSES], its tags (`kind` from the closed (:ApiKind) list, the \
                          `topics` it serves, `tags_by`: caller, nimble, seed or owner), and \
                          `used_by`: the enabled and disabled workflows whose steps call it \
                          (by context_name or base_url). An empty used_by is an API nothing \
                          uses. This is the deterministic answer to 'what APIs do we \
                          have / know about' — do NOT semantic-search notes for it (that finds \
                          discussion, not the ingested endpoint graph)."
                .to_string(),
            input_schema: tool_schema!(plugin "list_apis"),
        }
    }

    fn list_endpoints_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_endpoints".to_string(),
            description: "Every endpoint (method, path, summary, and `probe`: whether define_api_context ever called it and what it got) an API exposes, for a given \
                          API name. This is the deterministic answer to 'what endpoints does \
                          <API> have' / 'list the endpoints for <API>' — do NOT hand-write \
                          Cypher for it (the graph has more than one endpoint-shaped structure, \
                          and the API name is case-sensitive). Get the exact `api` name from \
                          list_apis first (e.g. `home_assistant`, not `HomeAssistant`)."
                .to_string(),
            input_schema: tool_schema!(plugin "list_endpoints"),
        }
    }

    async fn handle_list_apis(&self) -> ToolCallResult {
        // used_by: a step names the context, or calls a URL under its base_url.
        let cypher = "MATCH (c:ApiContext) \
             OPTIONAL MATCH (c)-[:EXPOSES]->(e:ApiEndpoint) \
             WITH c, count(e) AS endpoints \
             OPTIONAL MATCH (c)-[:SERVES]->(t:Topic) \
             WITH c, endpoints, collect(t.name) AS topics \
             OPTIONAL MATCH (w:Workflow) \
               WHERE w.steps CONTAINS ('\"context_name\":\"' + c.name + '\"') \
                  OR (coalesce(c.base_url, '') <> '' AND w.steps CONTAINS c.base_url) \
             WITH c, endpoints, topics, \
                  collect(CASE WHEN w IS NULL THEN null \
                               ELSE w.name + CASE WHEN w.enabled THEN '' ELSE ' (disabled)' END END) AS used_by \
             RETURN c.name AS name, coalesce(c.base_url, '') AS base_url, \
                    coalesce(c.source, '') AS source, endpoints, \
                    c.api_kind AS kind, topics, c.tags_by AS tags_by, used_by \
             ORDER BY endpoints DESC, name ASC";
        match self.graph.query(cypher, Value::Null).await {
            Ok(rows) => {
                let result = rows;
                let count = result.len();
                // Named, not counted: each is drillable to its row above.
                let names = |f: &dyn Fn(&Value) -> bool| -> Vec<Value> {
                    result
                        .iter()
                        .filter(|r| f(r))
                        .map(|r| r["name"].clone())
                        .collect()
                };
                let untagged = names(&|r| r["kind"].is_null() && r["tags_by"].is_null());
                let unused =
                    names(&|r| r["used_by"].as_array().is_none_or(std::vec::Vec::is_empty));
                ToolCallResult::success_json(json!({
                    "apis": result, "count": count, "untagged": untagged, "used_by_no_workflow": unused,
                }))
            }
            Err(e) => ToolCallResult::error(format!("list_apis query failed: {e}")),
        }
    }

    async fn handle_list_endpoints(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            api: String,
        }
        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {e}")),
        };
        // The one correct structure: (:ApiContext)-[:EXPOSES]->(:ApiEndpoint).
        // Baking it here is the point — a hand-written query can pick the wrong
        // endpoint-shaped path or mis-case the name and silently return zero.
        let cypher = "MATCH (c:ApiContext {name: $api})-[:EXPOSES]->(e:ApiEndpoint) \
             RETURN e.method AS method, e.path AS path, coalesce(e.summary, '') AS summary, \
                    CASE WHEN e.probe IS NULL THEN 'never called' \
                         ELSE e.probe + ' on ' + toString(date(e.probed_at)) END AS probe \
             ORDER BY e.path, e.method";
        match self.graph.query(cypher, json!({ "api": input.api })).await {
            Ok(rows) => {
                let result = rows;
                let count = result.len();
                let mut response = json!({
                    "api": input.api,
                    "endpoints": result,
                    "count": count,
                });
                // Zero is almost always a wrong/mis-cased name, not an API with
                // no endpoints — point at list_apis rather than let the caller
                // conclude the endpoints "aren't stored".
                if count == 0 {
                    response["hint"] = json!(
                        "0 endpoints — the api name is case-sensitive and must match an \
                         ApiContext exactly. Call list_apis to get the exact names (e.g. \
                         `home_assistant`, not `HomeAssistant`), then retry."
                    );
                }
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("list_endpoints query failed: {e}")),
        }
    }
}

#[async_trait]
impl Skill for ApisSkill {
    fn name(&self) -> &str {
        "APIs"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::ingest_api_def(),
            Self::define_api_context_def(),
            Self::list_apis_def(),
            Self::list_endpoints_def(),
        ]
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "ingest_api" => Some(self.handle_ingest_api(args).await),
            "define_api_context" => Some(self.handle_define_api_context(args).await),
            "list_apis" => Some(self.handle_list_apis().await),
            "list_endpoints" => Some(self.handle_list_endpoints(args).await),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tools, in the order they are offered.
    const TOOLS: &[&str] = &[
        "ingest_api",
        "define_api_context",
        "list_apis",
        "list_endpoints",
    ];

    /// A graph that answers every query with no rows.
    struct Empty;

    #[async_trait]
    impl Graph for Empty {
        async fn query(&self, _cypher: &str, _params: Value) -> anyhow::Result<Vec<Value>> {
            Ok(vec![])
        }
    }

    /// Each schema declares the arguments its handler reads: the dispatch
    /// check refuses (enforce) or flags (shadow) any it does not, so a
    /// stub schema left from the template would break every call.
    #[test]
    fn each_schema_declares_what_its_handler_reads() {
        let defs = skill().tools();
        let props = |tool: &str| -> Vec<String> {
            let d = defs.iter().find(|d| d.name == tool).unwrap();
            d.input_schema["properties"]
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default()
        };
        let has = |tool: &str, keys: &[&str]| {
            let p = props(tool);
            for k in keys {
                assert!(
                    p.iter().any(|x| x == k),
                    "{tool}'s schema lacks `{k}`: {p:?}"
                );
            }
        };
        has(
            "ingest_api",
            &[
                "spec_url",
                "docs_url",
                "codebase_path",
                "name",
                "description",
                "prune",
                "kind",
                "topics",
            ],
        );
        has(
            "define_api_context",
            &[
                "name",
                "base_url",
                "auth_scheme",
                "auth_param",
                "auth_env_var",
                "default_headers",
                "description",
                "endpoints",
                "kind",
                "topics",
            ],
        );
        has("list_endpoints", &["api"]);
    }

    fn skill() -> ApisSkill {
        ApisSkill::new(Arc::new(Empty), None, None)
    }

    #[tokio::test]
    async fn every_tool_loads_its_schema_and_is_dispatched() {
        let s = skill();
        let names: Vec<String> = s.tools().into_iter().map(|d| d.name).collect();
        assert_eq!(names, TOOLS);
        for t in TOOLS {
            assert!(
                s.execute(t, Some(json!({}))).await.is_some(),
                "{t} is offered but not dispatched"
            );
        }
    }

    /// Zero endpoints is almost always a mis-cased name: the answer says so.
    #[tokio::test]
    async fn no_endpoints_points_at_list_apis() {
        let r = skill()
            .execute("list_endpoints", Some(json!({"api": "HomeAssistant"})))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&r.model_text()).unwrap();
        assert_eq!(v["count"], 0);
        assert!(v["hint"].as_str().unwrap().contains("list_apis"));
    }

    /// An ingest with no source, or two, is refused before anything is fetched.
    #[tokio::test]
    async fn ingest_needs_exactly_one_source() {
        let r = skill()
            .execute("ingest_api", Some(json!({})))
            .await
            .unwrap();
        assert!(r.model_text().contains("exactly ONE"), "{}", r.model_text());
    }
}
