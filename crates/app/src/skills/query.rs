//! QuerySkill — generic Neo4j (Cypher) and DuckDB (SQL) query primitives.
//!
//! These tools give the agent direct read access to both databases without
//! requiring a purpose-built Rust tool for every possible query.  Write access
//! to Neo4j is guarded by a keyword allowlist; DuckDB is always read-only.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::repository::{Neo4jClient, TelemetryClient};
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

pub struct QuerySkill {
    neo4j: Option<Neo4jClient>,
    telemetry: Option<TelemetryClient>,
}

impl QuerySkill {
    pub fn new(neo4j: Option<Neo4jClient>, telemetry: Option<TelemetryClient>) -> Self {
        Self { neo4j, telemetry }
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    fn neo4j_query_def() -> ToolDefinition {
        ToolDefinition {
            name: "neo4j_query".to_string(),
            description: "Execute a Cypher query against Neo4j. \
                          Read-only by default (readonly=true); set readonly=false to allow \
                          CREATE/MERGE/SET/DELETE. Use params for safe parameter binding."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cypher": {
                        "type": "string",
                        "description": "Cypher query string"
                    },
                    "params": {
                        "type": "object",
                        "description": "Query parameters as key-value pairs (string values only)"
                    },
                    "readonly": {
                        "type": "boolean",
                        "description": "Reject write keywords when true (default: true)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum rows to return (default: 100)"
                    }
                },
                "required": ["cypher"]
            }),
        }
    }

    fn duckdb_query_def() -> ToolDefinition {
        ToolDefinition {
            name: "duckdb_query".to_string(),
            description: "Execute a read-only SQL SELECT query against the DuckDB analytics \
                          database (telemetry, model usage stats, interaction logs). \
                          Tables: model_usage, model_registry, interactions, knowledge_gaps."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "SQL SELECT query"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum rows to return (default: 100)"
                    }
                },
                "required": ["sql"]
            }),
        }
    }

    // =========================================================================
    // Handlers
    // =========================================================================

    async fn handle_neo4j_query(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            cypher: String,
            #[serde(default)]
            params: Option<serde_json::Map<String, Value>>,
            #[serde(default = "default_true")]
            readonly: bool,
            #[serde(default = "default_limit")]
            limit: usize,
        }
        fn default_true() -> bool {
            true
        }
        fn default_limit() -> usize {
            100
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        // Safety guard for read-only mode — checked before the DB call so it
        // fires even when Neo4j is unavailable (fail-fast on bad input).
        if input.readonly {
            let upper = input.cypher.to_uppercase();
            for kw in &[
                "CREATE", "MERGE", "SET", "DELETE", "REMOVE", "DETACH", "DROP",
            ] {
                if upper.split_whitespace().any(|w| w.starts_with(kw)) {
                    return ToolCallResult::error(format!(
                        "Write keyword '{}' rejected in readonly mode. Pass readonly=false to allow writes.",
                        kw
                    ));
                }
            }
        }

        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };

        // Inject LIMIT if not already present and query returns rows
        let cypher = {
            let upper = input.cypher.trim().to_uppercase();
            if upper.contains("RETURN") && !upper.contains(" LIMIT ") {
                format!("{} LIMIT {}", input.cypher.trim(), input.limit)
            } else {
                input.cypher.clone()
            }
        };

        // Build the query with params
        let mut q = neo4rs::query(&cypher);
        if let Some(params) = &input.params {
            for (k, v) in params {
                match v {
                    Value::String(s) => q = q.param(k.as_str(), s.clone()),
                    Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            q = q.param(k.as_str(), i);
                        } else if let Some(f) = n.as_f64() {
                            q = q.param(k.as_str(), f);
                        }
                    }
                    Value::Bool(b) => q = q.param(k.as_str(), *b),
                    _ => q = q.param(k.as_str(), v.to_string()),
                }
            }
        }

        match neo4j.execute(q).await {
            Ok(rows) => {
                let result: Vec<Value> = rows
                    .iter()
                    .map(|row| {
                        // Convert row to a JSON object by collecting all known keys.
                        // neo4rs rows expose values via typed get; we try common types.
                        // Deserialize the row into a serde_json::Value.
                        // Use RETURN n.field AS field aliases in queries for predictable output.
                        row.to::<Value>().unwrap_or(Value::Null)
                    })
                    .collect();

                let count = result.len();
                let response = json!({
                    "rows": result,
                    "count": count,
                });
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("Neo4j query failed: {}", e)),
        }
    }

    async fn handle_duckdb_query(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            sql: String,
            #[serde(default = "default_limit")]
            limit: usize,
        }
        fn default_limit() -> usize {
            100
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        let Some(ref telemetry) = self.telemetry else {
            return ToolCallResult::error(
                "DuckDB not available (TELEMETRY_DB_PATH not set)".to_string(),
            );
        };

        match telemetry.query_raw(&input.sql, input.limit) {
            Ok(rows) => {
                let count = rows.len();
                let response = json!({
                    "rows": rows,
                    "count": count,
                });
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("DuckDB query failed: {}", e)),
        }
    }
}

#[async_trait]
impl Skill for QuerySkill {
    fn name(&self) -> &str {
        "Query"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        let mut tools = vec![Self::neo4j_query_def()];
        if self.telemetry.is_some() {
            tools.push(Self::duckdb_query_def());
        }
        tools
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "neo4j_query" => Some(self.handle_neo4j_query(args).await),
            "duckdb_query" => Some(self.handle_duckdb_query(args).await),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::test_helpers::*;

    fn skill_no_db() -> QuerySkill {
        QuerySkill::new(None, None)
    }

    // -- tool registry --------------------------------------------------------

    #[test]
    fn tools_without_db_has_only_neo4j_query() {
        let tools = skill_no_db().tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "neo4j_query");
    }

    #[test]
    fn execute_unknown_tool_returns_none() {
        let r = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(skill_no_db().execute("not_a_tool", None));
        assert!(r.is_none());
    }

    // -- neo4j_query: no-db path ----------------------------------------------

    #[tokio::test]
    async fn neo4j_query_without_db_returns_error() {
        let msg = result_error(
            skill_no_db()
                .execute(
                    "neo4j_query",
                    Some(serde_json::json!({"cypher": "MATCH (n) RETURN n"})),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("not available"));
    }

    // -- neo4j_query: readonly guard ------------------------------------------

    #[tokio::test]
    async fn neo4j_query_readonly_blocks_create() {
        let msg = result_error(
            skill_no_db()
                .execute(
                    "neo4j_query",
                    Some(serde_json::json!({
                        "cypher": "CREATE (n:Test {id: '1'})",
                        "readonly": true
                    })),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("CREATE"));
        assert!(msg.contains("readonly"));
    }

    #[tokio::test]
    async fn neo4j_query_readonly_blocks_merge() {
        let msg = result_error(
            skill_no_db()
                .execute(
                    "neo4j_query",
                    Some(serde_json::json!({"cypher": "MERGE (n:Foo)"})),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("MERGE"));
    }

    #[tokio::test]
    async fn neo4j_query_readonly_blocks_delete() {
        let msg = result_error(
            skill_no_db()
                .execute(
                    "neo4j_query",
                    Some(serde_json::json!({"cypher": "MATCH (n) DELETE n"})),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("DELETE"));
    }

    #[tokio::test]
    async fn neo4j_query_readonly_blocks_set() {
        let msg = result_error(
            skill_no_db()
                .execute(
                    "neo4j_query",
                    Some(serde_json::json!({"cypher": "MATCH (n) SET n.x = 1"})),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("SET"));
    }

    #[tokio::test]
    async fn neo4j_query_readonly_allows_match_return() {
        // Should pass guard (blocked by no-db, not by readonly), error is "not available"
        let msg = result_error(
            skill_no_db()
                .execute(
                    "neo4j_query",
                    Some(serde_json::json!({"cypher": "MATCH (n) RETURN n"})),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("not available"));
    }

    // -- duckdb_query: no-db path ---------------------------------------------

    #[tokio::test]
    async fn duckdb_query_without_telemetry_returns_error() {
        let msg = result_error(
            skill_no_db()
                .execute("duckdb_query", Some(serde_json::json!({"sql": "SELECT 1"})))
                .await
                .unwrap(),
        );
        assert!(msg.contains("not available"));
    }

    // -- missing required fields ----------------------------------------------

    #[tokio::test]
    async fn neo4j_query_missing_cypher_returns_error() {
        let r = skill_no_db()
            .execute("neo4j_query", Some(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
    }
}
